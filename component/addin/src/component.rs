use crate::contact_identity::{normalize_identity, NormalizedIdentity};
use crate::worker::{RuntimeConfig, SharedState, WorkerAction, WorkerHandle};
use native_api_1c::native_api_1c_core::ffi::connection::Connection;
use native_api_1c::native_api_1c_macro::AddIn;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const MAX_JSON_PAYLOAD: usize = 1024 * 1024;
const MAX_SCRIPT_PAYLOAD: usize = 1024 * 1024;

fn url_encode(input: &str) -> String {
    let mut encoded = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn normalize_company_id(company_id: &str) -> Result<String, &'static str> {
    let value = company_id.trim();
    if value.is_empty() {
        return Err("companyId must not be empty");
    }
    if value.len() > 100 {
        return Err("companyId exceeds 100 characters");
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("companyId contains disallowed characters");
    }
    Ok(value.to_string())
}

fn parse_json_payload(json: &str) -> Result<serde_json::Value, String> {
    if json.len() > MAX_JSON_PAYLOAD {
        return Err("JSON payload exceeds 1 MiB".to_string());
    }
    serde_json::from_str(json).map_err(|error| format!("Invalid JSON payload: {error}"))
}

fn parse_chat_menu_actions(json: &str) -> Result<serde_json::Value, String> {
    let value = parse_json_payload(json)?;
    let actions = value
        .as_array()
        .ok_or_else(|| "Chat menu actions must be a JSON array".to_string())?;
    let supported_ids = [
        "view_contact",
        "create_contact",
        "link_contact",
        "create_opportunity",
    ];
    let mut normalized = Vec::with_capacity(actions.len());
    let mut added_ids = Vec::with_capacity(actions.len());

    for action in actions {
        let object = action
            .as_object()
            .ok_or_else(|| "Each chat menu action must be a JSON object".to_string())?;
        let id = object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Each chat menu action must contain a string id".to_string())?;
        if !supported_ids.contains(&id) {
            return Err(format!("Unsupported chat menu action id: {id}"));
        }
        if added_ids.contains(&id) {
            return Err(format!("Duplicate chat menu action id: {id}"));
        }

        let mut normalized_action = serde_json::Map::new();
        normalized_action.insert("id".to_string(), serde_json::Value::String(id.to_string()));
        for property in ["label", "description"] {
            if let Some(value) = object.get(property) {
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("Chat menu action {property} must be a string"))?;
                normalized_action.insert(
                    property.to_string(),
                    serde_json::Value::String(text.to_string()),
                );
            }
        }
        normalized.push(serde_json::Value::Object(normalized_action));
        added_ids.push(id);
    }

    Ok(serde_json::Value::Array(normalized))
}

#[derive(AddIn)]
pub struct EasyBusyChatAddIn {
    #[add_in_done]
    lifecycle_done: fn(&mut Self),
    instance_id: u64,
    #[add_in_con]
    connection: Arc<Option<&'static Connection>>,

    #[add_in_prop(ty = Str, name = "Version", name_ru = "Версия", readable)]
    pub version: String,

    #[add_in_prop(ty = Bool, name = "IsConnected", name_ru = "Подключено", readable, getter = "Self::connected_value")]
    pub is_connected: bool,

    #[add_in_prop(ty = Str, name = "LastError", name_ru = "ПоследняяОшибка", readable, getter = "Self::error_value")]
    pub last_error: String,

