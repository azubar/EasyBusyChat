// Standalone Windows ABI smoke test. No host callbacks, UI or network requests.
use std::ffi::{c_char, c_void};
use std::path::Path;
use std::ptr::null_mut;

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(path: *const u16) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    fn FindResourceW(module: *mut c_void, name: *const u16, kind: *const u16) -> *mut c_void;
    fn LoadResource(module: *mut c_void, resource: *mut c_void) -> *mut c_void;
    fn LockResource(resource: *mut c_void) -> *mut c_void;
    fn SizeofResource(module: *mut c_void, resource: *mut c_void) -> u32;
}

#[repr(C)]
struct InitDoneVTable {
    destructor: usize,
    init: usize,
    memory_manager: usize,
    get_info: unsafe extern "system" fn(*mut c_void) -> i32,
    done: unsafe extern "system" fn(*mut c_void),
}

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let path = Path::new(arguments.get(1).expect("DLL path required")).canonicalize().unwrap();
    let wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain(Some(0)).collect();
    for cycle in 0..20 {
        unsafe {
            let module = LoadLibraryW(wide.as_ptr());
            assert!(!module.is_null(), "DLL load failed: {}", std::io::Error::last_os_error());
            let names = GetProcAddress(module, c"GetClassNames".as_ptr());
            let create = GetProcAddress(module, c"GetClassObject".as_ptr());
            let destroy = GetProcAddress(module, c"DestroyObject".as_ptr());
            assert!(!names.is_null() && !create.is_null() && !destroy.is_null());
            let names: unsafe extern "C" fn() -> *const u16 = std::mem::transmute(names);
            let create: unsafe extern "C" fn(*const u16, *mut *mut c_void) -> i32 = std::mem::transmute(create);
            let destroy: unsafe extern "C" fn(*mut *mut c_void) -> i32 = std::mem::transmute(destroy);
            let mut first = null_mut();
            let mut second = null_mut();
            assert_eq!(create(names(), &mut first), 1);
            assert_eq!(create(names(), &mut second), 1);
            assert!(!first.is_null() && !second.is_null() && first != second);
            let table = &**(first as *const *const InitDoneVTable);
            (table.done)(first);
            (table.done)(first); // Done must be idempotent.
            assert_eq!(destroy(&mut first), 0);
            assert_eq!(destroy(&mut second), 0); // Drop must also stop worker without Done.
            assert!(first.is_null() && second.is_null());
            if let Some(exe) = arguments.get(2).filter(|_| cycle == 0) {
                let expected = std::fs::read(exe).unwrap();
                let resource = FindResourceW(module, 101 as *const u16, 10 as *const u16);
                assert!(!resource.is_null());
                let size = SizeofResource(module, resource) as usize;
                let data = LockResource(LoadResource(module, resource));
                assert!(!data.is_null());
                assert!(std::slice::from_raw_parts(data as *const u8, size) == expected, "Embedded UI differs from supplied artifact");
            }
            assert_ne!(FreeLibrary(module), 0);
        }
    }
    println!("PASS: 20 DLL load/unload cycles, 40 component instances, Done and Drop");
}
