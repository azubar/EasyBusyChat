pub mod api_operations;
pub mod commands;
pub mod easybusy_api;
pub mod ipc;
#[path = "../../addin/src/logger.rs"]
pub mod logger;
pub mod profile;

use serde_json::Value;
use std::sync::Mutex;
use tauri::Manager;

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct ChatConfig {
    #[serde(rename = "companyId")]
    pub company_id: String,
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "apiBaseUrl")]
    pub api_base_url: String,
    #[serde(default = "default_generation")]
    pub generation: u64,
    #[serde(rename = "chatMenuActions", default)]
    pub chat_menu_actions: Option<Value>,
}

fn default_generation() -> u64 {
    1
}

// Global state of Tauri app
pub struct AppState {
    pub auth_profile: easy_busy_chat_ipc_protocol::auth_profile::AuthProfile,
    pub pipe_name: String,
    pub pipe_server: Mutex<Option<ipc::PipeServer>>,
    pub config: Mutex<Option<ChatConfig>>,
    pub api_operations: Mutex<api_operations::ApiOperations>,
}

fn init_logging() {
    logger::install();
    if let Some(path) = std::env::var_os("EASYBUSYCHAT_UI_LOG_FILE") {
        let _ = logger::enable(path.to_str());
    }
}

fn acquire_single_instance_mutex(pipe_name: &str) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    // Use the Local namespace to isolate the mutex to the current user session
    // to prevent cross-session attacks and conflicts.
    let mutex_name = format!("Local\\{}", pipe_name.replace('\\', "_"));
    let mutex_name_w: Vec<u16> = mutex_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // Restrict access permissions on the mutex so only the current user can open/interact with it.
    // Fail closed if we cannot generate secure security attributes.
    let user_sa = match ipc::UserSecurityAttributes::new() {
        Some(sa) => sa,
        None => {
            log::error!("Failed to create user-restricted security attributes for single instance Mutex. Failing closed.");
            return false;
        }
    };
    let sa_ptr = user_sa.as_ptr();

    unsafe {
        let handle = CreateMutexW(sa_ptr, 1, mutex_name_w.as_ptr());
        if handle.is_null() {
            return false;
        }
        let error = GetLastError();
        if error == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            return false;
        }
        // Note: We deliberately leak/keep the mutex handle open for the lifetime of this process.
        // It will be cleaned up by the operating system when the process exits.
    }
    true
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_logging();

    // 1. CLI args
    let args: Vec<String> = std::env::args().collect();
    let pipe_name = match args
        .iter()
        .position(|a| a == "--pipe-name")
        .and_then(|i| args.get(i + 1))
    {
        Some(name) => name.clone(),
        None => {
            eprintln!("Usage: EasyBusyUI --pipe-name <pipe-name> [--hidden]");
            std::process::exit(1);
        }
    };

    // 2. Single-instance guard
    if !acquire_single_instance_mutex(&pipe_name) {
        log::warn!("Another instance of EasyBusyUI for this pipe is already running. Exiting.");
        std::process::exit(0);
    }

    // Assign the profile before Tauri creates any configured WebView.
    let mut context = tauri::generate_context!();
    let auth_profile = match profile::prepare_context(&mut context, &args) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("Cannot initialize isolated WebView2 profile: {error}");
            std::process::exit(1);
        }
    };

    // 3. Tauri Builder
    tauri::Builder::default()
        .manage(AppState {
            auth_profile,
            pipe_name: pipe_name.clone(),
            pipe_server: Mutex::new(None),
            config: Mutex::new(None),
            api_operations: Mutex::new(api_operations::ApiOperations::default()),
        })
        .invoke_handler(tauri::generate_handler![
            commands::on_webview_message,
            commands::get_config,
            commands::resolve_contact_chat,
        ])
        .setup(move |app| {
            for config in app.config().app.windows.clone() {
                profile::build_window(app, &config)?;
            }
            let handle = app.handle().clone();

            // Start Named Pipe server
            let server = ipc::PipeServer::start(pipe_name, handle);
            app.state::<AppState>()
                .pipe_server
                .lock()
                .unwrap()
                .replace(server);

            Ok(())
        })
        .run(context)
        .expect("error while running tauri application");
}
