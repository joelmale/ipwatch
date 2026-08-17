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
pub mod settings;

/// Installs the `tracing` subscriber.
///
/// Without this, every `tracing::warn!`/`error!` in the crate is discarded —
/// including the whole degradation story: an unopenable database, a corrupt
/// settings file falling back to defaults, provider chains failing over, a
/// toast that never fired. Those paths deliberately keep the app running
/// instead of crashing, which is only defensible if the reason is recorded
/// somewhere. Silent degradation is indistinguishable from a bug.
///
/// `RUST_LOG` overrides the default filter. `try_init` rather than `init` so a
/// second call (tests, a re-entrant mobile entry point) cannot panic.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("ipwatch=info,ipwatch_lib=info,warn"));

    let _ = fmt().with_env_filter(filter).try_init();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

    let mut builder = tauri::Builder::default();

    // Registered first, before every other plugin below (tauri_plugin_opener,
    // notification, autostart) and before app::tiles::register. Per the
    // plugin's own docs (https://v2.tauri.app/plugin/single-instance/):
    // "The Single Instance plugin must be the first one to be registered to
    // work well. This assures that it runs before other plugins can
    // interfere." Concretely: when a second instance launches, this plugin
    // must detect the already-running instance and exit the second process
    // before any other plugin — or app::setup — gets a chance to start a
    // second poller, open a second SQLite writer, or spawn a second tray
    // icon (PLAN.md brief 6.1). Getting this order wrong does not fail to
    // compile.
    //
    // Desktop-only: gated both here with `#[cfg(desktop)]` (the alias Tauri's
    // build script defines) and at the Cargo-dependency level in Cargo.toml,
    // since the crate has no mobile equivalent.
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(
            app::on_second_instance,
        ));
    }

    let builder = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ));
    let builder = app::tiles::register(builder);
    builder
        .setup(app::setup)
        .invoke_handler(tauri::generate_handler![
            app::get_details,
            app::refresh,
            app::get_history,
            app::run_dns_leak_test,
            app::get_settings,
            app::set_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
