use crate::event_queue::EventQueue;
use crate::worker::ChatSelection;
use easy_busy_chat_ipc_protocol::{FrameError, NdjsonDecoder};
use native_api_1c::native_api_1c_core::ffi::connection::Connection;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const MAX_EVENT_FRAME_BYTES: usize = 1024 * 1024;
const WRITE_TIMEOUT_MS: u32 = 2000;
const READ_POLL_MS: u32 = 100;
const PUSH_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SafeHandle(pub HANDLE);

unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

#[derive(Debug)]
enum PipeIoError {
    Windows(u32),
    TimedOut,
    ZeroBytes,
}

impl std::fmt::Display for PipeIoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Windows(code) => write!(formatter, "Windows error {code}"),
            Self::TimedOut => formatter.write_str("pipe I/O timed out"),
            Self::ZeroBytes => formatter.write_str("pipe I/O transferred zero bytes"),
        }
    }
}

fn create_overlapped_event() -> Result<(Box<OVERLAPPED>, HANDLE), PipeIoError> {
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(PipeIoError::Windows(unsafe { GetLastError() }));
    }
    let mut overlapped = Box::new(unsafe { std::mem::zeroed::<OVERLAPPED>() });
    overlapped.hEvent = event;
    Ok((overlapped, event))
}

fn wait_for_overlapped(
    handle: HANDLE,
    overlapped: &OVERLAPPED,
    timeout_ms: u32,
) -> Result<u32, PipeIoError> {
    let wait_result = unsafe { WaitForSingleObject(overlapped.hEvent, timeout_ms) };
    if wait_result == WAIT_TIMEOUT {
        unsafe {
            CancelIoEx(handle, overlapped);
        }
        let mut cancelled_bytes = 0;
        unsafe {
            GetOverlappedResult(handle, overlapped, &mut cancelled_bytes, 1);
        }
        return Err(PipeIoError::TimedOut);
    }
    if wait_result != WAIT_OBJECT_0 {
        return Err(PipeIoError::Windows(unsafe { GetLastError() }));
    }

    let mut transferred = 0;
    let result = unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, 0) };
    if result == 0 {
        return Err(PipeIoError::Windows(unsafe { GetLastError() }));
    }
    Ok(transferred)
}

fn overlapped_write_all(handle: HANDLE, mut bytes: &[u8]) -> Result<(), PipeIoError> {
    while !bytes.is_empty() {
        let (mut overlapped, event) = create_overlapped_event()?;
        let mut immediate = 0;
        let result = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len().min(u32::MAX as usize) as u32,
                &mut immediate,
                overlapped.as_mut(),
            )
        };
        let transferred = if result != 0 {
            immediate
        } else {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                unsafe {
                    CloseHandle(event);
                }
                return Err(PipeIoError::Windows(error));
            }
            let completed = wait_for_overlapped(handle, &overlapped, WRITE_TIMEOUT_MS);
            unsafe {
                CloseHandle(event);
            }
            completed?
        };
        if result != 0 {
            unsafe {
                CloseHandle(event);
            }
        }
        if transferred == 0 {
            return Err(PipeIoError::ZeroBytes);
        }
        bytes = &bytes[transferred as usize..];
    }
    Ok(())
}

