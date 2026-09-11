fn main() {
    tauri_build::build();
    // Tauri links the Windows manifest to binaries, but Cargo examples need it too
    // (Common Controls v6 is required by the real WebView2 regression probe).
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let resource =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("resource.lib");
        println!("cargo:rustc-link-arg-examples={}", resource.display());
    }
}
