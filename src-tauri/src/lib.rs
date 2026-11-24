use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use colab_comfy_core::colab_cli::StopReport;
use colab_comfy_core::config::GpuTier;
use colab_comfy_core::locate::{find_colab, OPEN_BIN};
use colab_comfy_core::webview_policy::{
    is_allowed_canvas_navigation, is_allowed_external_url, resolve_safe_download_destination,
};
use colab_comfy_core::{AppSnapshot, SessionManager, UiError};
use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};

struct AppState {
    manager: Arc<SessionManager>,
    allow_immediate_exit: Arc<AtomicBool>,
    home_dir: Option<PathBuf>,
    data_dir: PathBuf,
}

#[cfg(target_os = "macos")]
mod app_nap {
    use std::cell::RefCell;

    use objc2::rc::Retained;
    use objc2::runtime::{NSObjectProtocol, ProtocolObject};
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

    thread_local! {
        static ACTIVITY_TOKEN: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
            const { RefCell::new(None) };
    }

    pub fn set_active(app: &tauri::AppHandle, active: bool) {
        let _ = app.run_on_main_thread(move || {
            ACTIVITY_TOKEN.with(|slot| {
                let mut guard = slot.borrow_mut();
                let info = NSProcessInfo::processInfo();
                if active && guard.is_none() {
                    let reason = NSString::from_str("Colab ComfyUI active GPU session");
                    let token = info.beginActivityWithOptions_reason(
                        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
                        &reason,
                    );
                    *guard = Some(token);
                } else if !active {
                    if let Some(token) = guard.take() {
                        unsafe {
                            info.endActivity(&token);
                        }
                    }
                }
            });
        });
    }
}

#[cfg(not(target_os = "macos"))]
mod app_nap {
    pub fn set_active(_app: &tauri::AppHandle, _active: bool) {}
}

fn open_external_url_in_os(url: &str) -> Result<(), UiError> {
    if !is_allowed_external_url(url) {
        return Err(UiError {
            code: "INVALID_URL".to_string(),
            message: format!("拒绝打开非 http/https 链接：{url}"),
        });
    }
    std::process::Command::new(OPEN_BIN)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|err| UiError {
            code: "OPEN_FAILED".to_string(),
            message: format!("打开浏览器失败：{err}"),
        })?;
    Ok(())
}

#[tauri::command]
fn colab_path(state: tauri::State<'_, AppState>) -> Option<String> {
    find_colab(
        std::env::var_os("PATH").as_deref(),
        state.home_dir.as_deref(),
    )
    .map(|path| path.display().to_string())
}

#[tauri::command]
async fn get_snapshot(state: tauri::State<'_, AppState>) -> Result<AppSnapshot, UiError> {
    Ok(state.manager.snapshot().await)
}

#[tauri::command]
async fn reconcile_startup(state: tauri::State<'_, AppState>) -> Result<AppSnapshot, UiError> {
    Ok(state.manager.reconcile_on_startup().await)
}

#[tauri::command]
async fn refresh_status(state: tauri::State<'_, AppState>) -> Result<AppSnapshot, UiError> {
    Ok(state.manager.refresh_status().await)
}

#[tauri::command]
async fn start_session(
    state: tauri::State<'_, AppState>,
    gpu: GpuTier,
    high_mem: bool,
    allow_zero_balance: bool,
) -> Result<AppSnapshot, UiError> {
    state
        .manager
        .start_session(gpu, high_mem, allow_zero_balance)
        .await
        .map_err(|e| UiError::from(&e))
}

#[tauri::command]
async fn cancel_start(state: tauri::State<'_, AppState>) -> Result<AppSnapshot, UiError> {
    Ok(state.manager.cancel_start().await)
}

#[tauri::command]
async fn reconnect_tunnel(state: tauri::State<'_, AppState>) -> Result<AppSnapshot, UiError> {
    state
        .manager
        .reconnect_tunnel()
        .await
        .map_err(|e| UiError::from(&e))
}

