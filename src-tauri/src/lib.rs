use tauri::Manager;

#[tauri::command]
fn colab_path() -> Option<String> {
    colab_comfy_core::find_colab().map(|path| path.display().to_string())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .invoke_handler(tauri::generate_handler![colab_path])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
