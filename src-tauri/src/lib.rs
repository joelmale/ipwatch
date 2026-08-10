//! ipwatch — monitors external IP, geolocation, and DNS to verify VPN status.
//!
//! All network access lives here in Rust; the webview never makes requests, so
//! the CSP can stay strict. The backend talks to the frontend over Tauri events
//! (`ip-changed`, `refresh-started`, `refresh-done`) and receives commands back.

pub mod app;
pub mod db;
pub mod dnsleak;
pub mod netinfo;
pub mod poller;
pub mod providers;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init());
    let builder = app::tiles::register(builder);
    builder
        .setup(app::setup)
        .invoke_handler(tauri::generate_handler![
            app::get_details,
            app::refresh,
            app::get_history,
            app::run_dns_leak_test
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