#[tauri::command]
async fn stop_session(state: tauri::State<'_, AppState>) -> Result<StopReport, UiError> {
    state
        .manager
        .stop_session()
        .await
        .map_err(|e| UiError::from(&e))
}

fn build_canvas_window(
    app: &tauri::AppHandle,
    url_str: &str,
    local_port: u16,
    downloads_dir: PathBuf,
) -> Result<(), UiError> {
    if let Some(existing) = app.get_webview_window("canvas") {
        let _ = existing.show();
        let _ = existing.unminimize();
        let _ = existing.set_focus();
        return Ok(());
    }

    let parsed: tauri::Url = url_str.parse().map_err(|err| UiError {
        code: "INVALID_URL".to_string(),
        message: format!("无效画布地址：{err}"),
    })?;

    let _ = std::fs::create_dir_all(&downloads_dir);

    WebviewWindowBuilder::new(app, "canvas", WebviewUrl::External(parsed))
        .title("ComfyUI Canvas")
        .inner_size(1280.0, 860.0)
        .min_inner_size(800.0, 600.0)
        .disable_drag_drop_handler()
        .on_navigation(move |nav_url| {
            let url_str = nav_url.as_str();
            if is_allowed_canvas_navigation(url_str, local_port) {
                return true;
            }
            if is_allowed_external_url(url_str) {
                let _ = open_external_url_in_os(url_str);
            }
            false
        })
        .on_new_window(move |new_url, _features| {
            let _ = open_external_url_in_os(new_url.as_str());
            NewWindowResponse::Deny
        })
        .on_download(move |_webview, event| {
            if let DownloadEvent::Requested { destination, .. } = event {
                *destination = resolve_safe_download_destination(&downloads_dir, destination);
            }
            true
        })
        .build()
        .map_err(|err| UiError {
            code: "WINDOW_BUILD_FAILED".to_string(),
            message: format!("创建画布窗口失败：{err}"),
        })?;

    Ok(())
}

#[tauri::command]
async fn open_canvas_window(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), UiError> {
    let snap = state.manager.snapshot().await;
    let url_str = snap.canvas_url.ok_or_else(|| UiError {
        code: "TUNNEL_NOT_READY".to_string(),
        message: "本地代理隧道尚未就绪，请先启动或重连隧道。".to_string(),
    })?;
    let local_port = snap
        .active_session
        .as_ref()
        .map(|s| s.local_port)
        .unwrap_or(colab_comfy_core::config::LOCAL_PROXY_PORT);
    let downloads_dir = app
        .path()
        .download_dir()
        .unwrap_or_else(|_| state.data_dir.join("downloads"));

    build_canvas_window(&app, &url_str, local_port, downloads_dir)
}

#[tauri::command]
async fn open_colab_url(state: tauri::State<'_, AppState>) -> Result<(), UiError> {
    state
        .manager
        .open_session_in_browser()
        .await
        .map_err(|e| UiError::from(&e))
}

#[tauri::command]
async fn open_colab_pay(state: tauri::State<'_, AppState>) -> Result<(), UiError> {
    state
        .manager
        .open_colab_pay()
        .await
        .map_err(|e| UiError::from(&e))
}

#[tauri::command]
fn open_colab_manage_sessions() -> Result<(), UiError> {
    open_external_url_in_os("https://colab.research.google.com/")
}

#[tauri::command]
async fn stop_and_exit(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), UiError> {
    if state.manager.has_running_or_pending_session().await {
        let report = state
            .manager
            .stop_session()
            .await
            .map_err(|e| UiError::from(&e))?;
        if !(report.terminated_sentinel && report.endpoint_absent && report.owner_verified) {
            return Err(UiError {
                code: "STOP_UNVERIFIED".to_string(),
                message: report.summary,
            });
        }
    }
    state.allow_immediate_exit.store(true, Ordering::SeqCst);
    app_nap::set_active(&app, false);
    app.exit(0);
    Ok(())
}

