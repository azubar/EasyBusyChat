#![allow(non_snake_case)]
use std::ffi::c_void;
type HANDLE = *mut c_void;
#[link(name = "kernel32")]
extern "system" {
    fn CreateNamedPipeW(name: *const u16, open: u32, mode: u32, instances: u32, out_size: u32, in_size: u32, timeout: u32, sa: *const c_void) -> HANDLE;
    fn CreateFileW(name: *const u16, access: u32, share: u32, sa: *const c_void, creation: u32, flags: u32, template: HANDLE) -> HANDLE;
    fn SetNamedPipeHandleState(pipe: HANDLE, mode: *const u32, count: *const u32, timeout: *const u32) -> i32;
    fn ConnectNamedPipe(pipe: HANDLE, ov: *mut c_void) -> i32;
    fn PeekNamedPipe(pipe: HANDLE, buffer: *mut c_void, size: u32, read: *mut u32, available: *mut u32, left: *mut u32) -> i32;
    fn WriteFile(pipe: HANDLE, bytes: *const u8, size: u32, written: *mut u32, ov: *mut c_void) -> i32;
    fn ReadFile(pipe: HANDLE, bytes: *mut u8, size: u32, read: *mut u32, ov: *mut c_void) -> i32;
    fn CloseHandle(handle: HANDLE) -> i32;
}
// The runner extracts this function verbatim from ui/src/ipc.rs.
include!(concat!(env!("EASYBUSY_PIPE_TEST_DIR"), "/pipe-write.inc.rs"));
fn main() {
    let name: Vec<u16> = format!(r"\\.\pipe\EasyBusy-Audit-{}", std::process::id()).encode_utf16().chain(Some(0)).collect();
    unsafe {
        let server = CreateNamedPipeW(name.as_ptr(), 3, 0, 1, 65536, 65536, 0, std::ptr::null());
        assert_ne!(server as isize, -1);
        let client = CreateFileW(name.as_ptr(), 0xC0000000, 0, std::ptr::null(), 3, 0, std::ptr::null_mut());
        assert_ne!(client as isize, -1);
        ConnectNamedPipe(server, std::ptr::null_mut());
        assert_ne!(SetNamedPipeHandleState(server, &1, std::ptr::null(), std::ptr::null()), 0);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_stop = stop.clone();
        let reader_handle = client as usize;
        let reader = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(25));
            let mut collected = Vec::new();
            loop {
                let mut available = 0;
                let handle = reader_handle as HANDLE;
                if PeekNamedPipe(handle, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut available, std::ptr::null_mut()) == 0 { break; }
                if available > 0 {
                    let start = collected.len();
                    collected.resize(start + available as usize, 0);
                    let mut n = 0;
                    if ReadFile(handle, collected[start..].as_mut_ptr(), available, &mut n, std::ptr::null_mut()) == 0 { break; }
                    collected.truncate(start + n as usize);
                } else if reader_stop.load(std::sync::atomic::Ordering::Acquire) { break; }
                else { std::thread::sleep(std::time::Duration::from_millis(1)); }
            }
            collected
        });
        let original = format!("{{\"event\":\"notification\",\"payload\":\"{}\"}}\n", "a".repeat(200_000));
        let result = pipe_write_all(server, original.as_bytes());
        let next = b"{\"event\":\"reactReady\"}\n";
        let second = pipe_write_all(server, next);
        stop.store(true, std::sync::atomic::Ordering::Release);
        let collected = reader.join().unwrap();
        let mut expected = original.as_bytes().to_vec();
        expected.extend_from_slice(next);
        assert_eq!(collected, expected, "NDJSON bytes must be preserved exactly");
        let started = std::time::Instant::now();
        let stalled = pipe_write_all(server, original.as_bytes());
        assert_eq!(stalled, Err("Pipe write deadline exceeded"));
        assert!(started.elapsed() < std::time::Duration::from_secs(4), "stalled reader must not hang writer");
        CloseHandle(client);
        CloseHandle(server);
        println!("first={result:?}, requested={}, second={second:?}, received={}", original.len(), collected.len());
        assert!(result.is_ok() && second.is_ok() && collected.len() == original.len() + next.len(), "valid event was dropped although the reader resumes after 25 ms");
        println!("PASS: stalled reader returns at bounded write deadline");
    }
}
