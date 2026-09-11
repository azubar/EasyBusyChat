use crate::easybusy_api::{EasyBusyApiClient, ResolveIdentityPayload};
use crate::{AppState, ChatConfig};
use serde_json::Value;
use tauri::command;
use tauri::Manager;

fn event_payload_from_webview_message(data: &Value) -> Value {
    let mut event_payload = data.get("payload").cloned().unwrap_or_else(|| {
        let mut legacy_payload = data.clone();
        if let Some(object) = legacy_payload.as_object_mut() {
            object.remove("event");
        }
        legacy_payload
    });

    if let (Some(req_id), Some(payload)) = (data.get("reqId"), event_payload.as_object_mut()) {
        payload.entry("reqId").or_insert_with(|| req_id.clone());
    }

    event_payload
}

/// Called from JS polyfill when template.html sends
/// a message via chrome.webview.postMessage()
#[command]
pub fn on_webview_message(state: tauri::State<AppState>, payload: String) {
    // Parse JSON from JS
    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&payload) {
        let event_type = data
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Event type whitelist for defense in depth
        let event_whitelist = [
            "browserReady",
            "webviewReady",
            "reactReady",
            "chatOpened",
            "chat_action",
            "notification",
            "hostClosed",
            "navigationCompleted",
            "error",
            "action_result",
            "console_log",
            "commandResult",
        ];

        if !event_whitelist.contains(&event_type.as_str()) {
            log::warn!("WebView sent disallowed event type: {}", event_type);
            return;
        }

        if event_type == "console_log" {
            // Never persist arbitrary console arguments (chat text, tokens, etc.).
            log::debug!("WebView console diagnostic received");
            return;
        }

        log::info!(
            "WebView event accepted: event={}, payload_len={}",
            event_type,
            payload.len()
        );

        // Forward event into Named Pipe -> DLL
        let event_payload = event_payload_from_webview_message(&data);
        let event = crate::ipc::IpcEvent {
            event: event_type.clone(),
            payload: Some(event_payload),
        };

        if let Some(ref server) = *state.pipe_server.lock().unwrap() {
            server.send_event(event);
        } else {
            log::warn!(
                "WebView event accepted but pipe server is not initialized: event={}",
                event_type
            );
        }
    } else {
        log::warn!(
            "Failed to parse WebView payload as JSON: len={}",
            payload.len()
        );
    }
}

