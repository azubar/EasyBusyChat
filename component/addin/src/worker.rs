use crate::contact_identity::NormalizedIdentity;
use crate::event_queue::EventQueue;
use crate::ipc::{PipeClient, SafeHandle};
use crate::operations::OperationStore;
use native_api_1c::native_api_1c_core::ffi::connection::Connection;
use parking_lot::RwLock;
use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const COMMAND_QUEUE_CAPACITY: usize = 256;
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const API_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct RuntimeConfig {
    pub api_base_url: String,
    pub company_id: String,
    pub access_token: String,
    pub pipe_name: String,
    pub connection: Arc<Option<&'static Connection>>,
    pub chat_menu_actions: Option<Value>,
}

pub enum WorkerAction {
    Configure(RuntimeConfig),
    SetChatMenuActions(Value),
    Open,
    OpenChat {
        account_id: String,
        provider: String,
        chat_id: String,
        hash: String,
    },
    EnsureContact {
        provider: String,
        account_id: String,
        identity: NormalizedIdentity,
        contact_name: String,
    },
    OpenChatByIdentity {
        provider: String,
        account_id: String,
        identity: NormalizedIdentity,
        contact_name: String,
        chat_type: i32,
    },
    Send {
        action: &'static str,
        payload: Option<Value>,
    },
    Close,
    EnableLog(String),
    DisableLog,
    #[cfg(test)]
    Noop,
}

struct WorkerCommand {
    operation_id: String,
    action: WorkerAction,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadinessSnapshot {
    pub process_started: bool,
    pub pipe_connected: bool,
    pub webview_ready: bool,
    pub react_ready: bool,
    pub chat_opened: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSelection {
    pub generation: u64,
    pub provider: String,
    pub account_id: String,
    pub chat_id: String,
}

impl ChatSelection {
    pub fn matches(
        &self,
        generation: u64,
        provider: &str,
        account_id: &str,
        chat_id: &str,
    ) -> bool {
        self.generation == generation
            && self.provider.eq_ignore_ascii_case(provider)
            && self.account_id == account_id
            && self.chat_id == chat_id
    }
}

pub struct SharedState {
    pub connected: Arc<AtomicBool>,
    pub generation: AtomicU64,
    pub readiness: RwLock<ReadinessSnapshot>,
    pub operations: OperationStore,
    pub events: Arc<EventQueue>,
    pub last_error: RwLock<String>,
    pub push_events_enabled: Arc<AtomicBool>,
    pub external_events_ready: Arc<AtomicBool>,
    pub webview_ready: Arc<AtomicBool>,
    pub browser_ready: Arc<AtomicBool>,
    pub chat_selection: Arc<RwLock<Option<ChatSelection>>>,
    shutdown_requested: AtomicBool,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            connected: Arc::new(AtomicBool::new(false)),
            generation: AtomicU64::new(0),
            readiness: RwLock::new(ReadinessSnapshot::default()),
            operations: OperationStore::new(),
            events: Arc::new(EventQueue::new()),
            last_error: RwLock::new(String::new()),
            push_events_enabled: Arc::new(AtomicBool::new(false)),
            external_events_ready: Arc::new(AtomicBool::new(false)),
            webview_ready: Arc::new(AtomicBool::new(false)),
            browser_ready: Arc::new(AtomicBool::new(false)),
            chat_selection: Arc::new(RwLock::new(None)),
            shutdown_requested: AtomicBool::new(false),
        }
    }

    pub fn set_push_events_enabled(&self, enabled: bool) {
        self.push_events_enabled.store(enabled, Ordering::Release);
        self.external_events_ready.store(enabled, Ordering::Release);
    }

    pub fn connection_state_json(&self) -> String {
        let mut readiness = self.readiness.read().clone();
        readiness.pipe_connected = self.connected.load(Ordering::Acquire);
        readiness.webview_ready = self.webview_ready.load(Ordering::Acquire);
        readiness.react_ready = self.browser_ready.load(Ordering::Acquire);
        readiness.chat_opened = self.chat_selection.read().is_some();
        serde_json::json!({
            "generation": self.generation.load(Ordering::Acquire),
            "connected": readiness.pipe_connected,
            "readiness": readiness,
            "pushEventsEnabled": self.push_events_enabled.load(Ordering::Acquire),
            "droppedEvents": self.events.dropped_count(),
            "lastError": self.last_error.read().clone(),
        })
        .to_string()
    }

