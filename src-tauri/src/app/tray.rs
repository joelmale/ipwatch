//! System tray: icon, tooltip, and menu — the app's primary interface. Most
//! of the time the main window is closed and this tray is all the user sees.
//!
//! Pure decision logic (which icon to show, how to format the tooltip, and
//! the session "expected country" latch) lives in free functions/types with
//! no Tauri dependency, so it is unit-testable without a running app. Only
//! `init` and the small glue below it touch `tauri::App`/`AppHandle`.

use std::sync::Arc;

use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};
use tokio::sync::broadcast;

use crate::poller::{Poller, Snapshot};

const ICON_OK: &[u8] = include_bytes!("../../icons/tray-ok.png");
const ICON_WARN: &[u8] = include_bytes!("../../icons/tray-warn.png");
const ICON_OFFLINE: &[u8] = include_bytes!("../../icons/tray-offline.png");

/// The main window's label, per `tauri.conf.json` (unlabelled entries default
/// to `"main"`).
const MAIN_WINDOW_LABEL: &str = "main";

/// Which of the three embedded icons the tray should currently show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconKind {
    Ok,
    Warn,
    Offline,
}

impl IconKind {
    /// Offline always wins — a stale last-known-good reading matters more
    /// than a latched country warning. Otherwise a latched warning beats ok.
    fn select(online: bool, warn_latched: bool) -> Self {
        if !online {
            Self::Offline
        } else if warn_latched {
            Self::Warn
        } else {
            Self::Ok
        }
    }
}

/// Tracks the "expected" country for this run of the app and latches the
/// warn state the first time an observation disagrees with it.
///
/// NOTE: a real "expected country" is a persisted user setting, which is
/// Phase 4 work. Until then this treats the first country code observed
/// after launch as the baseline. Once latched, warn stays latched for the
/// rest of the session even if the country reverts — same logic as the
/// poller's own `offline` flag: a session-scoped signal, not a persisted one.
struct SessionBaseline {
    country_code: Option<String>,
    warn_latched: bool,
}

impl SessionBaseline {
    fn new() -> Self {
        Self {
            country_code: None,
            warn_latched: false,
        }
    }

    /// Feeds one observed country code (`None` if the provider didn't report
    /// one this tick) and returns whether warn is latched afterward.
    fn observe(&mut self, country_code: Option<&str>) -> bool {
        if let Some(cc) = country_code {
            match self.country_code.as_deref() {
                None => self.country_code = Some(cc.to_string()),
                Some(baseline) if baseline != cc => self.warn_latched = true,
                _ => {}
            }
        }
        self.warn_latched
    }
}

/// Builds the `"<CC> · <ip>"` tooltip, degrading gracefully when data is
/// missing. Always bounded well under Windows' 127-character tooltip limit —
/// built from a country code and an IP address, never from unbounded
/// provider text.
fn format_tooltip(snapshot: Option<&Snapshot>, online: bool) -> String {
    let Some(snapshot) = snapshot else {
        return "ipwatch — starting…".to_string();
    };

    let mut tooltip = match &snapshot.geo.country_code {
        Some(cc) => format!("{cc} · {}", snapshot.ip),
        None => snapshot.ip.to_string(),
    };

    if !online {
        tooltip.push_str(" (offline)");
    }

    tooltip
}

/// The three embedded icons, decoded once at startup.
struct TrayIcons {
    ok: Image<'static>,
    warn: Image<'static>,
    offline: Image<'static>,
}

impl TrayIcons {
    fn load() -> tauri::Result<Self> {
        Ok(Self {
            ok: Image::from_bytes(ICON_OK)?,
            warn: Image::from_bytes(ICON_WARN)?,
            offline: Image::from_bytes(ICON_OFFLINE)?,
        })
    }

    fn get(&self, kind: IconKind) -> Image<'static> {
        match kind {
            IconKind::Ok => self.ok.clone(),
            IconKind::Warn => self.warn.clone(),
            IconKind::Offline => self.offline.clone(),
        }
    }
}

