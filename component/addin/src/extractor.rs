use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::*;

const IDR_EASYBUSYUI_EXE: u16 = 101;
const RT_RCDATA: *const u16 = 10 as *const u16;

pub struct ExeExtractor;

impl ExeExtractor {
    /// Get HMODULE of this DLL
    fn get_dll_hmodule() -> anyhow::Result<HMODULE> {
        let mut hmodule: HMODULE = std::ptr::null_mut();
        unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                Self::get_dll_hmodule as *const u16,
                &mut hmodule,
            );
        }
        if hmodule.is_null() {
            return Err(anyhow::anyhow!("GetModuleHandleExW failed"));
        }
        Ok(hmodule)
    }

    /// Find DLL directory path
    pub fn get_dll_directory() -> anyhow::Result<std::path::PathBuf> {
        let hmodule = Self::get_dll_hmodule()?;
        let mut path_buf = vec![0u16; 32768];

        let len =
            unsafe { GetModuleFileNameW(hmodule, path_buf.as_mut_ptr(), path_buf.len() as u32) };

        if len == 0 || len == path_buf.len() as u32 {
            return Err(anyhow::anyhow!(
                "GetModuleFileNameW failed or path was truncated"
            ));
        }

        let path = std::path::PathBuf::from(String::from_utf16_lossy(&path_buf[..len as usize]));
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Invalid DLL path"))?;
        Ok(parent.to_path_buf())
    }

    /// Get target directory for UI extraction (prefers per-user local app data directory for security and writeability)
    pub fn get_extraction_directory() -> std::path::PathBuf {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            let path = std::path::PathBuf::from(local_app_data).join("EasyBusyChat");
            let _ = std::fs::create_dir_all(&path);
            if path.exists() {
                return path;
            }
        }
        Self::get_dll_directory().unwrap_or_else(|_| std::env::temp_dir())
    }

    pub fn extract_if_needed() -> anyhow::Result<String> {
        use sha2::{Digest, Sha256};
        static IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let data = Self::resource_bytes()?;
        let identity = IDENTITY.get_or_init(|| {
            format!(
                "{}-{}-{:x}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::ARCH,
                Sha256::digest(data)
            )
        });
        let path = Self::get_extraction_directory()
            .join("bin")
            .join(identity)
            .join("EasyBusyUI.exe");
        Self::publish_resource(&path, data)?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn publish_resource(path: &std::path::Path, data: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
        if std::fs::read(path).is_ok_and(|bytes| bytes == data) {
            return Ok(());
        }
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("Artifact has no parent directory"))?,
        )?;
        let temp = path.with_extension(format!(
            "exe.{}.{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> anyhow::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)?;
            file.write_all(data)?;
            file.sync_all()?;
            drop(file);
            if let Err(error) = std::fs::rename(&temp, path) {
                if !std::fs::read(path).is_ok_and(|bytes| bytes == data) {
                    return Err(error.into());
                }
            }
            Ok(())
        })();
        if temp.exists() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    fn resource_bytes() -> anyhow::Result<&'static [u8]> {
        let hmodule = Self::get_dll_hmodule()?;
        unsafe {
            let hrsrc = FindResourceW(hmodule, IDR_EASYBUSYUI_EXE as *const u16, RT_RCDATA);
            if hrsrc.is_null() {
                return Err(anyhow::anyhow!("FindResourceW failed"));
            }
            let size = SizeofResource(hmodule, hrsrc);
            if size == 0 {
                return Err(anyhow::anyhow!("SizeofResource returned 0"));
            }
            let hglobal = LoadResource(hmodule, hrsrc);
            if hglobal.is_null() {
                return Err(anyhow::anyhow!("LoadResource failed"));
            }
            let data_ptr = LockResource(hglobal) as *const u8;
            if data_ptr.is_null() {
                return Err(anyhow::anyhow!("LockResource failed"));
            }
            Ok(std::slice::from_raw_parts(data_ptr, size as usize))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn immutable_artifacts_coexist_and_corruption_is_not_trusted() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/artifact-test");
        let first = root.join("version-one/EasyBusyUI.exe");
        let second = root.join("version-two/EasyBusyUI.exe");
        ExeExtractor::publish_resource(&first, b"first").unwrap();
        ExeExtractor::publish_resource(&second, b"second").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        std::fs::write(&first, b"wrong").unwrap();
        ExeExtractor::publish_resource(&first, b"first").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
    }
}
