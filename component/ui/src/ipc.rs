use easy_busy_chat_ipc_protocol::{command_log_summary, FrameError, NdjsonDecoder};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SafeHandle(pub HANDLE);

unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

/// Command from DLL
#[derive(serde::Deserialize)]
pub struct IpcCommand {
    pub action: String,
    #[serde(rename = "operationId", default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Event to DLL
#[derive(serde::Serialize, Debug)]
pub struct IpcEvent {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

fn send_command_result(app_handle: &tauri::AppHandle, payload: serde_json::Value) {
    use tauri::Manager;
    let state = app_handle.state::<crate::AppState>();
    let pipe_server = state.pipe_server.lock().unwrap();
    if let Some(server) = pipe_server.as_ref() {
        server.send_event(IpcEvent {
            event: "commandResult".to_string(),
            payload: Some(payload),
        });
    } else {
        log::warn!("Cannot return commandResult because pipe server is unavailable");
    }
}

/// Helper to write all bytes to a synchronous Windows pipe handle.
fn pipe_write_all(handle: HANDLE, mut bytes: &[u8]) -> Result<(), &'static str> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !bytes.is_empty() {
        if std::time::Instant::now() >= deadline {
            return Err("Pipe write deadline exceeded");
        }
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len().min(4096) as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err("WriteFile returned 0");
        }
        if written == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn set_pipe_nonblocking(handle: HANDLE, context: &str) -> bool {
    let mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
    let ok = unsafe {
        SetNamedPipeHandleState(handle, &mode, std::ptr::null_mut(), std::ptr::null_mut())
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        log::warn!(
            "Failed to switch pipe handle to nonblocking mode: context={}, error={}",
            context,
            err
        );
        false
    } else {
        log::info!(
            "Pipe handle switched to nonblocking mode: context={}",
            context
        );
        true
    }
}

struct PipeConnection {
    handle: SafeHandle,
    failed: std::sync::atomic::AtomicBool,
}
impl PipeConnection {
    fn disconnect(&self) {
        if !self.failed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            unsafe {
                DisconnectNamedPipe(self.handle.0);
            }
        }
    }
}
impl Drop for PipeConnection {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle.0);
        }
    }
}

pub struct PipeServer {
    _active_handle: Arc<Mutex<Option<Arc<PipeConnection>>>>,
    event_tx: std::sync::mpsc::SyncSender<IpcEvent>,
}

impl PipeServer {
    pub fn start(pipe_name: String, app_handle: tauri::AppHandle) -> Self {
        let active_handle = Arc::new(Mutex::new(None));
        let active_handle_clone = active_handle.clone();

        let (tx, rx) = std::sync::mpsc::sync_channel::<IpcEvent>(256);

        std::thread::spawn(move || {
            Self::run_loop(pipe_name, app_handle, active_handle_clone);
        });

        let active_handle_writer = active_handle.clone();
        std::thread::spawn(move || {
            while let Ok(event) = rx.recv() {
                let event_name = event.event.clone();
                // Keep the connection alive throughout the write without holding the mutex.
                let handle_opt = {
                    let handle_lock = active_handle_writer.lock().unwrap();
                    handle_lock.clone()
                };

                if let Some(safe_handle) = handle_opt {
                    if safe_handle
                        .failed
                        .load(std::sync::atomic::Ordering::Acquire)
                    {
                        continue;
                    }
                    let handle = safe_handle.handle.0;
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    let data = format!("{}\n", json);
                    let bytes = data.as_bytes();
                    if bytes.len() > 1024 * 1024 {
                        log::warn!("Dropping oversized IPC event: event={}", event_name);
                        safe_handle.disconnect();
                        continue;
                    }
                    log::info!(
                        "Writing event to DLL pipe: event={}, bytes={}",
                        event_name,
                        bytes.len()
                    );
                    if let Err(e) = pipe_write_all(handle, bytes) {
                        safe_handle.disconnect();
                        log::error!("Pipe write_all failed for event {}: {}", event_name, e);
                    } else {
                        log::info!("Event written to DLL pipe: event={}", event_name);
                    }
                } else {
                    log::warn!(
                        "Cannot write event to DLL pipe because no client is connected: event={}",
                        event_name
                    );
                }
            }
        });

        Self {
            _active_handle: active_handle,
            event_tx: tx,
        }
    }

