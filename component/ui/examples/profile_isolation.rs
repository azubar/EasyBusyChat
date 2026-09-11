//! Real WebView2 regression probe. Synthetic credentials, local pages, hidden windows.
//! --shared reproduces A06; --reuse verifies persistent localStorage from a previous run.
//! Cookie isolation is checked live; cookie disk flush is asynchronous in WebView2.
use easy_busy_chat_ipc_protocol::auth_profile::AuthProfile;
use std::collections::HashSet;
use std::sync::Mutex;
use tauri::{Manager, WebviewUrl};

#[derive(Default)]
struct ProbeState {
    ready: Mutex<HashSet<String>>,
    read: Mutex<HashSet<String>>,
    reuse: bool,
}

fn evaluate(app: &tauri::AppHandle, label: &str, body: &str) {
    app.get_webview_window(label).unwrap().eval(body).unwrap();
}

fn profile_probe(app: &tauri::AppHandle, label: &str, stage: &str, value: &str, origin: &str) {
    let state = app.state::<ProbeState>();
    if origin != "https://profilecheck.localhost" {
        eprintln!("FAIL: unexpected test origin");
        std::process::exit(1);
    }
    match stage {
        "ready" => {
            if state.reuse && value.split('|').next() != Some(format!("synthetic-{label}").as_str()) {
                eprintln!("FAIL: profile did not retain localStorage after restart");
                std::process::exit(1);
            }
            let all_ready = { let mut ready = state.ready.lock().unwrap(); ready.insert(label.into()); ready.len() == 2 };
            if all_ready {
                evaluate(&app, "a", "localStorage.setItem('token','synthetic-a'); document.cookie='token=synthetic-a; path=/; max-age=86400'; report('written-a');");
            }
        }
        "written-a" => evaluate(&app, "b", "localStorage.setItem('token','synthetic-b'); document.cookie='token=synthetic-b; path=/; max-age=86400'; report('written-b');"),
        "written-b" => {
            evaluate(&app, "a", "report('read');");
            evaluate(&app, "b", "report('read');");
        }
        "read" => {
            if value != format!("synthetic-{label}|synthetic-{label}") {
                eprintln!("FAIL: A06 reproduced: another window replaced token/cookie");
                std::process::exit(1);
            }
            let complete = { let mut read = state.read.lock().unwrap(); read.insert(label.into()); read.len() == 2 };
            if complete {
                println!("PASS: two WebView2 windows retain independent localStorage and cookies; reuse={}", state.reuse);
                app.exit(0);
            }
        }
        _ => { eprintln!("FAIL: unexpected probe stage"); std::process::exit(1); }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let shared = args.iter().any(|arg| arg == "--shared");
    let reuse = args.iter().any(|arg| arg == "--reuse");
    let root =
        std::path::PathBuf::from(args.get(1).expect("absolute test data directory required"));
    assert!(root.is_absolute());
    std::env::remove_var("WEBVIEW2_USER_DATA_FOLDER");
    let mut context = tauri::generate_context!();
    let original = context.config().clone();
    context.config_mut().app.windows.clear();
    let mut configs = Vec::new();
    for label in ["a", "b"] {
        let token = if shared {
            "synthetic-a".to_owned()
        } else {
            format!("synthetic-{label}")
        };
        let profile = AuthProfile::for_context("https://api.example", "company", &token);
        let mut config = original.clone();
        easy_busy_chat_ui::profile::configure_windows(&mut config, &profile, &root).unwrap();
        let mut window = config.app.windows.remove(0);
        window.label = label.into();
        window.url = WebviewUrl::CustomProtocol("profilecheck://localhost/".parse().unwrap());
        window.visible = false;
        configs.push(window);
    }
    let html = r#"<!doctype html><meta charset="utf-8"><script>
      function report(stage) {
        const cookie = document.cookie.split('; ').find(v=>v.startsWith('token='));
        fetch('/result', {method:'POST', body:JSON.stringify({stage,
          value: (localStorage.getItem('token')||'')+'|'+(cookie ? cookie.slice(6) : ''), origin: location.origin})});
      }
      window.addEventListener('DOMContentLoaded',()=>report('ready'));
    </script>"#;
    tauri::Builder::default()
        .manage(ProbeState {
            reuse,
            ..Default::default()
        })
        .register_uri_scheme_protocol("profilecheck", move |context, request| {
            if request.uri().path() == "/result" {
                let result: serde_json::Value = serde_json::from_slice(request.body()).unwrap();
                profile_probe(
                    context.app_handle(),
                    context.webview_label(),
                    result["stage"].as_str().unwrap(),
                    result["value"].as_str().unwrap(),
                    result["origin"].as_str().unwrap(),
                );
                return tauri::http::Response::builder().body(Vec::new()).unwrap();
            }
            tauri::http::Response::builder()
                .header("Content-Type", "text/html; charset=utf-8")
                .body(html.as_bytes().to_vec())
                .unwrap()
        })
        .setup(move |app| {
            for config in configs {
                easy_busy_chat_ui::profile::build_window(app, &config)?;
            }
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(30));
                eprintln!("FAIL: WebView2 probe timed out");
                std::process::exit(1);
            });
            Ok(())
        })
        .run(context)
        .expect("Cannot run WebView2 isolation probe");
}
