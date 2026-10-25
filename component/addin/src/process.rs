use crate::ipc::SafeHandle;
use std::os::windows::io::AsRawHandle;
use std::process::Command;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Threading::*;

/// Launch EasyBusyUI.exe as a child process, returning its PID and duplicated process handle.
pub fn launch_ui(
    exe_path: &str,
    pipe_name: &str,
    profile: &easy_busy_chat_ipc_protocol::auth_profile::AuthProfile,
    log_path: Option<&std::path::Path>,
) -> anyhow::Result<(u32, SafeHandle)> {
    let mut cmd = Command::new(exe_path);
    cmd.arg("--pipe-name").arg(pipe_name).arg("--hidden");
    cmd.arg("--profile-key").arg(profile.key());
    // An inherited WebView2 override must not collapse isolated profiles.
    cmd.env_remove("WEBVIEW2_USER_DATA_FOLDER");

    cmd.env_remove("EASYBUSYCHAT_UI_LOG_FILE");
    if let Some(log_path) = log_path {
        cmd.env("EASYBUSYCHAT_UI_LOG_FILE", ui_log_path(log_path, pipe_name));
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Detach from the console, but keep the host's job membership. A host
        // job may prohibit breakaway; requesting it makes CreateProcess fail
        // with ERROR_ACCESS_DENIED even when the executable is accessible.
        cmd.creation_flags(DETACHED_PROCESS);
    }

    let child = cmd.spawn()?;
    let pid = child.id();

    // Duplicate the raw process handle so it remains valid after Child is dropped
    let raw_handle = child.as_raw_handle();
    let duplicated = unsafe {
        let current_process = GetCurrentProcess();
        let mut dup_handle = INVALID_HANDLE_VALUE;
        let ok = DuplicateHandle(
            current_process,
            raw_handle as HANDLE,
            current_process,
            &mut dup_handle,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        );
        if ok != 0 {
            SafeHandle(dup_handle)
        } else {
            return Err(anyhow::anyhow!("DuplicateHandle failed"));
        }
    };

    Ok((pid, duplicated))
}

/// Check if the process with the given handle is alive
pub fn is_process_alive(handle: HANDLE) -> bool {
    unsafe {
        let mut exit_code: u32 = 0;
        let result = GetExitCodeProcess(handle, &mut exit_code);
        result != 0 && exit_code == 259 // STILL_ACTIVE is 259
    }
}

/// Get process exit code (returns Some(exit_code) if exited, None if still running or failed to query)
pub fn get_process_exit_code(handle: HANDLE) -> Option<u32> {
    unsafe {
        let mut exit_code: u32 = 0;
        let result = GetExitCodeProcess(handle, &mut exit_code);
        if result != 0 && exit_code != 259 {
            Some(exit_code)
        } else {
            None
        }
    }
}

/// Terminate the UI process using its process handle without writing to the IPC pipe.
///
/// Close() is called from 1C's UI thread, so this function must not depend on
/// a possibly blocked Named Pipe. It waits briefly for a natural exit and then
/// force-terminates the child process.
pub fn terminate_ui(handle: HANDLE) {
    let wait_before_kill = unsafe { WaitForSingleObject(handle, 500) };
    if wait_before_kill == WAIT_OBJECT_0 {
        log::info!("EasyBusyUI process exited before forced termination");
        return;
    }

    log::warn!("EasyBusyUI did not exit in time; forcing process termination");
    unsafe {
        TerminateProcess(handle, 1);
    }

    let wait_after_kill = unsafe { WaitForSingleObject(handle, 2000) };
    if wait_after_kill == WAIT_OBJECT_0 {
        log::info!("EasyBusyUI process terminated");
    } else {
        log::warn!(
            "EasyBusyUI process termination was requested but wait returned {}",
            wait_after_kill
        );
    }
}

pub fn ui_log_path(base: &std::path::Path, pipe: &str) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    pipe.hash(&mut hash);
    base.with_extension(format!("ui-{:016x}.log", hash.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::JobObjects::*;

    #[test]
    fn launches_in_a_host_job_that_prohibits_breakaway() {
        const CHILD: &str = "EBC_JOB_LAUNCH_REGRESSION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::tests::launches_in_a_host_job_that_prohibits_breakaway",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!job.is_null());
            assert_ne!(AssignProcessToJobObject(job, GetCurrentProcess()), 0);
            let dir = std::env::temp_dir().join(format!("ebc-job-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("helper.rs");
            let exe = dir.join("helper.exe");
            std::fs::write(
                &source,
                "fn main() { std::thread::sleep(std::time::Duration::from_secs(2)); }",
            )
            .unwrap();
            assert!(Command::new("rustc")
                .arg(&source)
                .arg("-o")
                .arg(&exe)
                .status()
                .unwrap()
                .success());
            // The old launch mode fails in this exact host environment.
            use std::os::windows::process::CommandExt;
            let old_error = Command::new(&exe)
                .arg("__ebc_nonexistent_job_test_file__")
                .creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB)
                .spawn()
                .unwrap_err();
            assert_eq!(old_error.raw_os_error(), Some(5));

            let profile = easy_busy_chat_ipc_protocol::auth_profile::AuthProfile::for_context(
                "https://example.invalid",
                "job-regression",
                "synthetic",
            );
            let (_, child) = launch_ui(exe.to_str().unwrap(), "job-test", &profile, None)
                .expect("UI helper must start without escaping the host job");
            let mut in_job = 0;
            assert_ne!(IsProcessInJob(child.0, job, &mut in_job), 0);
            assert_ne!(in_job, 0);
            assert_eq!(WaitForSingleObject(child.0, 5000), WAIT_OBJECT_0);
            CloseHandle(child.0);
            CloseHandle(job);
            std::fs::remove_file(exe).unwrap();
            std::fs::remove_file(source).unwrap();
            std::fs::remove_dir(dir).unwrap();
        }
    }
}