    #[add_in_prop(
        ty = Str,
        name = "LastOperationId",
        name_ru = "ИдентификаторПоследнейОперации",
        readable
    )]
    pub last_operation_id: String,

    #[add_in_func(name = "Init", name_ru = "Инициализация")]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[returns(ty = Bool, result)]
    pub init_component: fn(&mut Self, String, String, String) -> Result<bool, ()>,

    #[add_in_func(name = "Open", name_ru = "Открыть")]
    #[returns(ty = Bool, result)]
    pub open: fn(&mut Self) -> Result<bool, ()>,

    #[add_in_func(name = "BeginOpen", name_ru = "НачатьОткрытие")]
    #[returns(ty = Str)]
    pub begin_open: fn(&mut Self) -> String,

    #[add_in_func(name = "Close", name_ru = "Закрыть")]
    pub close: fn(&mut Self),

    #[add_in_func(name = "BeginClose", name_ru = "НачатьЗакрытие")]
    #[returns(ty = Str)]
    pub begin_close: fn(&mut Self) -> String,

    #[add_in_func(name = "Navigate", name_ru = "Навигация")]
    #[arg(ty = Str)]
    pub navigate: fn(&mut Self, String),

    #[add_in_func(name = "BeginNavigate", name_ru = "НачатьНавигацию")]
    #[arg(ty = Str)]
    #[returns(ty = Str)]
    pub begin_navigate: fn(&mut Self, String) -> String,

    #[add_in_func(name = "PostMessage", name_ru = "ОтправитьСообщение")]
    #[arg(ty = Str)]
    pub post_message: fn(&mut Self, String),

    #[add_in_func(name = "BeginPostMessage", name_ru = "НачатьОтправкуСообщения")]
    #[arg(ty = Str)]
    #[returns(ty = Str)]
    pub begin_post_message: fn(&mut Self, String) -> String,

    #[add_in_func(name = "ExecuteScript", name_ru = "ВыполнитьСкрипт")]
    #[arg(ty = Str)]
    pub execute_script: fn(&mut Self, String),

    #[add_in_func(name = "BeginExecuteScript", name_ru = "НачатьВыполнениеСкрипта")]
    #[arg(ty = Str)]
    #[returns(ty = Str)]
    pub begin_execute_script: fn(&mut Self, String) -> String,

    #[add_in_func(name = "Resize", name_ru = "ИзменитьРазмер")]
    #[arg(ty = Int)]
    #[arg(ty = Int)]
    #[arg(ty = Int)]
    #[arg(ty = Int)]
    pub resize: fn(&mut Self, i32, i32, i32, i32),

    #[add_in_func(name = "BeginResize", name_ru = "НачатьИзменениеРазмера")]
    #[arg(ty = Int)]
    #[arg(ty = Int)]
    #[arg(ty = Int)]
    #[arg(ty = Int)]
    #[returns(ty = Str)]
    pub begin_resize: fn(&mut Self, i32, i32, i32, i32) -> String,

    #[add_in_func(name = "Ping", name_ru = "Пинг")]
    #[returns(ty = Str)]
    pub ping: fn(&Self) -> String,

    #[add_in_func(name = "GetLastEvent", name_ru = "ПолучитьПоследнееСобытие")]
    #[returns(ty = Str)]
    pub get_last_event: fn(&mut Self) -> String,

    #[add_in_func(name = "GetOperationStatus", name_ru = "ПолучитьСтатусОперации")]
    #[arg(ty = Str)]
    #[returns(ty = Str)]
    pub get_operation_status: fn(&mut Self, String) -> String,

    #[add_in_func(name = "CancelOperation", name_ru = "ОтменитьОперацию")]
    #[arg(ty = Str)]
    #[returns(ty = Bool)]
    pub cancel_operation: fn(&mut Self, String) -> bool,

    #[add_in_func(name = "GetConnectionState", name_ru = "ПолучитьСостояниеПодключения")]
    #[returns(ty = Str)]
    pub get_connection_state: fn(&mut Self) -> String,

    #[add_in_func(
        name = "GetDroppedEventCount",
        name_ru = "ПолучитьЧислоПотерянныхСобытий"
    )]
    #[returns(ty = Str)]
    pub get_dropped_event_count: fn(&Self) -> String,

    #[add_in_func(name = "SetPushEventsEnabled", name_ru = "УстановитьPushСобытия")]
    #[arg(ty = Bool)]
    pub set_push_events_enabled: fn(&mut Self, bool),

    #[add_in_func(name = "SetChatMenuActions", name_ru = "УстановитьМенюДействийЧата")]
    #[arg(ty = Str)]
    #[returns(ty = Bool, result)]
    pub set_chat_menu_actions: fn(&mut Self, String) -> Result<bool, ()>,

    #[add_in_func(name = "ShowTestNotification", name_ru = "ПоказатьТестовоеУведомление")]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    pub show_test_notification: fn(&mut Self, String, String),

    #[add_in_func(
        name = "BeginShowTestNotification",
        name_ru = "НачатьПоказТестовогоУведомления"
    )]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[returns(ty = Str)]
    pub begin_show_test_notification: fn(&mut Self, String, String) -> String,

    #[add_in_func(name = "MuteNotifications", name_ru = "ОтключитьУведомления")]
    #[arg(ty = Bool)]
    pub mute_notifications: fn(&mut Self, bool),

    #[add_in_func(
        name = "BeginMuteNotifications",
        name_ru = "НачатьИзменениеУведомлений"
    )]
    #[arg(ty = Bool)]
    #[returns(ty = Str)]
    pub begin_mute_notifications: fn(&mut Self, bool) -> String,

    #[add_in_func(name = "OpenChat", name_ru = "ОткрытьЧат")]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[returns(ty = Bool, result)]
    pub open_chat: fn(&mut Self, String, String, String) -> Result<bool, ()>,

    #[add_in_func(name = "BeginOpenChat", name_ru = "НачатьОткрытиеЧата")]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[returns(ty = Str)]
    pub begin_open_chat: fn(&mut Self, String, String, String) -> String,

    #[add_in_func(name = "EnsureContact", name_ru = "ПолучитьИлиСоздатьКонтакт")]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str, default = "")]
    #[returns(ty = Bool, result)]
    pub ensure_contact: fn(&mut Self, String, String, String, String) -> Result<bool, ()>,

    #[add_in_func(
        name = "BeginEnsureContact",
        name_ru = "НачатьПолучениеИлиСозданиеКонтакта"
    )]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str, default = "")]
    #[returns(ty = Str)]
    pub begin_ensure_contact: fn(&mut Self, String, String, String, String) -> String,

    #[add_in_func(name = "OpenChatByIdentity", name_ru = "ОткрытьЧатПоКонтакту")]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str, default = "")]
    #[arg(ty = Int, default = 0)]
    #[returns(ty = Bool, result)]
    #[allow(clippy::type_complexity)]
    pub open_chat_by_identity:
        fn(&mut Self, String, String, String, String, i32) -> Result<bool, ()>,

    #[add_in_func(
        name = "BeginOpenChatByIdentity",
        name_ru = "НачатьОткрытиеЧатаПоКонтакту"
    )]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str)]
    #[arg(ty = Str, default = "")]
    #[arg(ty = Int, default = 0)]
    #[returns(ty = Str)]
    pub begin_open_chat_by_identity: fn(&mut Self, String, String, String, String, i32) -> String,

    #[add_in_func(name = "EnableLog", name_ru = "ВключитьЛог")]
    #[arg(ty = Str, default = "")]
    #[returns(ty = Bool, result)]
    pub enable_log: fn(&mut Self, String) -> Result<bool, ()>,

    #[add_in_func(name = "DisableLog", name_ru = "ОтключитьЛог")]
    pub disable_log: fn(&mut Self),

    initialized: bool,
    shared: Arc<SharedState>,
    worker: Option<WorkerHandle>,
}