    fn run_loop(
        pipe_name: String,
        app_handle: tauri::AppHandle,
        active_handle: Arc<Mutex<Option<Arc<PipeConnection>>>>,
    ) {
        let pipe_name_w: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();

        // Secure Named Pipe: fail closed if security attributes cannot be initialized (Hypothesis 26)
        let user_sa = match UserSecurityAttributes::new() {
            Some(sa) => sa,
            None => {
                log::error!("Security violation: failed to create user security attributes. Named pipe server will not start.");
                return;
            }
        };
        let sa_ptr = user_sa.as_ptr();

        loop {
            let pipe_handle = unsafe {
                CreateNamedPipeW(
                    pipe_name_w.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    65536,
                    65536,
                    0,
                    sa_ptr,
                )
            };

            if pipe_handle == INVALID_HANDLE_VALUE {
                log::error!("CreateNamedPipeW failed");
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }

            log::info!("Named Pipe server listening on {}", pipe_name);

            let connected = unsafe { ConnectNamedPipe(pipe_handle, std::ptr::null_mut()) != 0 };
            let error = unsafe { GetLastError() };
            let is_connected = connected || (error == ERROR_PIPE_CONNECTED);

            if !is_connected {
                log::error!("ConnectNamedPipe failed, error={}", error);
                unsafe {
                    CloseHandle(pipe_handle);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }

            log::info!("Client connected to named pipe");
            if !set_pipe_nonblocking(pipe_handle, "server command reader") {
                unsafe {
                    DisconnectNamedPipe(pipe_handle);
                    CloseHandle(pipe_handle);
                }
                continue;
            }

            let connection = Arc::new(PipeConnection {
                handle: SafeHandle(pipe_handle),
                failed: std::sync::atomic::AtomicBool::new(false),
            });
            {
                let mut active = active_handle.lock().unwrap();
                *active = Some(connection.clone());
            }

            let mut buffer = vec![0u8; 65536];
            let mut decoder = NdjsonDecoder::new(1024 * 1024);

            loop {
                if connection.failed.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                let mut bytes_read = 0u32;
                let result = unsafe {
                    ReadFile(
                        pipe_handle,
                        buffer.as_mut_ptr(),
                        buffer.len() as u32,
                        &mut bytes_read,
                        std::ptr::null_mut(),
                    )
                };

                if result == 0 {
                    let error = unsafe { GetLastError() };
                    if error == ERROR_NO_DATA {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                        continue;
                    }
                    log::info!("Client disconnected from named pipe, read_error={}", error);
                    break;
                }

                if bytes_read == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }

                let frames = match decoder.push(&buffer[..bytes_read as usize]) {
                    Ok(frames) => frames,
                    Err(FrameError::TooLarge) => {
                        log::warn!("Command frame exceeds 1 MiB. Disconnecting client.");
                        break;
                    }
                    Err(FrameError::InvalidUtf8) => {
                        log::warn!("Command frame contains invalid UTF-8. Disconnecting client.");
                        break;
                    }
                };

                for line in frames {
                    if let Ok(cmd) = serde_json::from_str::<IpcCommand>(&line) {
                        Self::handle_command(cmd, &app_handle);
                    } else {
                        log::warn!("Failed to parse IPC command: bytes={}", line.len());
                    }
                }
            }

            {
                let mut active = active_handle.lock().unwrap();
                *active = None;
            }

            connection.disconnect();
        }
    }