#[command]
pub fn get_config(state: tauri::State<AppState>) -> Option<ChatConfig> {
    state.config.lock().unwrap().clone()
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveCommandRequest {
    pub action: String,
    pub operation_id: String,
    pub payload: ResolveIdentityPayload,
}

#[cfg(test)]
pub(crate) fn resolve_contact_chat_blocking(
    config: Option<ChatConfig>,
    request: ResolveCommandRequest,
) -> serde_json::Value {
    resolve_contact_chat_with_control(
        config,
        request,
        std::sync::Arc::new(crate::easybusy_api::OperationControl::new(
            std::time::Duration::from_secs(25),
        )),
    )
}

fn resolve_contact_chat_with_control(
    config: Option<ChatConfig>,
    request: ResolveCommandRequest,
    control: std::sync::Arc<crate::easybusy_api::OperationControl>,
) -> serde_json::Value {
    let operation_id = request.operation_id.clone();
    let action = request.action.clone();
    let payload = request.payload;

    let Some(config) = config else {
        return command_failure(
            &operation_id,
            "ui_not_connected",
            "Easy Busy configuration is not available",
            None,
        );
    };

    let resolved = match EasyBusyApiClient::new_with_control(config, control) {
        Ok(client) => match action.as_str() {
            "ensureContact" => client.ensure_contact(&payload),
            "openChatByIdentity" => client.resolve_chat(&payload),
            _ => Err(crate::easybusy_api::ApiFailure {
                code: "api_validation_failed",
                message: "Unsupported identity command".to_string(),
                partial_result: None,
            }),
        },
        Err(error) => Err(error),
    };

    match resolved {
        Ok(result) => serde_json::json!({
            "operationId": operation_id,
            "success": true,
            "code": if action == "ensureContact" { "contact_resolved" } else { "chat_resolved" },
            "message": if action == "ensureContact" { "Contact resolved" } else { "Chat resolved" },
            "result": result,
        }),
        Err(error) => command_failure(
            &operation_id,
            error.code,
            &error.message,
            error.partial_result,
        ),
    }
}

#[command]
pub async fn resolve_contact_chat(
    app_handle: tauri::AppHandle,
    request: ResolveCommandRequest,
) -> serde_json::Value {
    let state = app_handle.state::<AppState>();
    let control = match state
        .api_operations
        .lock()
        .unwrap()
        .register(&request.operation_id)
    {
        Ok(control) => control,
        Err(code) => {
            return command_failure(
                &request.operation_id,
                code,
                "API operation cannot start",
                None,
            )
        }
    };
    resolve_registered_contact_chat(app_handle, request, control).await
}

pub(crate) async fn resolve_registered_contact_chat(
    app_handle: tauri::AppHandle,
    request: ResolveCommandRequest,
    control: std::sync::Arc<crate::easybusy_api::OperationControl>,
) -> serde_json::Value {
    let config = app_handle
        .state::<AppState>()
        .config
        .lock()
        .unwrap()
        .clone();
    let operation_id = request.operation_id.clone();
    let response = match tauri::async_runtime::spawn_blocking(move || {
        resolve_contact_chat_with_control(config, request, control)
    })
    .await
    {
        Ok(response) => response,
        Err(_) => command_failure(
            &operation_id,
            "api_server_error",
            "Easy Busy API worker failed",
            Some(serde_json::json!({"outcomeUnknown":true})),
        ),
    };
    app_handle
        .state::<AppState>()
        .api_operations
        .lock()
        .unwrap()
        .finish(&operation_id);
    response
}

pub(crate) fn command_failure(
    operation_id: &str,
    code: &str,
    message: &str,
    result: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut response = serde_json::json!({
        "operationId": operation_id,
        "success": false,
        "code": code,
        "message": message,
    });
    if let Some(result) = result {
        response["result"] = result;
    }
    response
}

#[cfg(test)]
mod readiness_contract {
    use serde_json::json;

    const FRONTEND: &str = include_str!("../frontend/index.html");

    #[test]
    fn missing_config_returns_correlated_failure_without_network() {
        let request = serde_json::from_value(json!({
            "action":"ensureContact", "operationId":"unconfigured-operation",
            "payload":{"provider":"telegram","accountId":"account","identity":{"type":"phone","value":"+380501234567"}}
        })).unwrap();
        let response = super::resolve_contact_chat_blocking(None, request);
        assert_eq!(response["operationId"], "unconfigured-operation");
        assert_eq!(response["success"], false);
        assert_eq!(response["code"], "ui_not_connected");
    }

    #[test]
    fn chat_action_copies_outer_req_id_into_payload() {
        let message = json!({
            "event": "chat_action",
            "reqId": 42,
            "payload": {
                "actionId": "view_contact",
                "chatId": "chat-1"
            }
        });

        let payload = super::event_payload_from_webview_message(&message);

        assert_eq!(payload["actionId"], "view_contact");
        assert_eq!(payload["chatId"], "chat-1");
        assert_eq!(payload["reqId"], 42);
    }

    #[test]
    fn payload_req_id_is_not_overwritten() {
        let message = json!({
            "event": "chat_action",
            "reqId": 42,
            "payload": {
                "actionId": "view_contact",
                "reqId": 7
            }
        });

        let payload = super::event_payload_from_webview_message(&message);

        assert_eq!(payload["reqId"], 7);
    }

    #[test]
    fn frontend_bridges_typed_host_commands_and_command_results() {
        assert!(FRONTEND.contains("msg.event === 'host_command'"));
        assert!(FRONTEND.contains("resolve_contact_chat"));
        assert!(FRONTEND.contains("event: 'commandResult'"));
        assert!(FRONTEND.contains("operationId: msg.operationId"));
    }

    #[test]
    fn config_ready_only_announces_webview_readiness() {
        let start = FRONTEND.find("window.onConfigReady").unwrap();
        let end = FRONTEND[start..].find("window.hostNavigating").unwrap() + start;
        let block = &FRONTEND[start..end];

        assert!(block.contains("postHostEvent('webviewReady'"));
        assert!(!block.contains("postHostEvent('browserReady'"));
        assert!(!block.contains("postHostEvent('reactReady'"));
    }

    #[test]
    fn react_ready_is_emitted_after_render_call() {
        let render = FRONTEND
            .find("window.renderChatsApp('chat_container'")
            .unwrap();
        let ready = FRONTEND.find("postHostEvent('reactReady'").unwrap();

        assert!(ready > render);
        assert!(FRONTEND.contains("disableBrowserNotifications: chatSettings.muted"));
    }

    #[test]
    fn open_chat_callback_emits_matching_selection() {
        let start = FRONTEND.find("onOpenChat: async function").unwrap();
        let end = FRONTEND[start..].find("function showMessage").unwrap() + start;
        let block = &FRONTEND[start..end];

        assert!(block.contains("postHostEvent('chatOpened'"));
        assert!(block.contains("provider:"));
        assert!(block.contains("accountId:"));
        assert!(block.contains("chatId:"));
    }

    #[test]
    fn frontend_exposes_runtime_chat_action_settings() {
        assert!(FRONTEND.contains("var chatActionSettings ="));
        assert!(FRONTEND.contains("window.setChatMenuActions = function (actions)"));
        assert!(FRONTEND.contains("view_contact"));
        assert!(FRONTEND.contains("create_opportunity"));
    }

    #[test]
    fn open_chat_applies_current_action_settings() {
        let start = FRONTEND.find("onOpenChat: async function").unwrap();
        let end = FRONTEND[start..].find("function showMessage").unwrap() + start;
        let block = &FRONTEND[start..end];

        assert!(block.contains("applyChatActionSettings("));
        assert!(FRONTEND.contains("actionSetting.label"));
        assert!(FRONTEND.contains("actionSetting.description"));
    }

    #[test]
    fn configured_chat_actions_are_applied_before_first_render() {
        let start = FRONTEND.find("function loadConfigAndInit()").unwrap();
        let end = FRONTEND[start..].find("window.onConfigReady").unwrap() + start;
        let block = &FRONTEND[start..end];

        let apply = block
            .find("window.setChatMenuActions(config.chatMenuActions)")
            .expect("configured chat actions must be applied during config loading");
        let initialize = block
            .find("initializeChat()")
            .expect("config loading must initialize the chat");

        assert!(apply < initialize);
    }
}