impl Default for EasyBusyChatAddIn {
    fn default() -> Self {
        crate::logger::install();
        let shared = Arc::new(SharedState::new());
        let worker = WorkerHandle::start(shared.clone()).ok();
        let last_error = if worker.is_some() {
            String::new()
        } else {
            "Failed to start command worker".to_string()
        };
        shared.set_public_error(&last_error);

        static NEXT_INSTANCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            lifecycle_done: Self::shutdown_component,
            instance_id: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            connection: Arc::new(None),
            version: env!("CARGO_PKG_VERSION").to_string(),
            is_connected: false,
            last_error,
            last_operation_id: String::new(),
            init_component: Self::init_component_inner,
            open: Self::open_inner,
            begin_open: Self::begin_open_inner,
            close: Self::close_inner,
            begin_close: Self::begin_close_inner,
            navigate: Self::navigate_inner,
            begin_navigate: Self::begin_navigate_inner,
            post_message: Self::post_message_inner,
            begin_post_message: Self::begin_post_message_inner,
            execute_script: Self::execute_script_inner,
            begin_execute_script: Self::begin_execute_script_inner,
            resize: Self::resize_inner,
            begin_resize: Self::begin_resize_inner,
            ping: Self::ping_inner,
            get_last_event: Self::get_last_event_inner,
            get_operation_status: Self::get_operation_status_inner,
            cancel_operation: Self::cancel_operation_inner,
            get_connection_state: Self::get_connection_state_inner,
            get_dropped_event_count: Self::get_dropped_event_count_inner,
            set_push_events_enabled: Self::set_push_events_enabled_inner,
            set_chat_menu_actions: Self::set_chat_menu_actions_inner,
            show_test_notification: Self::show_test_notification_inner,
            begin_show_test_notification: Self::begin_show_test_notification_inner,
            mute_notifications: Self::mute_notifications_inner,
            begin_mute_notifications: Self::begin_mute_notifications_inner,
            open_chat: Self::open_chat_inner,
            begin_open_chat: Self::begin_open_chat_inner,
            ensure_contact: Self::ensure_contact_inner,
            begin_ensure_contact: Self::begin_ensure_contact_inner,
            open_chat_by_identity: Self::open_chat_by_identity_inner,
            begin_open_chat_by_identity: Self::begin_open_chat_by_identity_inner,
            enable_log: Self::enable_log_inner,
            disable_log: Self::disable_log_inner,
            initialized: false,
            shared,
            worker,
        }
    }
}