#[tauri::command]
async fn leave_running_and_exit(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), UiError> {
    state.manager.disconnect_local_only_for_exit().await;
    state.allow_immediate_exit.store(true, Ordering::SeqCst);
    app_nap::set_active(&app, false);
    app.exit(0);
    Ok(())
}

fn show_main_for_exit_confirm(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
    let _ = app.emit("exit-confirm-requested", ());
}

pub fn run() {
    let home_dir = std::env::var_os("HOME").map(PathBuf::from);
    let data_dir = home_dir
        .as_ref()
        .map(|h| h.join(".colab-comfyui"))
        .unwrap_or_else(|| PathBuf::from("/tmp/colab-comfyui"));
    let path_env: Option<OsString> = std::env::var_os("PATH");
    let https_proxy = std::env::var_os("HTTPS_PROXY").or_else(|| std::env::var_os("https_proxy"));
    let http_proxy = std::env::var_os("HTTP_PROXY").or_else(|| std::env::var_os("http_proxy"));

    let manager = Arc::new(
        SessionManager::new(data_dir.clone(), home_dir.clone(), path_env)
            .with_proxy_hints(https_proxy, http_proxy),
    );
    let allow_immediate_exit = Arc::new(AtomicBool::new(false));

    let app = tauri::Builder::default()
        .manage(AppState {
            manager: manager.clone(),
            allow_immediate_exit: allow_immediate_exit.clone(),
            home_dir,
            data_dir,
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .setup(move |app| {
            let handle = app.handle().clone();
            let mgr = manager.clone();
            let mut rx = mgr.subscribe();
            tauri::async_runtime::spawn(async move {
                let initial_active = mgr.has_running_or_pending_session().await;
                app_nap::set_active(&handle, initial_active);
                while rx.changed().await.is_ok() {
                    let snap = mgr.snapshot().await;
                    let active = snap.active_session.is_some() || snap.pending_session.is_some();
                    app_nap::set_active(&handle, active);
                    let _ = handle.emit("snapshot-updated", snap);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            colab_path,
            get_snapshot,
            reconcile_startup,
            refresh_status,
            start_session,
            cancel_start,
            reconnect_tunnel,
            stop_session,
            open_canvas_window,
            open_colab_url,
            open_colab_pay,
            open_colab_manage_sessions,
            stop_and_exit,
            leave_running_and_exit,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(move |app_handle, event| match event {
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::CloseRequested { api, .. },
            ..
        } if label == "main" => {
            let state = app_handle.state::<AppState>();
            if !state.allow_immediate_exit.load(Ordering::SeqCst)
                && state.manager.has_running_or_pending_session_sync()
            {
                api.prevent_close();
                show_main_for_exit_confirm(app_handle);
            }
        }
        RunEvent::ExitRequested { api, .. } => {
            let state = app_handle.state::<AppState>();
            if !state.allow_immediate_exit.load(Ordering::SeqCst)
                && state.manager.has_running_or_pending_session_sync()
            {
                api.prevent_exit();
                show_main_for_exit_confirm(app_handle);
            }
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => {
            if let Some(window) = app_handle.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    #[test]
    fn capabilities_never_grant_canvas_or_remote_permissions() {
        let cap_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        let entries = fs::read_dir(&cap_dir).expect("capabilities dir must exist");
        let mut saw_default = false;
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = fs::read_to_string(&path).unwrap();
            let lower = raw.to_ascii_lowercase();
            assert!(
                !lower.contains("\"canvas\""),
                "capability file {} must never reference canvas",
                path.display()
            );
            assert!(
                !lower.contains("\"remote\""),
                "capability file {} must never grant remote capability",
                path.display()
            );

            let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
            if path.file_name().and_then(|n| n.to_str()) == Some("default.json") {
                saw_default = true;
                let windows = parsed["windows"]
                    .as_array()
                    .expect("default.json must have windows array");
                assert_eq!(
                    windows,
                    &vec![serde_json::Value::String("main".to_string())]
                );
            }
        }
        assert!(saw_default, "capabilities/default.json must exist");
    }
}
