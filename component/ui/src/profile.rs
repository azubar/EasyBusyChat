use easy_busy_chat_ipc_protocol::auth_profile::AuthProfile;
use std::path::Path;

/// Called before Builder::run creates windows; no shared-profile fallback.
pub fn prepare_context(
    context: &mut tauri::Context<tauri::Wry>,
    args: &[String],
) -> Result<AuthProfile, &'static str> {
    let key = args
        .iter()
        .position(|arg| arg == "--profile-key")
        .and_then(|index| args.get(index + 1))
        .ok_or("Missing --profile-key from DLL bootstrap")?;
    let profile = AuthProfile::parse(key)?;
    let local_data = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?;
    configure_windows(context.config_mut(), &profile, Path::new(&local_data))?;
    // Called before Tauri starts WebView2 or worker threads.
    std::env::remove_var("WEBVIEW2_USER_DATA_FOLDER");
    Ok(profile)
}

pub fn configure_windows(
    config: &mut tauri::utils::config::Config,
    profile: &AuthProfile,
    local_data: &Path,
) -> Result<(), &'static str> {
    let directory = profile.data_directory(local_data)?;
    if config.app.windows.is_empty() {
        return Err("No configured application window");
    }
    for window in &mut config.app.windows {
        window.data_directory = Some(directory.clone());
        // Build explicitly in setup: Tauri 2.11's config conversion does not
        // propagate data_directory into WebviewAttributes.
        window.create = false;
    }
    Ok(())
}

pub fn build_window<M: tauri::Manager<tauri::Wry>>(
    manager: &M,
    config: &tauri::utils::config::WindowConfig,
) -> anyhow::Result<tauri::WebviewWindow> {
    let directory = config
        .data_directory
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Missing isolated WebView2 directory"))?;
    Ok(tauri::WebviewWindowBuilder::from_config(manager, config)?
        .data_directory(directory)
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_separates_actual_window_configs_and_preserves_release_settings() {
        let base: tauri::utils::config::Config =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let root = std::env::temp_dir();
        let first = AuthProfile::for_context("api", "company", "synthetic-a");
        let second = AuthProfile::for_context("api", "company", "synthetic-b");
        let mut a = base.clone();
        let mut b = base.clone();
        configure_windows(&mut a, &first, &root).unwrap();
        configure_windows(&mut b, &second, &root).unwrap();
        assert_ne!(
            a.app.windows[0].data_directory,
            b.app.windows[0].data_directory
        );
        configure_windows(&mut b, &first, &root).unwrap();
        assert_eq!(
            a.app.windows[0].data_directory,
            b.app.windows[0].data_directory
        );
        assert!(a.app.windows[0].use_https_scheme);
        assert!(!a.app.windows[0].create);
        assert_eq!(a.app.windows[0].devtools, Some(true));
        assert_eq!(a.app.security.csp, base.app.security.csp);
    }
}