impl EasyBusyChatAddIn {
    fn connected_value(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }
    fn error_value(&self) -> String {
        self.shared.last_error.read().clone()
    }
    fn shutdown_component(&mut self) {
        self.shared.set_push_events_enabled(false);
        if let Some(mut worker) = self.worker.take() {
            worker.shutdown();
        }
        self.connection = Arc::new(None);
        self.initialized = false;
    }

    fn init_component_inner(
        &mut self,
        api_base_url: String,
        company_id: String,
        access_token: String,
    ) -> Result<bool, ()> {
        let company_id = match normalize_company_id(&company_id) {
            Ok(value) => value,
            Err(error) => return Ok(self.reject_local("init", "invalid_company_id", error)),
        };
        let api_base_url = api_base_url.trim().to_string();
        if api_base_url.trim_end_matches('/') != "https://api.easy-busy.cloud" {
            return Ok(self.reject_local(
                "init",
                "invalid_api_base_url",
                "This frontend supports only https://api.easy-busy.cloud",
            ));
        }
        if access_token.is_empty() || access_token.len() > MAX_JSON_PAYLOAD {
            return Ok(self.reject_local(
                "init",
                "invalid_access_token",
                "accessToken must contain 1 byte..1 MiB",
            ));
        }

        let pipe_name = format!(
            r"\\.\pipe\EasyBusyChat-{}-{}-{}",
            company_id,
            std::process::id(),
            self.instance_id
        );
        let accepted = self.enqueue(
            "configure",
            WorkerAction::Configure(RuntimeConfig {
                api_base_url,
                company_id,
                access_token,
                pipe_name,
                connection: self.connection.clone(),
                chat_menu_actions: None,
            }),
        );
        self.initialized = accepted;
        Ok(accepted)
    }

    fn open_inner(&mut self) -> Result<bool, ()> {
        let operation_id = self.begin_open_inner();
        Ok(self.operation_was_queued(&operation_id))
    }

    fn begin_open_inner(&mut self) -> String {
        if !self.initialized {
            self.reject_local("open", "not_initialized", "Call Init before Open");
            return self.last_operation_id.clone();
        }
        self.enqueue_id("open", WorkerAction::Open)
    }

