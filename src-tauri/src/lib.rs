//! ipwatch — monitors external IP, geolocation, and DNS to verify VPN status.
//!
//! All network access lives here in Rust; the webview never makes requests, so
//! the CSP can stay strict. The backend talks to the frontend over Tauri events
//! (`ip-changed`, `refresh-started`, `refresh-done`) and receives commands back.

pub mod db;
pub mod netinfo;
pub mod poller;
pub mod providers;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![greet])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