    fn handle_command(cmd: IpcCommand, app_handle: &tauri::AppHandle) {
        use tauri::Manager;
        log::debug!(
            "Received command: {}",
            command_log_summary(&cmd.action, &cmd.payload)
        );
        if cmd.action == "quit" {
            app_handle.exit(0);
            return;
        }
        if cmd.action == "cancelOperation" {
            if let Some(id) = cmd
                .payload
                .get("operationId")
                .and_then(serde_json::Value::as_str)
            {
                app_handle
                    .state::<crate::AppState>()
                    .api_operations
                    .lock()
                    .unwrap()
                    .cancel(id);
            }
            return;
        }
        if matches!(cmd.action.as_str(), "ensureContact" | "openChatByIdentity") {
            let Some(id) = cmd
                .operation_id
                .as_ref()
                .filter(|id| !id.is_empty())
                .cloned()
            else {
                return;
            };
            let payload = match serde_json::from_value(cmd.payload) {
                Ok(value) => value,
                Err(_) => {
                    send_command_result(
                        app_handle,
                        crate::commands::command_failure(
                            &id,
                            "api_validation_failed",
                            "Invalid identity payload",
                            None,
                        ),
                    );
                    return;
                }
            };
            let request = crate::commands::ResolveCommandRequest {
                action: cmd.action,
                operation_id: id,
                payload,
            };
            // Register on the pipe reader before spawning, so the next cancel
            // frame always finds the control even if the executor is busy.
            let control = match app_handle
                .state::<crate::AppState>()
                .api_operations
                .lock()
                .unwrap()
                .register(&request.operation_id)
            {
                Ok(control) => control,
                Err(code) => {
                    send_command_result(
                        app_handle,
                        crate::commands::command_failure(
                            &request.operation_id,
                            code,
                            "API operation cannot start",
                            None,
                        ),
                    );
                    return;
                }
            };
            let app = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                let response =
                    crate::commands::resolve_registered_contact_chat(app.clone(), request, control)
                        .await;
                send_command_result(&app, response);
            });
            return;
        }
        let app = app_handle.clone();
        let id = cmd.operation_id.clone();
        let schedule = app_handle.run_on_main_thread(move || {
            let result = Self::apply_command(&cmd, &app);
            // JavaScript commands acknowledge themselves after the handler runs.
            if let Some(id) = cmd.operation_id.as_deref() {
                match result {
                    Ok(true) => send_command_result(&app, serde_json::json!({"operationId":id,"success":true,"code":"command_applied"})),
                    Err(error) => send_command_result(&app, crate::commands::command_failure(id, "command_failed", &error, None)),
                    Ok(false) => {},
                }
            }
        });
        if schedule.is_err() {
            if let Some(id) = id {
                send_command_result(
                    app_handle,
                    crate::commands::command_failure(
                        &id,
                        "ui_unavailable",
                        "Cannot schedule UI command",
                        None,
                    ),
                );
            }
        }
    }

    fn apply_command(cmd: &IpcCommand, app: &tauri::AppHandle) -> Result<bool, String> {
        use serde_json::Value;
        use tauri::Manager;
        let text = |key: &str| {
            cmd.payload
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("Missing string: {key}"))
        };
        if cmd.action == "setLogging" {
            if cmd.payload.get("enabled").and_then(Value::as_bool) == Some(true) {
                crate::logger::enable(Some(text("path")?)).map_err(|error| error.to_string())?;
            } else {
                crate::logger::disable();
            }
            return Ok(true);
        }
        let window = app
            .get_webview_window("main")
            .ok_or("Main window is unavailable")?;
        let body = match cmd.action.as_str() {
            "open" => {
                window.show().map_err(|e| e.to_string())?;
                window.set_focus().map_err(|e| e.to_string())?;
                return Ok(true);
            }
            "close" => {
                window.hide().map_err(|e| e.to_string())?;
                return Ok(true);
            }
            "resize" => {
                let number = |key| {
                    cmd.payload
                        .get(key)
                        .and_then(Value::as_f64)
                        .ok_or_else(|| format!("Missing number: {key}"))
                };
                window
                    .set_position(tauri::LogicalPosition::new(
                        number("left")?.clamp(-10000.0, 10000.0),
                        number("top")?.clamp(-10000.0, 10000.0),
                    ))
                    .map_err(|e| e.to_string())?;
                window
                    .set_size(tauri::LogicalSize::new(
                        number("width")?.clamp(100.0, 4096.0),
                        number("height")?.clamp(100.0, 4096.0),
                    ))
                    .map_err(|e| e.to_string())?;
                return Ok(true);
            }
            "setConfig" => {
                let config: crate::ChatConfig = serde_json::from_value(cmd.payload.clone())
                    .map_err(|_| "Invalid configuration")?;
                if !app.state::<crate::AppState>().auth_profile.accepts(
                    &config.api_base_url,
                    &config.company_id,
                    &config.access_token,
                ) {
                    return Err(
                        "Authentication context changed; restart UI with its own profile".into(),
                    );
                }
                *app.state::<crate::AppState>().config.lock().unwrap() = Some(config);
                "await window.onConfigReady();".to_string()
            }
            "navigate" => format!(
                "await window.navigateChat({});",
                serde_json::to_string(text("hash")?).unwrap()
            ),
            "postMessage" => format!("window.easybusychat.__dispatchMessage({});", cmd.payload),
            // Acknowledge synchronous evaluation, not arbitrary future side effects.
            "executeScript" => format!(
                "(0,eval)({});",
                serde_json::to_string(text("script")?).unwrap()
            ),
            "setChatMenuActions" => {
                let actions = cmd
                    .payload
                    .get("actions")
                    .filter(|v| v.is_array())
                    .ok_or("Missing actions array")?;
                if let Some(config) = app
                    .state::<crate::AppState>()
                    .config
                    .lock()
                    .unwrap()
                    .as_mut()
                {
                    config.chat_menu_actions = Some(actions.clone());
                }
                format!("if (await window.setChatMenuActions({actions}) === false) throw new Error('Invalid actions');")
            }
            "mute" => {
                let muted = cmd
                    .payload
                    .get("mute")
                    .and_then(Value::as_bool)
                    .ok_or("Missing mute boolean")?;
                format!("await window.setMutedState({muted});")
            }
            "showNotification" => format!(
                "window.showHtmlNotification({},{});",
                serde_json::to_string(text("title")?).unwrap(),
                serde_json::to_string(text("text")?).unwrap()
            ),
            _ => return Err("Unsupported command".into()),
        };
        let id = serde_json::to_string(&cmd.operation_id).unwrap();
        let script = format!(
            r#"(async()=>{{ const id={id}; try {{ {body}
            if(id) window.easybusychat.postMessage({{event:'commandResult',payload:{{operationId:id,success:true,code:'command_applied'}}}});
        }} catch(error) {{ if(id) window.easybusychat.postMessage({{event:'commandResult',payload:{{operationId:id,success:false,code:'command_failed',message:String(error && error.message || error)}}}}); }} }})();"#
        );
        window.eval(&script).map_err(|e| e.to_string())?;
        Ok(false)
    }

    pub fn send_event(&self, event: IpcEvent) {
        let event_name = event.event.clone();
        log::info!("Queueing event to DLL pipe: event={}", event_name);
        match self.event_tx.try_send(event) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                if let Some(connection) = self._active_handle.lock().unwrap().as_ref() {
                    connection.disconnect();
                }
                log::warn!(
                    "Dropping event because IPC queue is full: event={}",
                    event_name
                );
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                log::error!("IPC event writer is stopped: event={}", event_name);
            }
        }
    }
}