    fn close_inner(&mut self) {
        self.begin_close_inner();
    }

    fn begin_close_inner(&mut self) -> String {
        self.enqueue_id("close", WorkerAction::Close)
    }

    fn set_chat_menu_actions_inner(&mut self, json: String) -> Result<bool, ()> {
        if !self.initialized {
            return Ok(self.reject_local(
                "setChatMenuActions",
                "not_initialized",
                "Call Init before SetChatMenuActions",
            ));
        }
        let actions = match parse_chat_menu_actions(&json) {
            Ok(value) => value,
            Err(error) => {
                return Ok(self.reject_local(
                    "setChatMenuActions",
                    "invalid_chat_menu_actions",
                    &error,
                ));
            }
        };
        Ok(self.enqueue(
            "setChatMenuActions",
            WorkerAction::SetChatMenuActions(actions),
        ))
    }

    fn navigate_inner(&mut self, hash_or_url: String) {
        self.begin_navigate_inner(hash_or_url);
    }

    fn begin_navigate_inner(&mut self, hash_or_url: String) -> String {
        if hash_or_url.is_empty() || hash_or_url.len() > 4096 {
            self.reject_local(
                "navigate",
                "invalid_navigation",
                "Navigation value must contain 1..4096 characters",
            );
            return self.last_operation_id.clone();
        }
        self.enqueue_id(
            "navigate",
            WorkerAction::Send {
                action: "navigate",
                payload: Some(serde_json::json!({ "hash": hash_or_url })),
            },
        )
    }

    fn post_message_inner(&mut self, json: String) {
        self.begin_post_message_inner(json);
    }

    fn begin_post_message_inner(&mut self, json: String) -> String {
        let payload = match parse_json_payload(&json) {
            Ok(payload) => payload,
            Err(error) => {
                self.reject_local("postMessage", "invalid_json", &error);
                return self.last_operation_id.clone();
            }
        };
        self.enqueue_id(
            "postMessage",
            WorkerAction::Send {
                action: "postMessage",
                payload: Some(payload),
            },
        )
    }

    fn execute_script_inner(&mut self, script: String) {
        self.begin_execute_script_inner(script);
    }

    fn begin_execute_script_inner(&mut self, script: String) -> String {
        if script.is_empty() || script.len() > MAX_SCRIPT_PAYLOAD {
            self.reject_local(
                "executeScript",
                "invalid_script",
                "Script must contain 1 byte..1 MiB",
            );
            return self.last_operation_id.clone();
        }
        self.enqueue_id(
            "executeScript",
            WorkerAction::Send {
                action: "executeScript",
                payload: Some(serde_json::json!({ "script": script })),
            },
        )
    }

    fn resize_inner(&mut self, left: i32, top: i32, width: i32, height: i32) {
        self.begin_resize_inner(left, top, width, height);
    }

    fn begin_resize_inner(&mut self, left: i32, top: i32, width: i32, height: i32) -> String {
        if width <= 0 || height <= 0 {
            self.reject_local(
                "resize",
                "invalid_size",
                "Resize width and height must be positive",
            );
            return self.last_operation_id.clone();
        }
        self.enqueue_id(
            "resize",
            WorkerAction::Send {
                action: "resize",
                payload: Some(serde_json::json!({
                    "left": left,
                    "top": top,
                    "width": width,
                    "height": height,
                })),
            },
        )
    }

    fn ping_inner(&self) -> String {
        "pong".to_string()
    }

    fn get_last_event_inner(&mut self) -> String {
        self.refresh_legacy_snapshot();
        self.shared.events.pop().unwrap_or_default()
    }

    fn get_operation_status_inner(&mut self, operation_id: String) -> String {
        self.refresh_legacy_snapshot();
        self.shared.operations.status_json(operation_id.trim())
    }

    fn cancel_operation_inner(&mut self, operation_id: String) -> bool {
        let cancelled = self.shared.cancel_operation(operation_id.trim());
        self.refresh_legacy_snapshot();
        cancelled
    }