fn overlapped_read(
    handle: HANDLE,
    shutdown: &AtomicBool,
    buffer: &mut [u8],
) -> Result<Option<usize>, PipeIoError> {
    let (mut overlapped, event) = create_overlapped_event()?;
    let mut immediate = 0;
    let result = unsafe {
        ReadFile(
            handle,
            buffer.as_mut_ptr(),
            buffer.len().min(u32::MAX as usize) as u32,
            &mut immediate,
            overlapped.as_mut(),
        )
    };
    if result != 0 {
        unsafe {
            CloseHandle(event);
        }
        return Ok(Some(immediate as usize));
    }

    let error = unsafe { GetLastError() };
    if error != ERROR_IO_PENDING {
        unsafe {
            CloseHandle(event);
        }
        return Err(PipeIoError::Windows(error));
    }

    loop {
        if shutdown.load(Ordering::Acquire) {
            unsafe {
                CancelIoEx(handle, &*overlapped);
            }
            let mut cancelled_bytes = 0;
            unsafe {
                GetOverlappedResult(handle, &*overlapped, &mut cancelled_bytes, 1);
                CloseHandle(event);
            }
            return Ok(None);
        }
        let wait_result = unsafe { WaitForSingleObject(event, READ_POLL_MS) };
        if wait_result == WAIT_TIMEOUT {
            continue;
        }
        if wait_result != WAIT_OBJECT_0 {
            unsafe {
                CloseHandle(event);
            }
            return Err(PipeIoError::Windows(unsafe { GetLastError() }));
        }
        let mut transferred = 0;
        let completed = unsafe { GetOverlappedResult(handle, &*overlapped, &mut transferred, 0) };
        unsafe {
            CloseHandle(event);
        }
        if completed == 0 {
            return Err(PipeIoError::Windows(unsafe { GetLastError() }));
        }
        return Ok(Some(transferred as usize));
    }
}

fn serialize_command_frame(
    action: &str,
    operation_id: Option<&str>,
    payload: Option<serde_json::Value>,
) -> anyhow::Result<Vec<u8>> {
    let mut command = serde_json::json!({
        "action": action,
        "payload": payload.unwrap_or(serde_json::Value::Null),
    });
    if let Some(operation_id) = operation_id {
        command["operationId"] = serde_json::Value::String(operation_id.to_string());
    }
    let mut data = serde_json::to_vec(&command)?;
    if data.len() > MAX_EVENT_FRAME_BYTES {
        return Err(anyhow::anyhow!("IPC command exceeds 1 MiB"));
    }
    data.push(b'\n');
    Ok(data)
}

pub struct PipeClient {
    handle: SafeHandle,
    connected: Arc<AtomicBool>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
    dispatcher_thread: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

struct ExternalEventMessage {
    name: String,
    payload: String,
}

impl PipeClient {
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        pipe_name: &str,
        connection: Arc<Option<&'static Connection>>,
        event_queue: Arc<crate::event_queue::EventQueue>,
        push_events_enabled: Arc<AtomicBool>,
        external_events_ready: Arc<AtomicBool>,
        webview_ready: Arc<AtomicBool>,
        browser_ready: Arc<AtomicBool>,
        chat_selection: Arc<RwLock<Option<ChatSelection>>>,
        connected: Arc<AtomicBool>,
        timeout_ms: u32,
    ) -> anyhow::Result<Self> {
        let pipe_name_w = pipe_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        unsafe {
            if WaitNamedPipeW(pipe_name_w.as_ptr(), timeout_ms) == 0 {
                return Err(anyhow::anyhow!(
                    "WaitNamedPipeW failed: error {}",
                    GetLastError()
                ));
            }
        }

        let handle = unsafe {
            CreateFileW(
                pipe_name_w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(anyhow::anyhow!("CreateFileW failed: error {}", unsafe {
                GetLastError()
            }));
        }

        connected.store(true, Ordering::Release);
        let shutdown = Arc::new(AtomicBool::new(false));
        let (event_sender, event_receiver) =
            mpsc::sync_channel::<ExternalEventMessage>(PUSH_QUEUE_CAPACITY);
        let dispatcher_shutdown = shutdown.clone();
        let dispatcher_push_events_enabled = push_events_enabled.clone();
        let dispatcher_thread = std::thread::Builder::new()
            .name("external-event-dispatcher".to_string())
            .spawn(move || {
                Self::external_event_dispatcher_loop(
                    event_receiver,
                    connection,
                    dispatcher_push_events_enabled,
                    external_events_ready,
                    dispatcher_shutdown,
                );
            })
            .map_err(|error| {
                connected.store(false, Ordering::Release);
                unsafe {
                    CloseHandle(handle);
                }
                error
            })?;

        let reader_connected = connected.clone();
        let reader_shutdown = shutdown.clone();
        let reader_push_events_enabled = push_events_enabled;
        let reader_handle = SafeHandle(handle);
        let reader_thread = match std::thread::Builder::new()
            .name("pipe-reader".to_string())
            .spawn(move || {
                Self::reader_loop(
                    reader_handle,
                    reader_connected,
                    reader_shutdown,
                    event_queue,
                    reader_push_events_enabled,
                    webview_ready,
                    browser_ready,
                    chat_selection,
                    event_sender,
                );
            }) {
            Ok(thread) => thread,
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                connected.store(false, Ordering::Release);
                let _ = dispatcher_thread.join();
                unsafe {
                    CloseHandle(handle);
                }
                return Err(error.into());
            }
        };

        Ok(Self {
            handle: SafeHandle(handle),
            connected,
            reader_thread: Some(reader_thread),
            dispatcher_thread: Some(dispatcher_thread),
            shutdown,
        })
    }