    pub fn cancel_operation(&self, operation_id: &str) -> bool {
        let cancelled = self.operations.cancel(operation_id);
        if cancelled {
            self.publish_operation(operation_id);
        }
        cancelled
    }

    pub fn publish_operation_snapshot(&self, operation_id: &str) {
        self.publish_operation(operation_id);
    }

    pub fn set_public_error(&self, message: &str) {
        self.set_error(message);
    }

    fn publish_operation(&self, operation_id: &str) {
        if let Some(event) = self.operations.event_json(operation_id) {
            self.events.push(event);
        }
    }

    fn set_error(&self, message: impl Into<String>) {
        *self.last_error.write() = message.into();
    }

    fn clear_error(&self) {
        self.last_error.write().clear();
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct EnqueueError {
    pub code: &'static str,
    pub operation_id: String,
}

pub struct WorkerHandle {
    sender: mpsc::SyncSender<WorkerCommand>,
    pub shared: Arc<SharedState>,
    thread: Option<JoinHandle<()>>,
    done: Option<mpsc::Receiver<()>>,
}

impl WorkerHandle {
    pub fn start(shared: Arc<SharedState>) -> anyhow::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let (done_sender, done_receiver) = mpsc::sync_channel(1);
        let worker_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("easybusy-command-worker".to_string())
            .spawn(move || {
                let mut runtime = WorkerRuntime::new();
                worker_loop(receiver, &worker_shared, &mut runtime);
                runtime.shutdown(&worker_shared);
                let _ = done_sender.try_send(());
            })?;

        Ok(Self {
            sender,
            shared,
            thread: Some(thread),
            done: Some(done_receiver),
        })
    }

    pub fn enqueue(&self, action_name: &str, action: WorkerAction) -> Result<String, EnqueueError> {
        let generation = self.shared.generation.load(Ordering::Acquire);
        let operation_id = self.shared.operations.create(action_name, generation);
        let command = WorkerCommand {
            operation_id: operation_id.clone(),
            action,
        };

        match self.sender.try_send(command) {
            Ok(()) => {
                self.shared.publish_operation(&operation_id);
                Ok(operation_id)
            }
            Err(mpsc::TrySendError::Full(_)) => {
                self.reject(&operation_id, "queue_full", "Command queue is full");
                Err(EnqueueError {
                    code: "queue_full",
                    operation_id,
                })
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.reject(
                    &operation_id,
                    "worker_stopped",
                    "Command worker is not running",
                );
                Err(EnqueueError {
                    code: "worker_stopped",
                    operation_id,
                })
            }
        }
    }

    pub fn shutdown(&mut self) {
        self.shared.set_push_events_enabled(false);
        self.shared
            .shutdown_requested
            .store(true, Ordering::Release);
        // Never detach DLL code or a callback holding a borrowed host interface.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.done.take();
    }

    fn reject(&self, operation_id: &str, code: &'static str, message: &str) {
        self.shared.operations.fail(operation_id, code, message);
        self.shared.set_error(message);
        self.shared.publish_operation(operation_id);
    }

    #[cfg(test)]
    fn for_test(capacity: usize) -> (Self, mpsc::Receiver<WorkerCommand>) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        (
            Self {
                sender,
                shared: Arc::new(SharedState::new()),
                thread: None,
                done: None,
            },
            receiver,
        )
    }
}

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

struct WorkerRuntime {
    config: Option<RuntimeConfig>,
    child_pid: Option<u32>,
    child_handle: Option<SafeHandle>,
    pipe_client: Option<PipeClient>,
    muted: bool,
}

impl WorkerRuntime {
    fn new() -> Self {
        Self {
            config: None,
            child_pid: None,
            child_handle: None,
            pipe_client: None,
            muted: false,
        }
    }