    fn get_connection_state_inner(&mut self) -> String {
        self.refresh_legacy_snapshot();
        self.shared.connection_state_json()
    }

    fn get_dropped_event_count_inner(&self) -> String {
        self.shared.events.dropped_count().to_string()
    }

    fn set_push_events_enabled_inner(&mut self, enabled: bool) {
        self.shared.set_push_events_enabled(enabled);
        self.refresh_legacy_snapshot();
    }

    fn show_test_notification_inner(&mut self, title: String, text: String) {
        self.begin_show_test_notification_inner(title, text);
    }

    fn begin_show_test_notification_inner(&mut self, title: String, text: String) -> String {
        self.enqueue_id(
            "showNotification",
            WorkerAction::Send {
                action: "showNotification",
                payload: Some(serde_json::json!({ "title": title, "text": text })),
            },
        )
    }

    fn mute_notifications_inner(&mut self, mute: bool) {
        self.begin_mute_notifications_inner(mute);
    }

    fn begin_mute_notifications_inner(&mut self, mute: bool) -> String {
        self.enqueue_id(
            "mute",
            WorkerAction::Send {
                action: "mute",
                payload: Some(serde_json::json!({ "mute": mute })),
            },
        )
    }

    fn open_chat_inner(
        &mut self,
        account_id: String,
        provider: String,
        chat_id: String,
    ) -> Result<bool, ()> {
        let operation_id = self.begin_open_chat_inner(account_id, provider, chat_id);
        Ok(self.operation_was_queued(&operation_id))
    }

    fn begin_open_chat_inner(
        &mut self,
        account_id: String,
        provider: String,
        chat_id: String,
    ) -> String {
        if !self.initialized {
            self.reject_local("openChat", "not_initialized", "Call Init before OpenChat");
            return self.last_operation_id.clone();
        }
        if account_id.trim().is_empty() || provider.trim().is_empty() || chat_id.trim().is_empty() {
            self.reject_local(
                "openChat",
                "invalid_chat_selection",
                "accountId, provider and chatId must not be empty",
            );
            return self.last_operation_id.clone();
        }
        let hash = format!(
            "#provider={}&accountid={}&chatid={}",
            url_encode(&provider),
            url_encode(&account_id),
            url_encode(&chat_id),
        );
        self.enqueue_id(
            "openChat",
            WorkerAction::OpenChat {
                account_id,
                provider,
                chat_id,
                hash,
            },
        )
    }

    fn ensure_contact_inner(
        &mut self,
        provider: String,
        account_id: String,
        identity: String,
        contact_name: String,
    ) -> Result<bool, ()> {
        let operation_id =
            self.begin_ensure_contact_inner(provider, account_id, identity, contact_name);
        Ok(self.operation_was_queued(&operation_id))
    }

    fn begin_ensure_contact_inner(
        &mut self,
        provider: String,
        account_id: String,
        identity: String,
        contact_name: String,
    ) -> String {
        let Some((provider, account_id, identity, contact_name)) = self.validate_identity_request(
            "ensureContact",
            provider,
            account_id,
            identity,
            contact_name,
        ) else {
            return self.last_operation_id.clone();
        };

        self.enqueue_id(
            "ensureContact",
            WorkerAction::EnsureContact {
                provider,
                account_id,
                identity,
                contact_name,
            },
        )
    }

    fn open_chat_by_identity_inner(
        &mut self,
        provider: String,
        account_id: String,
        identity: String,
        contact_name: String,
        chat_type: i32,
    ) -> Result<bool, ()> {
        let operation_id = self.begin_open_chat_by_identity_inner(
            provider,
            account_id,
            identity,
            contact_name,
            chat_type,
        );
        Ok(self.operation_was_queued(&operation_id))
    }