/// Builds the tray icon + menu, wires the click/menu handlers and
/// close-to-tray behaviour, sets the initial icon/tooltip from
/// `poller.current()`, and spawns the task that keeps both live. Called once
/// from `app::setup`.
pub fn init(app: &tauri::App, poller: Arc<Poller>) -> tauri::Result<()> {
    let icons = TrayIcons::load()?;

    let refresh_item = MenuItemBuilder::with_id("refresh", "Refresh").build(app)?;
    let details_item = MenuItemBuilder::with_id("details", "Details").build(app)?;
    // No settings window exists until Phase 4. Disabled (not omitted) so
    // this reads as "not yet" rather than a dead/broken menu item.
    let settings_item = MenuItemBuilder::with_id("settings", "Settings")
        .enabled(false)
        .build(app)?;
    let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&refresh_item)
        .item(&details_item)
        .item(&settings_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let tray = TrayIconBuilder::new()
        // Matches the "no snapshot yet" tooltip case below; corrected within
        // moments by `spawn_live_updates` priming from `poller.current()`.
        .icon(icons.get(IconKind::Offline))
        .tooltip("ipwatch — starting…")
        .menu(&menu)
        // Left click opens Details (the Windows convention); right click
        // still opens the menu regardless of this setting.
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "refresh" => {
                let poller = app.state::<Arc<Poller>>().inner().clone();
                // The menu handler is sync; refresh_once is async, so this
                // must be spawned rather than awaited in place.
                tauri::async_runtime::spawn(async move {
                    poller.refresh_once().await;
                });
            }
            "details" => show_details_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_details_window(tray.app_handle());
            }
        })
        .build(app)?;

    wire_close_to_tray(app);
    spawn_live_updates(poller, tray, icons);

    Ok(())
}

/// Shows, unminimizes, and focuses the main window. It may be hidden (see
/// `wire_close_to_tray`), so `show()` before `set_focus()`.
fn show_details_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        tracing::error!("main window not found; cannot show Details");
        return;
    };
    if let Err(err) = window.unminimize() {
        tracing::warn!(%err, "failed to unminimize main window");
    }
    if let Err(err) = window.show() {
        tracing::error!(%err, "failed to show main window");
        return;
    }
    if let Err(err) = window.set_focus() {
        tracing::warn!(%err, "failed to focus main window");
    }
}

/// Intercepts the main window's close button: prevents the actual close and
/// hides the window instead, so monitoring keeps running in the background —
/// that is the whole point of a tray app. Deliberately `hide()`, never a
/// destroy/close path: a destroyed window can never be re-shown and Details
/// would silently stop working for the rest of the session.
fn wire_close_to_tray(app: &tauri::App) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        tracing::error!("main window not found; close-to-tray not wired");
        return;
    };
    let window_to_hide = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            if let Err(err) = window_to_hide.hide() {
                tracing::error!(%err, "failed to hide main window on close");
            }
        }
    });
}

/// Primes the tray from whatever the poller already knows, then follows
/// `poller.subscribe()` for the rest of the session, updating icon and
/// tooltip on every `Change`.
///
/// Takes its own subscription (each call to `subscribe()` gets an
/// independent receiver) rather than piggybacking on `spawn_event_bridge`'s,
/// so the tray and the frontend event bridge stay decoupled.
///
/// Online-ness is read fresh from `poller.is_online()` on every change rather
/// than inferred from `change.reason`, for the same reason `get_details`
/// does: recovering to the exact same IP publishes no `Change` at all, so a
/// flag derived purely from the stream would latch offline forever after
/// such a recovery. Note this does mean a silent same-IP recovery won't by
/// itself wake this loop to clear a stale "(offline)" tooltip — the tray
/// only reacts to published `Change`s, the same constraint the frontend's
/// `ip-changed` bridge has.
fn spawn_live_updates(poller: Arc<Poller>, tray: TrayIcon, icons: TrayIcons) {
    tauri::async_runtime::spawn(async move {
        let mut baseline = SessionBaseline::new();
        let mut rx = poller.subscribe();

        let initial = poller.current().await;
        let initial_online = poller.is_online().await;
        apply(&tray, &icons, &mut baseline, initial.as_ref(), initial_online);

        loop {
            match rx.recv().await {
                Ok(change) => {
                    let online = poller.is_online().await;
                    apply(&tray, &icons, &mut baseline, change.current.as_ref(), online);
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // Never exit on Lagged — that would freeze the tray for
                    // the rest of the session. A missed intermediate update
                    // is fine; the next Change corrects the tray fully.
                    tracing::warn!(skipped, "tray event stream lagged; continuing");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::error!(
                        "poller broadcast channel closed; tray will stop updating"
                    );
                    break;
                }
            }
        }
    });
}