unsafe fn get_current_user_sid() -> Option<Vec<u32>> {
    let mut token: HANDLE = std::ptr::null_mut();
    if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
        return None;
    }

    let mut len = 0;
    GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
    if len == 0 {
        CloseHandle(token);
        return None;
    }

    let mut buf = vec![0u8; len as usize];
    if GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut _, len, &mut len) == 0 {
        CloseHandle(token);
        return None;
    }
    CloseHandle(token);

    let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
    let sid_len = GetLengthSid(token_user.User.Sid) as usize;
    let mut sid = vec![0u32; sid_len.div_ceil(4)];
    std::ptr::copy_nonoverlapping(
        token_user.User.Sid as *const u8,
        sid.as_mut_ptr() as *mut u8,
        sid_len,
    );
    Some(sid)
}

pub struct SecurityDescriptorHelper {
    _sd: Box<SECURITY_DESCRIPTOR>,
    _acl: Vec<u32>,
    _sid: Vec<u32>,
}

pub struct UserSecurityAttributes {
    sa: SECURITY_ATTRIBUTES,
    _helper: SecurityDescriptorHelper,
}

impl UserSecurityAttributes {
    pub fn new() -> Option<Self> {
        unsafe {
            let sid = get_current_user_sid()?;
            let sid_len = sid.len() * 4;

            let acl_size =
                std::mem::size_of::<ACL>() + std::mem::size_of::<ACCESS_ALLOWED_ACE>() + sid_len
                    - std::mem::size_of::<u32>();

            let mut acl = vec![0u32; acl_size.div_ceil(4)];
            let acl_ptr = acl.as_mut_ptr() as *mut ACL;

            if InitializeAcl(acl_ptr, acl_size as u32, ACL_REVISION) == 0 {
                return None;
            }

            let access_mask = GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE;
            let sid_buffer = sid;

            if AddAccessAllowedAce(
                acl_ptr,
                ACL_REVISION,
                access_mask,
                sid_buffer.as_ptr() as *const _ as *mut _,
            ) == 0
            {
                return None;
            }

            let mut sd = Box::new(std::mem::zeroed::<SECURITY_DESCRIPTOR>());
            if InitializeSecurityDescriptor(
                sd.as_mut() as *mut _ as *mut _,
                SECURITY_DESCRIPTOR_REVISION,
            ) == 0
            {
                return None;
            }

            if SetSecurityDescriptorDacl(sd.as_mut() as *mut _ as *mut _, 1, acl_ptr, 0) == 0 {
                return None;
            }

            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.as_mut() as *mut _ as *mut _,
                bInheritHandle: 0,
            };

            Some(Self {
                sa,
                _helper: SecurityDescriptorHelper {
                    _sd: sd,
                    _acl: acl,
                    _sid: sid_buffer,
                },
            })
        }
    }

    pub fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.sa
    }
}

#[cfg(test)]
mod direct_identity_command_tests {
    use super::IpcCommand;

    #[test]
    fn deserializes_direct_correlated_identity_command() {
        let command: IpcCommand = serde_json::from_value(serde_json::json!({
            "action": "ensureContact",
            "operationId": "op-1234-25",
            "payload": {
                "provider": "telegram",
                "accountId": "account-1",
                "identity": { "type": "username", "value": "ivan_petrenko" },
                "contactName": "Ivan Petrenko",
                "chatType": 0
            }
        }))
        .expect("valid direct command");

        assert_eq!(command.action, "ensureContact");
        assert_eq!(command.operation_id.as_deref(), Some("op-1234-25"));
        assert_eq!(command.payload["identity"]["type"], "username");
    }
}
