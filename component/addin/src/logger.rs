use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
static LOGGER: RuntimeLogger = RuntimeLogger {
    enabled: AtomicBool::new(false),
    file: Mutex::new(None),
};
static INSTALL_RESULT: OnceLock<bool> = OnceLock::new();

struct LogFile {
    file: File,
    path: PathBuf,
    bytes: u64,
}
struct RuntimeLogger {
    enabled: AtomicBool,
    file: Mutex<Option<LogFile>>,
}

impl log::Log for RuntimeLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.enabled.load(Ordering::Acquire) && metadata.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let Ok(mut guard) = self.file.lock() else {
            return;
        };
        let Some(active) = guard.as_ref() else {
            return;
        };
        let text = format!("{} [{}] {}\n", timestamp(), record.level(), record.args());
        let mut line: String = text.chars().take(4096).collect();
        if !line.ends_with('\n') {
            line.push('\n');
        }
        if active.bytes + line.len() as u64 > MAX_LOG_BYTES {
            let path = active.path.clone();
            *guard = None;
            let backup = path.with_extension("log.1");
            if backup.exists() {
                let _ = std::fs::remove_file(&backup);
            }
            if std::fs::rename(&path, backup).is_err() {
                return;
            }
            *guard = open_log(&path).ok();
        }
        if let Some(active) = guard.as_mut() {
            if active.file.write_all(line.as_bytes()).is_ok() {
                active.bytes += line.len() as u64;
            }
        }
    }
    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(active) = guard.as_mut() {
                let _ = active.file.flush();
            }
        }
    }
}
fn open_log(path: &Path) -> std::io::Result<LogFile> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let bytes = file.metadata()?.len();
    Ok(LogFile {
        file,
        path: path.to_path_buf(),
        bytes,
    })
}
pub fn install() -> bool {
    *INSTALL_RESULT.get_or_init(|| {
        let installed = log::set_logger(&LOGGER).is_ok();
        if installed {
            log::set_max_level(log::LevelFilter::Info);
        }
        installed
    })
}
pub fn enable(path: Option<&str>) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(install(), "Another logger is already installed");
    let path = resolve_log_path(path);
    let file = open_log(&path)?;
    *LOGGER
        .file
        .lock()
        .map_err(|_| anyhow::anyhow!("Logger lock failed"))? = Some(file);
    LOGGER.enabled.store(true, Ordering::Release);
    Ok(path)
}
pub fn disable() {
    LOGGER.enabled.store(false, Ordering::Release);
    log::logger().flush();
    if let Ok(mut file) = LOGGER.file.lock() {
        *file = None;
    }
}
pub fn current_path() -> Option<PathBuf> {
    LOGGER
        .file
        .lock()
        .ok()?
        .as_ref()
        .map(|file| file.path.clone())
}
fn resolve_log_path(path: Option<&str>) -> PathBuf {
    match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("EasyBusyChat")
            .join("EasyBusyChatAddIn.log"),
    }
}
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}
#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;

    #[test]
    fn disabled_logger_writes_nothing_and_enabled_logger_rotates() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../target/logger-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.log");
        std::fs::write(&path, b"previous session\n").unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_LOG_BYTES)
            .unwrap();
        let file = open_log(&path).unwrap();
        let logger = RuntimeLogger {
            enabled: AtomicBool::new(false),
            file: Mutex::new(Some(file)),
        };
        logger.log(
            &log::Record::builder()
                .level(log::Level::Info)
                .args(format_args!("disabled diagnostic"))
                .build(),
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MAX_LOG_BYTES);
        assert!(!path.with_extension("log.1").exists());
        logger.enabled.store(true, Ordering::Release);
        logger.log(
            &log::Record::builder()
                .level(log::Level::Info)
                .args(format_args!("enabled diagnostic"))
                .build(),
        );
        logger.flush();
        assert_eq!(
            std::fs::metadata(path.with_extension("log.1"))
                .unwrap()
                .len(),
            MAX_LOG_BYTES
        );
        assert!(std::fs::metadata(&path).unwrap().len() < 4096);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("enabled diagnostic"));
    }
    #[test]
    fn default_log_path_is_explicit_and_stable() {
        assert_eq!(
            resolve_log_path(Some(" ")).file_name().unwrap(),
            "EasyBusyChatAddIn.log"
        );
        assert_eq!(
            resolve_log_path(Some("audit.log")),
            PathBuf::from("audit.log")
        );
    }
}