/// Updates the baseline latch, then pushes the resulting icon and tooltip to
/// the tray. Failures are logged, not propagated — a tray update failing is
/// never worth tearing down the update loop over.
fn apply(
    tray: &TrayIcon,
    icons: &TrayIcons,
    baseline: &mut SessionBaseline,
    snapshot: Option<&Snapshot>,
    online: bool,
) {
    let warn_latched = baseline.observe(snapshot.and_then(|s| s.geo.country_code.as_deref()));
    let icon_kind = IconKind::select(online, warn_latched);

    if let Err(err) = tray.set_icon(Some(icons.get(icon_kind))) {
        tracing::error!(%err, "failed to update tray icon");
    }

    let tooltip = format_tooltip(snapshot, online);
    if let Err(err) = tray.set_tooltip(Some(tooltip)) {
        tracing::error!(%err, "failed to update tray tooltip");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::GeoInfo;

    fn snapshot(ip: &str, country_code: Option<&str>) -> Snapshot {
        Snapshot {
            ip: ip.parse().unwrap(),
            geo: GeoInfo {
                country_code: country_code.map(String::from),
                ..Default::default()
            },
            observed_at: 0,
        }
    }

    // --- IconKind::select ---

    #[test]
    fn offline_wins_even_when_warn_is_latched() {
        assert_eq!(IconKind::select(false, true), IconKind::Offline);
    }

    #[test]
    fn offline_wins_when_not_latched() {
        assert_eq!(IconKind::select(false, false), IconKind::Offline);
    }

    #[test]
    fn online_and_latched_is_warn() {
        assert_eq!(IconKind::select(true, true), IconKind::Warn);
    }

    #[test]
    fn online_and_not_latched_is_ok() {
        assert_eq!(IconKind::select(true, false), IconKind::Ok);
    }

    // --- SessionBaseline::observe ---

    #[test]
    fn first_country_seen_becomes_baseline_without_latching() {
        let mut baseline = SessionBaseline::new();
        assert!(!baseline.observe(Some("US")));
    }

    #[test]
    fn matching_baseline_does_not_latch() {
        let mut baseline = SessionBaseline::new();
        baseline.observe(Some("US"));
        assert!(!baseline.observe(Some("US")));
    }

    #[test]
    fn diverging_country_latches_warn() {
        let mut baseline = SessionBaseline::new();
        baseline.observe(Some("US"));
        assert!(baseline.observe(Some("FR")));
    }

    #[test]
    fn warn_stays_latched_even_if_country_reverts() {
        let mut baseline = SessionBaseline::new();
        baseline.observe(Some("US"));
        assert!(baseline.observe(Some("FR")));
        assert!(baseline.observe(Some("US")));
    }

    #[test]
    fn missing_country_code_neither_sets_baseline_nor_latches() {
        let mut baseline = SessionBaseline::new();
        assert!(!baseline.observe(None));
        // Still unset, so the next real observation becomes the baseline
        // rather than being compared against nothing.
        assert!(!baseline.observe(Some("US")));
        assert!(!baseline.observe(Some("US")));
    }

    // --- format_tooltip ---

    #[test]
    fn no_snapshot_yet_shows_starting_placeholder() {
        assert_eq!(format_tooltip(None, false), "ipwatch — starting…");
    }

    #[test]
    fn online_with_country_code_formats_country_and_ip() {
        let s = snapshot("173.73.46.80", Some("US"));
        assert_eq!(format_tooltip(Some(&s), true), "US · 173.73.46.80");
    }

    #[test]
    fn online_without_country_code_falls_back_to_ip_only() {
        let s = snapshot("173.73.46.80", None);
        assert_eq!(format_tooltip(Some(&s), true), "173.73.46.80");
    }

    #[test]
    fn offline_appends_staleness_marker() {
        let s = snapshot("173.73.46.80", Some("US"));
        assert_eq!(
            format_tooltip(Some(&s), false),
            "US · 173.73.46.80 (offline)"
        );
    }

    #[test]
    fn offline_without_country_code_still_marks_staleness() {
        let s = snapshot("173.73.46.80", None);
        assert_eq!(format_tooltip(Some(&s), false), "173.73.46.80 (offline)");
    }
}