    pub fn send_command(
        &self,
        action: &str,
        payload: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        self.send_command_with_operation(action, None, payload)
    }

    pub fn send_correlated_command(
        &self,
        action: &str,
        operation_id: &str,
        payload: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        self.send_command_with_operation(action, Some(operation_id), payload)
    }

    fn send_command_with_operation(
        &self,
        action: &str,
        operation_id: Option<&str>,
        payload: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        if !self.is_connected() {
            return Err(anyhow::anyhow!("Not connected"));
        }
        let data = serialize_command_frame(action, operation_id, payload)?;
        if let Err(error) = overlapped_write_all(self.handle.0, &data) {
            self.connected.store(false, Ordering::Release);
            return Err(anyhow::anyhow!("Pipe write failed: {error}"));
        }
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    #[allow(clippy::too_many_arguments)]
    fn reader_loop(
        handle: SafeHandle,
        connected: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
        event_queue: Arc<crate::event_queue::EventQueue>,
        push_events_enabled: Arc<AtomicBool>,
        webview_ready: Arc<AtomicBool>,
        browser_ready: Arc<AtomicBool>,
        chat_selection: Arc<RwLock<Option<ChatSelection>>>,
        event_sender: mpsc::SyncSender<ExternalEventMessage>,
    ) {
        let mut buffer = vec![0u8; 64 * 1024];
        let mut decoder = NdjsonDecoder::new(MAX_EVENT_FRAME_BYTES);
        while !shutdown.load(Ordering::Acquire) {
            let bytes_read = match overlapped_read(handle.0, &shutdown, &mut buffer) {
                Ok(Some(0)) => break,
                Ok(Some(bytes_read)) => bytes_read,
                Ok(None) => break,
                Err(error) => {
                    if !shutdown.load(Ordering::Acquire) {
                        event_queue.push(
                            serde_json::json!({
                                "event": "error",
                                "payload": { "code": "pipe_read_failed", "message": error.to_string() }
                            })
                            .to_string(),
                        );
                    }
                    break;
                }
            };
            let frames = match decoder.push(&buffer[..bytes_read]) {
                Ok(frames) => frames,
                Err(FrameError::TooLarge) => {
                    event_queue.push(
                        r#"{"event":"error","payload":{"code":"frame_too_large","message":"IPC event exceeds 1 MiB"}}"#
                            .to_string(),
                    );
                    break;
                }
                Err(FrameError::InvalidUtf8) => {
                    event_queue.push(
                        r#"{"event":"error","payload":{"code":"invalid_utf8","message":"IPC event is not valid UTF-8"}}"#
                            .to_string(),
                    );
                    break;
                }
            };
            for json_line in frames {
                Self::handle_event_frame(
                    json_line,
                    &event_queue,
                    &push_events_enabled,
                    &webview_ready,
                    &browser_ready,
                    &chat_selection,
                    &event_sender,
                );
            }
        }
        connected.store(false, Ordering::Release);
        webview_ready.store(false, Ordering::Release);
        browser_ready.store(false, Ordering::Release);
        *chat_selection.write() = None;
    }

    fn handle_event_frame(
        json_line: String,
        event_queue: &EventQueue,
        push_events_enabled: &AtomicBool,
        webview_ready: &AtomicBool,
        browser_ready: &AtomicBool,
        chat_selection: &RwLock<Option<ChatSelection>>,
        event_sender: &mpsc::SyncSender<ExternalEventMessage>,
    ) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&json_line) else {
            return;
        };
        let event_name = event
            .get("event")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
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
            "commandResult",
        ];
        if !event_whitelist.contains(&event_name) {
            return;
        }
        if event_name == "webviewReady" {
            webview_ready.store(true, Ordering::Release);
        }
        if matches!(event_name, "browserReady" | "reactReady") {
            browser_ready.store(true, Ordering::Release);
        }
        if event_name == "chatOpened" {
            let payload = event.get("payload").unwrap_or(&serde_json::Value::Null);
            let selection = ChatSelection {
                generation: payload
                    .get("generation")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                provider: payload
                    .get("provider")
                    .and_then(|value| match value {
                        serde_json::Value::String(text) => Some(text.clone()),
                        serde_json::Value::Number(number) => Some(number.to_string()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                account_id: payload
                    .get("accountId")
                    .and_then(|value| match value {
                        serde_json::Value::String(text) => Some(text.clone()),
                        serde_json::Value::Number(number) => Some(number.to_string()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                chat_id: payload
                    .get("chatId")
                    .and_then(|value| match value {
                        serde_json::Value::String(text) => Some(text.clone()),
                        serde_json::Value::Number(number) => Some(number.to_string()),
                        _ => None,
                    })
                    .unwrap_or_default(),
            };
            if selection.generation > 0
                && !selection.provider.is_empty()
                && !selection.account_id.is_empty()
                && !selection.chat_id.is_empty()
            {
                *chat_selection.write() = Some(selection);
            }
        }
        event_queue.push(json_line);

        if push_events_enabled.load(Ordering::Acquire) {
            let payload = event
                .get("payload")
                .map(serde_json::Value::to_string)
                .unwrap_or_default();
            let _ = event_sender.try_send(ExternalEventMessage {
                name: event_name.to_string(),
                payload,
            });
        }
    }

    fn external_event_dispatcher_loop(
        event_receiver: mpsc::Receiver<ExternalEventMessage>,
        connection: Arc<Option<&'static Connection>>,
        push_events_enabled: Arc<AtomicBool>,
        external_events_ready: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
    ) {
        while !shutdown.load(Ordering::Acquire) {
            let message = match event_receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if !push_events_enabled.load(Ordering::Acquire)
                || !external_events_ready.load(Ordering::Acquire)
            {
                continue;
            }
            if let Some(connection) = connection.as_ref() {
                let _ = connection.external_event("EasyBusyChat", &message.name, &message.payload);
            }
        }
    }
}

impl Drop for PipeClient {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        unsafe {
            CancelIoEx(self.handle.0, std::ptr::null());
        }
        if let Some(reader) = self.reader_thread.take() {
            let _ = reader.join();
        }
        unsafe {
            CloseHandle(self.handle.0);
        }

        if let Some(dispatcher) = self.dispatcher_thread.take() {
            let _ = dispatcher.join();
        }
    }
}

#[cfg(test)]
mod correlated_command_tests {
    use super::serialize_command_frame;

    #[test]
    fn correlated_command_has_top_level_operation_id() {
        let frame = serialize_command_frame(
            "openChatByIdentity",
            Some("op-1234-25"),
            Some(serde_json::json!({
                "provider": "telegram",
                "accountId": "account-1",
                "identity": { "type": "phone", "value": "+380501234567" },
            })),
        )
        .expect("command frame");

        assert_eq!(frame.last(), Some(&b'\n'));
        let value: serde_json::Value =
            serde_json::from_slice(&frame[..frame.len() - 1]).expect("valid JSON");
        assert_eq!(value["action"], "openChatByIdentity");
        assert_eq!(value["operationId"], "op-1234-25");
        assert_eq!(value["payload"]["provider"], "telegram");
        assert!(value.get("event").is_none());
    }
}