    fn begin_open_chat_by_identity_inner(
        &mut self,
        provider: String,
        account_id: String,
        identity: String,
        contact_name: String,
        chat_type: i32,
    ) -> String {
        if chat_type != 0 {
            self.reject_local(
                "openChatByIdentity",
                "invalid_chat_type",
                "Only user chatType=0 is supported for phone/username identity",
            );
            return self.last_operation_id.clone();
        }
        let Some((provider, account_id, identity, contact_name)) = self.validate_identity_request(
            "openChatByIdentity",
            provider,
            account_id,
            identity,
            contact_name,
        ) else {
            return self.last_operation_id.clone();
        };

        self.enqueue_id(
            "openChatByIdentity",
            WorkerAction::OpenChatByIdentity {
                provider,
                account_id,
                identity,
                contact_name,
                chat_type,
            },
        )
    }

    fn validate_identity_request(
        &mut self,
        action: &str,
        provider: String,
        account_id: String,
        identity: String,
        contact_name: String,
    ) -> Option<(String, String, NormalizedIdentity, String)> {
        if !self.initialized {
            self.reject_local(action, "not_initialized", "Call Init before this operation");
            return None;
        }

        let provider = provider.trim().to_string();
        if provider.is_empty()
            || provider.len() > 64
            || !provider.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            self.reject_local(
                action,
                "invalid_provider",
                "provider must contain 1..64 safe ASCII characters",
            );
            return None;
        }

        let account_id = account_id.trim().to_string();
        if account_id.is_empty()
            || account_id.len() > 100
            || !account_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            self.reject_local(
                action,
                "invalid_account_id",
                "accountId must contain 1..100 safe ASCII characters",
            );
            return None;
        }

        let identity = match normalize_identity(&identity) {
            Ok(identity) => identity,
            Err(error) => {
                self.reject_local(action, error.code, error.message);
                return None;
            }
        };

        let contact_name = contact_name.trim().to_string();
        if contact_name.len() > 250 || contact_name.chars().any(char::is_control) {
            self.reject_local(
                action,
                "api_validation_failed",
                "contactName must not exceed 250 characters or contain control characters",
            );
            return None;
        }

        Some((provider, account_id, identity, contact_name))
    }

    fn enable_log_inner(&mut self, log_file: String) -> Result<bool, ()> {
        let operation_id = self.enqueue_id("enableLog", WorkerAction::EnableLog(log_file));
        Ok(self.operation_was_queued(&operation_id))
    }

    fn disable_log_inner(&mut self) {
        self.enqueue_id("disableLog", WorkerAction::DisableLog);
    }

    fn enqueue_id(&mut self, action_name: &str, action: WorkerAction) -> String {
        self.enqueue(action_name, action);
        self.last_operation_id.clone()
    }

    fn enqueue(&mut self, action_name: &str, action: WorkerAction) -> bool {
        let Some(worker) = self.worker.as_ref() else {
            return self.reject_local(
                action_name,
                "worker_stopped",
                "Command worker is not running",
            );
        };
        match worker.enqueue(action_name, action) {
            Ok(operation_id) => {
                self.last_operation_id = operation_id;
                self.last_error.clear();
                self.refresh_legacy_snapshot();
                true
            }
            Err(error) => {
                self.last_operation_id = error.operation_id;
                self.last_error = error.code.to_string();
                self.refresh_legacy_snapshot();
                false
            }
        }
    }

    fn reject_local(&mut self, action: &str, code: &str, message: &str) -> bool {
        let operation_id = self
            .shared
            .operations
            .create(action, self.shared.generation.load(Ordering::Acquire));
        self.shared.operations.fail(&operation_id, code, message);
        self.shared.publish_operation_snapshot(&operation_id);
        self.shared.set_public_error(message);
        self.last_operation_id = operation_id;
        self.last_error = message.to_string();
        false
    }

    fn operation_was_queued(&self, operation_id: &str) -> bool {
        self.shared
            .operations
            .snapshot(operation_id)
            .is_some_and(|snapshot| {
                matches!(
                    snapshot.state,
                    crate::operations::OperationState::Queued
                        | crate::operations::OperationState::Running
                        | crate::operations::OperationState::Completed
                )
            })
    }

    fn refresh_legacy_snapshot(&mut self) {
        self.is_connected = self.shared.connected.load(Ordering::Acquire);
        self.last_error = self.shared.last_error.read().clone();
    }
}

