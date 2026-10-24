#[tauri::command]
fn client_platform() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else {
        "ios"
    }
}

/// Disconnected shell until maintainer S2 enrollment and S4 TLS land.
/// No discovery, token import, local engine or credential storage fallback.
#[tauri::mobile_entry_point]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![client_platform])
        .run(tauri::generate_context!())
        .expect("failed to build the QQ mobile app");
}
