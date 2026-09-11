use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace_root = Path::new(&manifest_dir).parent().unwrap();
    let profile = env::var("PROFILE").unwrap();

    let target = env::var("TARGET").unwrap();

    let exe_dest = Path::new(&out_dir).join("EasyBusyUI.exe");

    let skip_ui = env::var("EASYBUSYCHAT_SKIP_UI_BUILD").as_deref() == Ok("1");
    assert!(
        !skip_ui || profile == "debug",
        "UI placeholder is forbidden outside the debug test profile"
    );
    if skip_ui {
        println!("cargo:warning=Skipping embedded UI build for add-in unit tests");
        std::fs::write(&exe_dest, b"unit-test-placeholder")
            .expect("Failed to create unit-test UI placeholder");
    } else if let Some(prebuilt) = env::var_os("EASYBUSYCHAT_UI_EXE") {
        let prebuilt = PathBuf::from(prebuilt);
        validate_ui_exe(&prebuilt, &target);
        std::fs::copy(prebuilt, &exe_dest).expect("Failed to embed prebuilt UI");
    } else {
        // === Step 1: Build UI Crate ===
        println!(
            "cargo:warning=Building EasyBusyUI.exe for target {}...",
            target
        );

        let ui_target_dir = env::var_os("EASYBUSYCHAT_UI_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root.join("target").join("ui_target"));

        let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let mut build_cmd = Command::new(cargo);
        build_cmd.args([
            "build",
            "--locked",
            "--package",
            "easy_busy_chat_ui",
            "--bin",
            "EasyBusyUI",
        ]);
        if profile == "release" {
            build_cmd.arg("--release");
        }
        build_cmd.arg("--target").arg(&target);
        build_cmd.arg("--target-dir").arg(&ui_target_dir);
        build_cmd.current_dir(workspace_root);

        let status = build_cmd
            .status()
            .expect("Failed to run cargo build for UI");
        assert!(status.success(), "UI crate build failed!");

        // === Step 2: Copy EXE to OUT_DIR ===
        let target_subdir = if profile == "release" {
            "release"
        } else {
            "debug"
        };
        let exe_source = ui_target_dir
            .join(&target)
            .join(target_subdir)
            .join("EasyBusyUI.exe");

        assert!(
            exe_source.exists(),
            "EasyBusyUI.exe not found at {:?}",
            exe_source
        );
        validate_ui_exe(&exe_source, &target);
        std::fs::copy(&exe_source, &exe_dest).expect("Failed to copy EXE to OUT_DIR");
    }

    // === Step 3: Copy resource.rc to OUT_DIR ===
    let rc_source = Path::new(&manifest_dir)
        .join("resources")
        .join("resource.rc");
    let rc_dest = Path::new(&out_dir).join("resource.rc");

    // Ensure parent dir of rc_dest exists (just in case)
    if let Some(parent) = rc_dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    std::fs::copy(&rc_source, &rc_dest).expect("Failed to copy resource.rc");

    embed_resource::compile(&rc_dest, embed_resource::NONE);

    // === Rerun triggers ===
    println!("cargo:rerun-if-changed=resources/resource.rc");
    println!("cargo:rerun-if-changed=../ui/src/");
    println!("cargo:rerun-if-changed=../ui/frontend/");
    println!("cargo:rerun-if-changed=../ui/tauri.conf.json");
    println!("cargo:rerun-if-env-changed=EASYBUSYCHAT_SKIP_UI_BUILD");
    println!("cargo:rerun-if-env-changed=EASYBUSYCHAT_UI_TARGET_DIR");
    println!("cargo:rerun-if-env-changed=EASYBUSYCHAT_UI_EXE");
    if let Some(path) = env::var_os("EASYBUSYCHAT_UI_EXE") {
        println!("cargo:rerun-if-changed={}", PathBuf::from(path).display());
    }
    for input in [
        "../Cargo.toml",
        "../Cargo.lock",
        "../.cargo/config.toml",
        "../ui/Cargo.toml",
        "../ui/build.rs",
        "../ui/permissions",
        "../ui/capabilities",
        "../ui/icons",
        "src/logger.rs",
        "../ipc_protocol",
    ] {
        println!("cargo:rerun-if-changed={input}");
    }
}

fn validate_ui_exe(path: &Path, target: &str) {
    let bytes = std::fs::read(path).expect("Cannot read UI artifact");
    assert!(
        bytes.len() >= 64 && &bytes[..2] == b"MZ",
        "UI artifact is not an executable"
    );
    let offset = u32::from_le_bytes(bytes[60..64].try_into().unwrap()) as usize;
    assert!(
        offset <= bytes.len().saturating_sub(6) && &bytes[offset..offset + 4] == b"PE\0\0",
        "Invalid PE header"
    );
    let machine = u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into().unwrap());
    let expected = if target.starts_with("x86_64-") {
        0x8664
    } else if target.starts_with("i686-") {
        0x014c
    } else {
        panic!("Unsupported target")
    };
    assert_eq!(
        machine, expected,
        "UI artifact architecture does not match DLL"
    );
}