    fn execute(
        &mut self,
        operation_id: &str,
        action: WorkerAction,
        shared: &Arc<SharedState>,
    ) -> OperationResult {
        match action {
            WorkerAction::Configure(config) => {
                if let Some(connection) = config.connection.as_ref() {
                    let _ = connection.set_event_buffer_depth(1000);
                }
                let reopen = self.pipe_client.is_some();
                self.shutdown(shared);
                self.config = Some(config);
                if reopen {
                    self.open_ui(operation_id, shared)
                } else {
                    OperationResult::completed("configured", "Configuration applied")
                }
            }
            WorkerAction::SetChatMenuActions(actions) => {
                let Some(config) = self.config.as_mut() else {
                    return OperationResult::failed(
                        "not_initialized",
                        "Call Init before SetChatMenuActions",
                    );
                };
                config.chat_menu_actions = Some(actions.clone());
                if self
                    .pipe_client
                    .as_ref()
                    .is_some_and(PipeClient::is_connected)
                {
                    self.send(
                        operation_id,
                        shared,
                        "setChatMenuActions",
                        Some(serde_json::json!({ "actions": actions })),
                    )
                } else {
                    OperationResult::completed(
                        "chat_menu_actions_configured",
                        "Chat menu actions will be sent with initial configuration",
                    )
                }
            }
            WorkerAction::Open => self.open_ui(operation_id, shared),
            WorkerAction::OpenChat {
                account_id,
                provider,
                chat_id,
                hash,
            } => self.open_chat(
                operation_id,
                shared,
                &account_id,
                &provider,
                &chat_id,
                &hash,
            ),
            WorkerAction::EnsureContact {
                provider,
                account_id,
                identity,
                contact_name,
            } => self.ensure_contact(
                operation_id,
                shared,
                &provider,
                &account_id,
                &identity,
                &contact_name,
            ),
            WorkerAction::OpenChatByIdentity {
                provider,
                account_id,
                identity,
                contact_name,
                chat_type,
            } => self.open_chat_by_identity(
                operation_id,
                shared,
                &provider,
                &account_id,
                &identity,
                &contact_name,
                chat_type,
            ),
            WorkerAction::Send { action, payload } => {
                if action == "mute" {
                    self.muted = payload
                        .as_ref()
                        .and_then(|value| value.get("mute"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                }
                self.send(operation_id, shared, action, payload)
            }
            WorkerAction::Close => {
                self.shutdown(shared);
                shared.events.push(
                    serde_json::json!({
                        "event": "shutdownCompleted",
                        "payload": {
                            "generation": shared.generation.load(Ordering::Acquire),
                        }
                    })
                    .to_string(),
                );
                OperationResult::completed("shutdown_completed", "UI shutdown completed")
            }
            WorkerAction::EnableLog(path) => match crate::logger::enable(Some(&path)) {
                Ok(path) => {
                    if let Some(config) = self.config.as_ref().filter(|_| {
                        self.pipe_client
                            .as_ref()
                            .is_some_and(PipeClient::is_connected)
                    }) {
                        let ui_path = crate::process::ui_log_path(&path, &config.pipe_name);
                        self.send(
                            operation_id,
                            shared,
                            "setLogging",
                            Some(serde_json::json!({"enabled":true,"path":ui_path})),
                        )
                    } else {
                        OperationResult::completed("logging_enabled", "Logging enabled")
                    }
                }
                Err(error) => OperationResult::failed("log_enable_failed", error.to_string()),
            },
            WorkerAction::DisableLog => {
                let result = if self
                    .pipe_client
                    .as_ref()
                    .is_some_and(PipeClient::is_connected)
                {
                    self.send(
                        operation_id,
                        shared,
                        "setLogging",
                        Some(serde_json::json!({"enabled":false})),
                    )
                } else {
                    OperationResult::completed("logging_disabled", "Logging disabled")
                };
                crate::logger::disable();
                result
            }
            #[cfg(test)]
            WorkerAction::Noop => OperationResult::completed("ok", ""),
        }
    }

    fn open_ui(&mut self, operation_id: &str, shared: &Arc<SharedState>) -> OperationResult {
        let Some(config) = self.config.clone() else {
            return OperationResult::failed("not_initialized", "Call Init before Open");
        };

        if self.ensure_connected(shared) {
            return self.send(operation_id, shared, "open", None);
        }

        self.shutdown(shared);
        let generation = shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        shared.operations.set_generation(operation_id, generation);
        shared.publish_operation(operation_id);
        shared.browser_ready.store(false, Ordering::Release);
        shared.webview_ready.store(false, Ordering::Release);
        *shared.chat_selection.write() = None;
        *shared.readiness.write() = ReadinessSnapshot::default();

        let exe_path = match crate::extractor::ExeExtractor::extract_if_needed() {
            Ok(path) => path,
            Err(error) => {
                return OperationResult::failed("ui_extract_failed", error.to_string());
            }
        };
        let ui_log_path = crate::logger::current_path();
        let profile = easy_busy_chat_ipc_protocol::auth_profile::AuthProfile::for_context(
            &config.api_base_url,
            &config.company_id,
            &config.access_token,
        );
        let (pid, handle) = match crate::process::launch_ui(
            &exe_path,
            &config.pipe_name,
            &profile,
            ui_log_path.as_deref(),
        ) {
            Ok(process) => process,
            Err(error) => {
                return OperationResult::failed("ui_launch_failed", error.to_string());
            }
        };
        self.child_pid = Some(pid);
        self.child_handle = Some(handle);
        shared.readiness.write().process_started = true;

        let started = Instant::now();
        while started.elapsed() < OPEN_TIMEOUT {
            if shared.shutdown_requested.load(Ordering::Acquire)
                || shared.operations.is_cancelled(operation_id)
            {
                self.shutdown(shared);
                return OperationResult::cancelled();
            }
            if !crate::process::is_process_alive(handle.0) {
                let exit_code = crate::process::get_process_exit_code(handle.0).unwrap_or(0);
                self.shutdown(shared);
                return OperationResult::failed(
                    "ui_exited",
                    format!("UI exited before pipe connection, code {exit_code}"),
                );
            }

            match PipeClient::connect(
                &config.pipe_name,
                config.connection.clone(),
                shared.events.clone(),
                shared.push_events_enabled.clone(),
                shared.external_events_ready.clone(),
                shared.webview_ready.clone(),
                shared.browser_ready.clone(),
                shared.chat_selection.clone(),
                shared.connected.clone(),
                250,
            ) {
                Ok(client) => {
                    self.pipe_client = Some(client);
                    shared.connected.store(true, Ordering::Release);
                    shared.readiness.write().pipe_connected = true;
                    if let Err(error) = self.send_config_and_state(&config, shared) {
                        self.shutdown(shared);
                        return OperationResult::failed("config_send_failed", error);
                    }
                    return self.send(operation_id, shared, "open", None);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }

        self.shutdown(shared);
        OperationResult::timed_out("pipe_connect_timeout", "UI pipe connection timed out")
    }

    fn open_chat(
        &mut self,
        operation_id: &str,
        shared: &Arc<SharedState>,
        account_id: &str,
        provider: &str,
        chat_id: &str,
        hash: &str,
    ) -> OperationResult {
        let opened = self.open_ui(operation_id, shared);
        if !opened.is_completed() {
            return opened;
        }

        let started = Instant::now();
        while !shared.browser_ready.load(Ordering::Acquire) {
            if shared.shutdown_requested.load(Ordering::Acquire)
                || shared.operations.is_cancelled(operation_id)
            {
                return OperationResult::cancelled();
            }
            if started.elapsed() >= READY_TIMEOUT {
                return OperationResult::timed_out(
                    "react_ready_timeout",
                    "React chat readiness timed out",
                );
            }
            if !self
                .pipe_client
                .as_ref()
                .is_some_and(PipeClient::is_connected)
            {
                return OperationResult::failed("ui_disconnected", "UI pipe disconnected");
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        *shared.chat_selection.write() = None;
        let result = self.send(
            operation_id,
            shared,
            "navigate",
            Some(serde_json::json!({ "hash": hash })),
        );
        if !result.is_completed() {
            return result;
        }

        let generation = shared.generation.load(Ordering::Acquire);
        let started = Instant::now();
        loop {
            if shared.shutdown_requested.load(Ordering::Acquire)
                || shared.operations.is_cancelled(operation_id)
            {
                return OperationResult::cancelled();
            }
            if shared
                .chat_selection
                .read()
                .as_ref()
                .is_some_and(|selection| {
                    selection.matches(generation, provider, account_id, chat_id)
                })
            {
                return OperationResult::completed(
                    "chat_opened",
                    format!("Chat opened: {provider}/{account_id}/{chat_id}"),
                );
            }
            if started.elapsed() >= READY_TIMEOUT {
                return OperationResult::timed_out(
                    "chat_open_timeout",
                    "Matching chatOpened event timed out",
                );
            }
            if !self
                .pipe_client
                .as_ref()
                .is_some_and(PipeClient::is_connected)
            {
                return OperationResult::failed("ui_disconnected", "UI pipe disconnected");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn ensure_contact(
        &mut self,
        operation_id: &str,
        shared: &Arc<SharedState>,
        provider: &str,
        account_id: &str,
        identity: &NormalizedIdentity,
        contact_name: &str,
    ) -> OperationResult {
        let ready = self.ensure_react_ready(operation_id, shared);
        if !ready.is_completed() {
            return ready;
        }

        self.execute_identity_command(
            operation_id,
            shared,
            "ensureContact",
            serde_json::json!({
                "provider": provider,
                "accountId": account_id,
                "identity": identity,
                "contactName": contact_name,
                "chatType": 0,
            }),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open_chat_by_identity(
        &mut self,
        operation_id: &str,
        shared: &Arc<SharedState>,
        provider: &str,
        account_id: &str,
        identity: &NormalizedIdentity,
        contact_name: &str,
        chat_type: i32,
    ) -> OperationResult {
        let ready = self.ensure_react_ready(operation_id, shared);
        if !ready.is_completed() {
            return ready;
        }

        let resolved = self.execute_identity_command(
            operation_id,
            shared,
            "openChatByIdentity",
            serde_json::json!({
                "provider": provider,
                "accountId": account_id,
                "identity": identity,
                "contactName": contact_name,
                "chatType": chat_type,
            }),
        );
        if !resolved.is_completed() {
            return resolved;
        }

        let mut result = resolved.result.unwrap_or_else(|| serde_json::json!({}));
        let Some(chat_id) = result
            .get("chat")
            .and_then(|chat| chat.get("id"))
            .and_then(json_scalar_to_string)
        else {
            return OperationResult::failed_with_result(
                "api_invalid_response",
                "Resolved chat does not contain an id",
                Some(mark_opened(result, false)),
            );
        };

        let hash = format!(
            "#provider={}&accountid={}&chatid={}",
            url_encode(provider),
            url_encode(account_id),
            url_encode(&chat_id),
        );
        let opened = self.open_chat(operation_id, shared, account_id, provider, &chat_id, &hash);
        result = mark_opened(result, opened.is_completed());

        match opened.state {
            ResultState::Completed => {
                OperationResult::completed_with_result("chat_opened", "Chat opened", Some(result))
            }
            ResultState::Cancelled => OperationResult::cancelled_with_result(Some(result)),
            ResultState::TimedOut => OperationResult::timed_out_with_result(
                "chat_open_timeout",
                opened.message,
                Some(result),
            ),
            ResultState::Failed => OperationResult::failed_with_result(
                "chat_open_failed",
                opened.message,
                Some(result),
            ),
        }
    }

    fn ensure_react_ready(
        &mut self,
        operation_id: &str,
        shared: &Arc<SharedState>,
    ) -> OperationResult {
        let opened = self.open_ui(operation_id, shared);
        if !opened.is_completed() {
            return opened;
        }

        let started = Instant::now();
        while !shared.browser_ready.load(Ordering::Acquire) {
            if shared.shutdown_requested.load(Ordering::Acquire)
                || shared.operations.is_cancelled(operation_id)
            {
                return OperationResult::cancelled();
            }
            if started.elapsed() >= READY_TIMEOUT {
                return OperationResult::timed_out(
                    "react_ready_timeout",
                    "React chat readiness timed out",
                );
            }
            if !self
                .pipe_client
                .as_ref()
                .is_some_and(PipeClient::is_connected)
            {
                return OperationResult::failed("ui_disconnected", "UI pipe disconnected");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        OperationResult::completed("react_ready", "React is ready")
    }

    fn execute_identity_command(
        &mut self,
        operation_id: &str,
        shared: &Arc<SharedState>,
        action: &'static str,
        payload: Value,
    ) -> OperationResult {
        let Some(client) = self.pipe_client.as_ref() else {
            return OperationResult::failed("ui_not_connected", "UI pipe is not connected");
        };
        if let Err(error) = client.send_correlated_command(action, operation_id, Some(payload)) {
            return OperationResult::failed("pipe_write_failed", error.to_string());
        }

        let started = Instant::now();
        loop {
            if shared.shutdown_requested.load(Ordering::Acquire)
                || shared.operations.is_cancelled(operation_id)
            {
                let _ = client.send_command(
                    "cancelOperation",
                    Some(serde_json::json!({"operationId":operation_id})),
                );
                return OperationResult::cancelled_with_result(Some(
                    serde_json::json!({"outcomeUnknown":true,"reconcileBeforeRetry":true}),
                ));
            }
            if let Some(response) = shared.events.take_command_result(operation_id) {
                return operation_result_from_command_response(response);
            }
            if started.elapsed() >= API_COMMAND_TIMEOUT {
                let _ = client.send_command(
                    "cancelOperation",
                    Some(serde_json::json!({"operationId":operation_id})),
                );
                return OperationResult::timed_out_with_result(
                    "api_timeout",
                    "Easy Busy API command timed out",
                    Some(serde_json::json!({"outcomeUnknown":true,"reconcileBeforeRetry":true})),
                );
            }
            if !self
                .pipe_client
                .as_ref()
                .is_some_and(PipeClient::is_connected)
            {
                return OperationResult::failed("ui_disconnected", "UI pipe disconnected");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn ensure_connected(&mut self, shared: &Arc<SharedState>) -> bool {
        let connected = self
            .pipe_client
            .as_ref()
            .is_some_and(PipeClient::is_connected);
        shared.connected.store(connected, Ordering::Release);
        shared.readiness.write().pipe_connected = connected;
        connected
    }

    fn send_config_and_state(
        &self,
        config: &RuntimeConfig,
        shared: &SharedState,
    ) -> Result<(), String> {
        let Some(client) = self.pipe_client.as_ref() else {
            return Err("UI pipe is not connected".to_string());
        };
        client
            .send_command(
                "setConfig",
                Some(initial_config_payload(
                    config,
                    shared.generation.load(Ordering::Acquire),
                )),
            )
            .map_err(|error| error.to_string())?;
        if self.muted {
            client
                .send_command("mute", Some(serde_json::json!({ "mute": true })))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn send(
        &mut self,
        operation_id: &str,
        shared: &Arc<SharedState>,
        action: &'static str,
        payload: Option<Value>,
    ) -> OperationResult {
        let Some(client) = self.pipe_client.as_ref() else {
            return OperationResult::failed("ui_not_connected", "UI pipe is not connected");
        };
        if let Err(error) = client.send_correlated_command(action, operation_id, payload) {
            return OperationResult::failed("pipe_write_failed", error.to_string());
        }
        let started = Instant::now();
        loop {
            if let Some(response) = shared.events.take_command_result(operation_id) {
                return operation_result_from_command_response(response);
            }
            if shared.shutdown_requested.load(Ordering::Acquire)
                || shared.operations.is_cancelled(operation_id)
            {
                return OperationResult::cancelled_with_result(Some(
                    serde_json::json!({"outcomeUnknown":true}),
                ));
            }
            if !client.is_connected() {
                return OperationResult::failed(
                    "ui_disconnected",
                    "UI pipe disconnected before acknowledgement",
                );
            }
            if started.elapsed() >= READY_TIMEOUT {
                return OperationResult::timed_out(
                    "command_ack_timeout",
                    "UI did not acknowledge the command",
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn shutdown(&mut self, shared: &Arc<SharedState>) {
        if let Some(client) = self.pipe_client.as_ref() {
            let _ = client.send_command("quit", None);
        }
        self.pipe_client = None;
        if let Some(handle) = self.child_handle.take() {
            crate::process::terminate_ui(handle.0);
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle.0);
            }
        }
        self.child_pid = None;
        shared.connected.store(false, Ordering::Release);
        shared.browser_ready.store(false, Ordering::Release);
        shared.webview_ready.store(false, Ordering::Release);
        *shared.chat_selection.write() = None;
        *shared.readiness.write() = ReadinessSnapshot::default();
    }
}

fn initial_config_payload(config: &RuntimeConfig, generation: u64) -> Value {
    serde_json::json!({
        "apiBaseUrl": config.api_base_url,
        "companyId": config.company_id,
        "accessToken": config.access_token,
        "generation": generation,
        "chatMenuActions": config.chat_menu_actions,
    })
}

enum ResultState {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

struct OperationResult {
    state: ResultState,
    code: String,
    message: String,
    result: Option<Value>,
}

impl OperationResult {
    fn completed(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::completed_with_result(code, message, None)
    }

    fn completed_with_result(
        code: impl Into<String>,
        message: impl Into<String>,
        result: Option<Value>,
    ) -> Self {
        Self {
            state: ResultState::Completed,
            code: code.into(),
            message: message.into(),
            result,
        }
    }

    fn failed(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::failed_with_result(code, message, None)
    }

    fn failed_with_result(
        code: impl Into<String>,
        message: impl Into<String>,
        result: Option<Value>,
    ) -> Self {
        Self {
            state: ResultState::Failed,
            code: code.into(),
            message: message.into(),
            result,
        }
    }

    fn cancelled() -> Self {
        Self::cancelled_with_result(None)
    }

    fn cancelled_with_result(result: Option<Value>) -> Self {
        Self {
            state: ResultState::Cancelled,
            code: "cancelled".to_string(),
            message: "Operation cancelled".to_string(),
            result,
        }
    }

    fn timed_out(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::timed_out_with_result(code, message, None)
    }

    fn timed_out_with_result(
        code: impl Into<String>,
        message: impl Into<String>,
        result: Option<Value>,
    ) -> Self {
        Self {
            state: ResultState::TimedOut,
            code: code.into(),
            message: message.into(),
            result,
        }
    }

    fn is_completed(&self) -> bool {
        matches!(self.state, ResultState::Completed)
    }
}

fn operation_result_from_command_response(response: Value) -> OperationResult {
    let success = response
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let code = response
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or(if success {
            "command_completed"
        } else {
            "api_invalid_response"
        })
        .to_string();
    let message = response
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(if success {
            "Command completed"
        } else {
            "Easy Busy command failed"
        })
        .to_string();
    let result = response.get("result").cloned();

    if success {
        OperationResult::completed_with_result(code, message, result)
    } else if code == "api_cancelled" {
        OperationResult::cancelled_with_result(result)
    } else if code == "api_timeout" {
        OperationResult::timed_out_with_result(code, message, result)
    } else {
        OperationResult::failed_with_result(code, message, result)
    }
}

fn json_scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn mark_opened(mut result: Value, opened: bool) -> Value {
    if !result.is_object() {
        result = serde_json::json!({ "resolved": result });
    }
    if let Some(object) = result.as_object_mut() {
        object.insert("opened".to_string(), Value::Bool(opened));
    }
    result
}

fn worker_loop(
    receiver: mpsc::Receiver<WorkerCommand>,
    shared: &Arc<SharedState>,
    runtime: &mut WorkerRuntime,
) {
    loop {
        if shared.shutdown_requested.load(Ordering::Acquire) {
            break;
        }
        let command = match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if shared.operations.is_cancelled(&command.operation_id) {
            shared.publish_operation(&command.operation_id);
            continue;
        }
        if !shared.operations.mark_running(&command.operation_id) {
            continue;
        }
        shared.publish_operation(&command.operation_id);

        let result = runtime.execute(&command.operation_id, command.action, shared);
        let changed = match result.state {
            ResultState::Completed => shared.operations.complete_with_result(
                &command.operation_id,
                &result.code,
                &result.message,
                result.result.clone(),
            ),
            ResultState::Failed => {
                shared.set_error(&result.message);
                shared.operations.fail_with_result(
                    &command.operation_id,
                    &result.code,
                    &result.message,
                    result.result.clone(),
                )
            }
            ResultState::Cancelled => {
                shared.operations.cancel(&command.operation_id);
                shared
                    .operations
                    .set_cancelled_result(&command.operation_id, result.result.clone())
            }
            ResultState::TimedOut => {
                shared.set_error(&result.message);
                shared.operations.time_out_with_result(
                    &command.operation_id,
                    &result.code,
                    &result.message,
                    result.result.clone(),
                )
            }
        };
        if changed {
            if matches!(result.state, ResultState::Completed) {
                shared.clear_error();
            }
            shared.publish_operation(&command.operation_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn enqueue_rejects_immediately_when_bounded_queue_is_full() {
        let (handle, _receiver) = WorkerHandle::for_test(1);

        assert!(handle.enqueue("noop", WorkerAction::Noop).is_ok());
        let error = handle
            .enqueue("noop", WorkerAction::Noop)
            .expect_err("second command must be rejected");

        assert_eq!(error.code, "queue_full");
    }

    #[test]
    fn pull_is_default_and_push_requires_explicit_opt_in() {
        let shared = SharedState::new();

        assert!(!shared.push_events_enabled.load(Ordering::Relaxed));
        shared.set_push_events_enabled(true);
        assert!(shared.push_events_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn rejected_enqueue_finishes_operation_with_failure_event() {
        let (handle, _receiver) = WorkerHandle::for_test(1);
        let _first = handle.enqueue("noop", WorkerAction::Noop).unwrap();

        let error = handle.enqueue("navigate", WorkerAction::Noop).unwrap_err();
        let status: serde_json::Value =
            serde_json::from_str(&handle.shared.operations.status_json(&error.operation_id))
                .unwrap();

        assert_eq!(status["state"], "failed");
        assert_eq!(status["code"], "queue_full");
        assert!(handle.shared.events.has_events());
    }

    #[test]
    fn command_result_maps_success_and_partial_failure() {
        let success = operation_result_from_command_response(serde_json::json!({
            "operationId": "op-1",
            "success": true,
            "code": "chat_resolved",
            "result": { "chat": { "id": 77 } }
        }));
        assert!(success.is_completed());
        assert_eq!(success.result.unwrap()["chat"]["id"], 77);

        let failure = operation_result_from_command_response(serde_json::json!({
            "operationId": "op-2",
            "success": false,
            "code": "api_validation_failed",
            "message": "invalid",
            "result": { "contact": { "id": "42" } }
        }));
        assert!(matches!(failure.state, ResultState::Failed));
        assert_eq!(failure.result.unwrap()["contact"]["id"], "42");
    }

    #[test]
    fn initial_config_payload_contains_chat_menu_actions() {
        let actions = serde_json::json!([{"id": "view_contact"}]);
        let config = RuntimeConfig {
            api_base_url: "https://api.easy-busy.cloud".to_string(),
            company_id: "company-1".to_string(),
            access_token: "secret".to_string(),
            pipe_name: r"\\.\pipe\EasyBusyChat-test".to_string(),
            connection: Arc::new(None),
            chat_menu_actions: Some(actions.clone()),
        };

        let payload = initial_config_payload(&config, 3);

        assert_eq!(payload["generation"], 3);
        assert_eq!(payload["chatMenuActions"], actions);
    }

    #[test]
    fn chat_selection_requires_matching_generation_provider_account_and_chat() {
        let selection = ChatSelection {
            generation: 7,
            provider: "TELEGRAM".to_string(),
            account_id: "account-1".to_string(),
            chat_id: "chat-9".to_string(),
        };

        assert!(selection.matches(7, "telegram", "account-1", "chat-9"));
        assert!(!selection.matches(6, "telegram", "account-1", "chat-9"));
        assert!(!selection.matches(7, "telegram", "account-2", "chat-9"));
        assert!(!selection.matches(7, "telegram", "account-1", "chat-8"));
    }
}