impl Drop for EasyBusyChatAddIn {
    fn drop(&mut self) {
        self.shutdown_component();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_company_id, parse_chat_menu_actions, parse_json_payload, EasyBusyChatAddIn,
    };

    #[test]
    fn company_id_validation_is_strict_instead_of_silently_rewriting() {
        assert_eq!(
            normalize_company_id("company-42_OK").unwrap(),
            "company-42_OK"
        );
        assert!(normalize_company_id("company/42").is_err());
        assert!(normalize_company_id(" ").is_err());
        assert!(normalize_company_id(&"a".repeat(101)).is_err());
    }

    #[test]
    fn unsupported_chat_type_is_rejected_before_queueing() {
        let mut component = EasyBusyChatAddIn::default();
        component.initialized = true;
        let operation_id = component.begin_open_chat_by_identity_inner(
            "telegram".to_string(),
            "account-1".to_string(),
            "+380501234567".to_string(),
            "Ivan".to_string(),
            2,
        );
        let snapshot = component.shared.operations.snapshot(&operation_id).unwrap();
        assert_eq!(snapshot.code, "invalid_chat_type");
        assert_eq!(snapshot.state, crate::operations::OperationState::Failed);
    }

    #[test]
    fn post_message_json_is_validated_before_enqueue() {
        assert_eq!(parse_json_payload(r#"{"ok":true}"#).unwrap()["ok"], true);
        assert!(parse_json_payload("not-json").is_err());
    }

    #[test]
    fn chat_menu_actions_accept_only_supported_unique_ids_and_string_metadata() {
        let actions = parse_chat_menu_actions(
            r#"[
                {"id":"view_contact"},
                {"id":"create_opportunity","label":"Створити замовлення","description":"З картки чату"}
            ]"#,
        )
        .unwrap();

        assert_eq!(actions.as_array().unwrap().len(), 2);
        assert_eq!(actions[1]["label"], "Створити замовлення");
        assert_eq!(actions[1]["description"], "З картки чату");
        assert!(parse_chat_menu_actions(r#"{"id":"view_contact"}"#).is_err());
        assert!(parse_chat_menu_actions(r#"[{"id":"unknown"}]"#).is_err());
        assert!(
            parse_chat_menu_actions(r#"[{"id":"view_contact"},{"id":"view_contact"}]"#).is_err()
        );
        assert!(parse_chat_menu_actions(r#"[{"id":"view_contact","label":7}]"#).is_err());
    }

    #[test]
    fn instances_have_distinct_pipe_identity_and_live_properties() {
        let first = EasyBusyChatAddIn::default();
        let second = EasyBusyChatAddIn::default();
        assert_ne!(first.instance_id, second.instance_id);
        first
            .shared
            .connected
            .store(true, std::sync::atomic::Ordering::Release);
        first.shared.set_public_error("background failure");
        assert!(first.connected_value());
        assert_eq!(first.error_value(), "background failure");
    }

    #[test]
    fn done_joins_worker_and_prevents_further_commands() {
        use native_api_1c::native_api_1c_core::interface::AddInWrapper;
        let mut component = EasyBusyChatAddIn::default();
        component.done();
        assert!(component.worker.is_none());
        assert!(!component.initialized);
        assert!(!component.open_inner().unwrap());
        component.done();
    }

    #[test]
    fn unsupported_api_url_is_rejected_before_configure() {
        let mut component = EasyBusyChatAddIn::default();
        assert!(!component
            .init_component_inner(
                "https://example.invalid".into(),
                "company".into(),
                "synthetic".into()
            )
            .unwrap());
        assert_eq!(
            component
                .shared
                .operations
                .snapshot(&component.last_operation_id)
                .unwrap()
                .code,
            "invalid_api_base_url"
        );
    }
}
