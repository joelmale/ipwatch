//! System tray: icon, tooltip, and menu — the app's primary interface. Most
//! of the time the main window is closed and this tray is all the user sees.
//!
//! Pure decision logic (which icon to show, how to format the tooltip, and
//! the session "expected country" latch) lives in free functions/types with
//! no Tauri dependency, so it is unit-testable without a running app. Only
//! `init` and the small glue below it touch `tauri::App`/`AppHandle`.

use std::sync::{Arc, Mutex as StdMutex};

use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};
use tokio::sync::broadcast;

use crate::poller::{Poller, Snapshot};

const ICON_OK: &[u8] = include_bytes!("../../icons/tray-ok.png");
const ICON_WARN: &[u8] = include_bytes!("../../icons/tray-warn.png");
const ICON_OFFLINE: &[u8] = include_bytes!("../../icons/tray-offline.png");
const ICON_UNKNOWN: &[u8] = include_bytes!("../../icons/tray-unknown.png");

/// The main window's label, per `tauri.conf.json` (unlabelled entries default
/// to `"main"`).
const MAIN_WINDOW_LABEL: &str = "main";

/// Which of the four embedded icons the tray should currently show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconKind {
    Ok,
    Warn,
    Offline,
    /// No reading has been obtained yet this session — distinct from
    /// `Offline`, which means a reading exists but is known stale. See
    /// `select`'s doc comment for why the distinction matters.
    Unknown,
}

impl IconKind {
    /// Unknown wins over everything else: with no snapshot at all, "online"
    /// and "warn latched" are not yet meaningful, so `has_snapshot` is
    /// checked first. Otherwise offline wins — a stale last-known-good
    /// reading matters more than a latched country warning. Otherwise a
    /// latched warning beats ok.
    ///
    /// `Unknown` and `Offline` are deliberately distinct states even though
    /// both are visually "the app doesn't have a fresh answer right now":
    /// `Unknown` means "I have never checked", `Offline` means "I checked
    /// and it failed but I still remember the last answer". Conflating them
    /// is misleading in a tool whose whole job is telling you what it knows.
    fn select(has_snapshot: bool, online: bool, warn_latched: bool) -> Self {
        if !has_snapshot {
            Self::Unknown
        } else if !online {
            Self::Offline
        } else if warn_latched {
            Self::Warn
        } else {
            Self::Ok
        }
    }
}

/// Tracks the baseline country for this run of the app and latches the warn
/// state the first time an observation disagrees with it.
///
/// Two baseline modes (PLAN.md Phase 4):
/// - `expected` is `Some`: the persisted `expected_country_code` setting is
///   the baseline from the very first observation. This is the "verify my
///   VPN's exit country stays X" case.
/// - `expected` is `None`: falls back to the pre-Phase-4 behaviour — the
///   first country code observed after launch becomes the baseline.
///
/// Either way, once latched, warn stays latched for the rest of the session
/// even if the country reverts to matching the baseline — same reasoning as
/// the poller's own `offline` flag: a transient drop is exactly the event
/// this exists to surface, and un-latching on a later match would let a
/// blink-and-you-miss-it drop go unreported if the user wasn't watching the
/// tray at that exact moment. A session-scoped signal, not a persisted one —
/// restarting the app always starts fresh.
struct SessionBaseline {
    expected: Option<String>,
    country_code: Option<String>,
    warn_latched: bool,
}

impl SessionBaseline {
    fn new(expected: Option<String>) -> Self {
        Self {
            expected,
            country_code: None,
            warn_latched: false,
        }
    }

    /// Feeds one observed country code (`None` if the provider didn't report
    /// one this tick) and returns whether warn is latched afterward.
    fn observe(&mut self, country_code: Option<&str>) -> bool {
        let Some(cc) = country_code else {
            return self.warn_latched;
        };

        match &self.expected {
            Some(expected) => {
                if expected != cc {
                    self.warn_latched = true;
                }
            }
            None => match self.country_code.as_deref() {
                None => self.country_code = Some(cc.to_string()),
                Some(baseline) if baseline != cc => self.warn_latched = true,
                _ => {}
            },
        }

        self.warn_latched
    }

    /// Re-baselines live, in response to the `expected_country_code` setting
    /// changing mid-session (PLAN.md Phase 4). Unlike `observe`, this
    /// deliberately discards any latch earned under the *old* expectation:
    /// the whole point of changing the setting is to re-judge the current
    /// situation against the new one, not to keep punishing a mismatch that
    /// no longer applies. Concretely, if the user sets the expected country
    /// to the country they are actually in, this must clear the warn state,
    /// not leave it latched.
    ///
    /// Behaves like a fresh `SessionBaseline::new(new_expected)` that
    /// immediately observes `current_country_code` (the latest known
    /// snapshot) once under the new rules — including restoring the
    /// session-first-country fallback when `new_expected` is `None`, since
    /// the fallback baseline field is reset here too and `current_country_code`
    /// becomes its first observation, exactly as at startup.
    ///
    /// Returns the resulting warn-latched state, same as `observe`.
    fn re_baseline(
        &mut self,
        new_expected: Option<String>,
        current_country_code: Option<&str>,
    ) -> bool {
        self.expected = new_expected;
        self.country_code = None;
        self.warn_latched = false;
        self.observe(current_country_code)
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

/// The four embedded icons, decoded once at startup.
struct TrayIcons {
    ok: Image<'static>,
    warn: Image<'static>,
    offline: Image<'static>,
    unknown: Image<'static>,
}

impl TrayIcons {
    fn load() -> tauri::Result<Self> {
        Ok(Self {
            ok: Image::from_bytes(ICON_OK)?,
            warn: Image::from_bytes(ICON_WARN)?,
            offline: Image::from_bytes(ICON_OFFLINE)?,
            unknown: Image::from_bytes(ICON_UNKNOWN)?,
        })
    }

    fn get(&self, kind: IconKind) -> Image<'static> {
        match kind {
            IconKind::Ok => self.ok.clone(),
            IconKind::Warn => self.warn.clone(),
            IconKind::Offline => self.offline.clone(),
            IconKind::Unknown => self.unknown.clone(),
        }
    }
}

/// Shared handle onto the running tray, managed as Tauri state (see
/// `SharedDb`/`SharedSettings` in `app/mod.rs` for the same convention) so
/// the `set_settings` command can apply a change to `expected_country_code`
/// live instead of waiting for the next `Change` off `poller.subscribe()` —
/// which could be a full poll interval away (up to an hour).
///
/// Only `baseline` needs its own lock: `TrayIcon` has its own interior
/// mutability (`set_icon`/`set_tooltip` take `&self`), `TrayIcons` is decoded
/// once at startup and read-only thereafter, and `Poller` is already
/// internally synchronized (see the `poller` module).
pub struct TrayLiveState {
    baseline: StdMutex<SessionBaseline>,
    tray: TrayIcon,
    icons: TrayIcons,
    poller: Arc<Poller>,
}

/// See `TrayLiveState`'s doc comment for why this needs no `Mutex` of its
/// own the way `SharedDb`/`SharedSettings` do.
pub type SharedTray = Arc<TrayLiveState>;

impl TrayLiveState {
    /// Applies a live change to `expected_country_code` (PLAN.md Phase 4):
    /// re-baselines against the poller's *current* snapshot under the new
    /// expectation (see `SessionBaseline::re_baseline`) and immediately
    /// pushes the resulting icon/tooltip to the tray, rather than waiting
    /// for the next `Change` to arrive.
    ///
    /// Called from the `set_settings` command; never called from the
    /// `spawn_live_updates` loop, which uses `observe` (via `apply`)
    /// instead — see `re_baseline`'s doc comment for why the two must not be
    /// conflated.
    pub async fn set_expected_country(&self, expected: Option<String>) {
        let snapshot = self.poller.current().await;
        let online = self.poller.is_online().await;

        let warn_latched = {
            let mut baseline = match self.baseline.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            baseline.re_baseline(
                expected,
                snapshot
                    .as_ref()
                    .and_then(|s| s.geo.country_code.as_deref()),
            )
        };

        push(
            &self.tray,
            &self.icons,
            warn_latched,
            snapshot.as_ref(),
            online,
        );
    }
}

/// Builds the tray icon + menu, wires the click/menu handlers and
/// close-to-tray behaviour, sets the initial icon/tooltip from
/// `poller.current()`, and spawns the task that keeps both live. Called once
/// from `app::setup`.
///
/// `expected_country_code` is the persisted Phase 4 setting, read once at
/// startup and handed to the `SessionBaseline` this call spawns — see that
/// type's doc comment for the two baseline modes. The resulting
/// `TrayLiveState` is also managed as Tauri state (`SharedTray`) so
/// `set_settings` can reach it later; see that type's doc comment.
pub fn init(
    app: &tauri::App,
    poller: Arc<Poller>,
    expected_country_code: Option<String>,
) -> tauri::Result<()> {
    let icons = TrayIcons::load()?;

    let refresh_item = MenuItemBuilder::with_id("refresh", "Refresh").build(app)?;
    let details_item = MenuItemBuilder::with_id("details", "Details").build(app)?;
    // The settings UI lives in the main window's Details panel — this just
    // shows + focuses it, same as the "details" item and the tray left
    // click, via the shared `show_details_window` helper.
    let settings_item = MenuItemBuilder::with_id("settings", "Settings").build(app)?;
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
        .icon(icons.get(IconKind::Unknown))
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
            "settings" => show_details_window(app),
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

    let state: SharedTray = Arc::new(TrayLiveState {
        baseline: StdMutex::new(SessionBaseline::new(expected_country_code)),
        tray,
        icons,
        poller,
    });
    app.manage(state.clone());
    spawn_live_updates(state);

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
fn spawn_live_updates(state: SharedTray) {
    tauri::async_runtime::spawn(async move {
        let mut rx = state.poller.subscribe();

        let initial = state.poller.current().await;
        let initial_online = state.poller.is_online().await;
        apply(&state, initial.as_ref(), initial_online);

        loop {
            match rx.recv().await {
                Ok(change) => {
                    let online = state.poller.is_online().await;
                    apply(&state, change.current.as_ref(), online);
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // Never exit on Lagged — that would freeze the tray for
                    // the rest of the session. A missed intermediate update
                    // is fine; the next Change corrects the tray fully.
                    tracing::warn!(skipped, "tray event stream lagged; continuing");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::error!("poller broadcast channel closed; tray will stop updating");
                    break;
                }
            }
        }
    });
}

/// Feeds one observation into the shared baseline's normal (latching)
/// `observe`, then pushes the resulting icon/tooltip to the tray. Used by
/// `spawn_live_updates` for every `Change` off the poller. Contrast with
/// `TrayLiveState::set_expected_country`, which re-baselines instead of
/// observing — see that method's doc comment.
fn apply(state: &TrayLiveState, snapshot: Option<&Snapshot>, online: bool) {
    let warn_latched = {
        let mut baseline = match state.baseline.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        baseline.observe(snapshot.and_then(|s| s.geo.country_code.as_deref()))
    };
    push(&state.tray, &state.icons, warn_latched, snapshot, online);
}

/// Pushes an already-decided `warn_latched` state, plus `snapshot`/`online`,
/// to the tray as an icon + tooltip update. Failures are logged, not
/// propagated — a tray update failing is never worth tearing down the
/// update loop, or failing the `set_settings` command, over.
fn push(
    tray: &TrayIcon,
    icons: &TrayIcons,
    warn_latched: bool,
    snapshot: Option<&Snapshot>,
    online: bool,
) {
    let icon_kind = IconKind::select(snapshot.is_some(), online, warn_latched);

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
        assert_eq!(IconKind::select(true, false, true), IconKind::Offline);
    }

    #[test]
    fn offline_wins_when_not_latched() {
        assert_eq!(IconKind::select(true, false, false), IconKind::Offline);
    }

    #[test]
    fn online_and_latched_is_warn() {
        assert_eq!(IconKind::select(true, true, true), IconKind::Warn);
    }

    #[test]
    fn online_and_not_latched_is_ok() {
        assert_eq!(IconKind::select(true, true, false), IconKind::Ok);
    }

    #[test]
    fn no_snapshot_is_unknown_even_when_online_and_not_latched() {
        // This is the case that used to render as a misleading green "Ok"
        // icon: the poller has never completed a tick (so `is_online()` is
        // still its default `true`), but there is nothing to report yet.
        assert_eq!(IconKind::select(false, true, false), IconKind::Unknown);
    }

    #[test]
    fn no_snapshot_is_unknown_even_when_warn_would_otherwise_be_latched() {
        assert_eq!(IconKind::select(false, true, true), IconKind::Unknown);
    }

    #[test]
    fn no_snapshot_is_unknown_even_when_reported_offline() {
        // Unknown outranks Offline too: without a snapshot there is no
        // "last known" reading to call stale, so Unknown is the accurate
        // state regardless of what `online` says.
        assert_eq!(IconKind::select(false, false, false), IconKind::Unknown);
        assert_eq!(IconKind::select(false, false, true), IconKind::Unknown);
    }

    // --- SessionBaseline::observe (no expected_country_code: session-first-country fallback) ---

    #[test]
    fn first_country_seen_becomes_baseline_without_latching() {
        let mut baseline = SessionBaseline::new(None);
        assert!(!baseline.observe(Some("US")));
    }

    #[test]
    fn matching_baseline_does_not_latch() {
        let mut baseline = SessionBaseline::new(None);
        baseline.observe(Some("US"));
        assert!(!baseline.observe(Some("US")));
    }

    #[test]
    fn diverging_country_latches_warn() {
        let mut baseline = SessionBaseline::new(None);
        baseline.observe(Some("US"));
        assert!(baseline.observe(Some("FR")));
    }

    #[test]
    fn warn_stays_latched_even_if_country_reverts() {
        let mut baseline = SessionBaseline::new(None);
        baseline.observe(Some("US"));
        assert!(baseline.observe(Some("FR")));
        assert!(baseline.observe(Some("US")));
    }

    #[test]
    fn missing_country_code_neither_sets_baseline_nor_latches() {
        let mut baseline = SessionBaseline::new(None);
        assert!(!baseline.observe(None));
        // Still unset, so the next real observation becomes the baseline
        // rather than being compared against nothing.
        assert!(!baseline.observe(Some("US")));
        assert!(!baseline.observe(Some("US")));
    }

    // --- SessionBaseline::observe (expected_country_code set: Phase 4) ---

    #[test]
    fn matching_expected_country_does_not_latch() {
        let mut baseline = SessionBaseline::new(Some("US".to_string()));
        assert!(!baseline.observe(Some("US")));
    }

    #[test]
    fn diverging_from_expected_country_latches_immediately() {
        // Unlike the fallback mode, the very first observation can latch —
        // there's no "first one sets the baseline" step when a baseline was
        // already given.
        let mut baseline = SessionBaseline::new(Some("US".to_string()));
        assert!(baseline.observe(Some("FR")));
    }

    #[test]
    fn expected_country_warn_stays_latched_even_if_it_reverts() {
        let mut baseline = SessionBaseline::new(Some("US".to_string()));
        assert!(baseline.observe(Some("FR")));
        assert!(baseline.observe(Some("US")));
    }

    #[test]
    fn missing_country_code_does_not_latch_against_an_expected_baseline() {
        let mut baseline = SessionBaseline::new(Some("US".to_string()));
        assert!(!baseline.observe(None));
    }

    // --- SessionBaseline::re_baseline (live setting change, PLAN.md Phase 4) ---

    #[test]
    fn re_baseline_to_the_country_already_observed_clears_a_latched_warn() {
        // This is the entire point of the setting: latch under the old
        // expectation, then set the expectation to the country the user is
        // actually in, and the tray must clear to green.
        let mut baseline = SessionBaseline::new(Some("US".to_string()));
        assert!(baseline.observe(Some("FR")));

        assert!(!baseline.re_baseline(Some("FR".to_string()), Some("FR")));
    }

    #[test]
    fn re_baseline_to_a_still_different_country_stays_latched() {
        let mut baseline = SessionBaseline::new(Some("US".to_string()));
        assert!(baseline.observe(Some("FR")));

        assert!(baseline.re_baseline(Some("NL".to_string()), Some("FR")));
    }

    #[test]
    fn re_baseline_to_none_restores_session_first_country_fallback() {
        // Latch under an expected baseline, then clear the setting back to
        // None: must behave like a fresh start under fallback mode, not
        // stay latched and not treat the current country as a mismatch.
        let mut baseline = SessionBaseline::new(Some("US".to_string()));
        assert!(baseline.observe(Some("FR")));

        assert!(!baseline.re_baseline(None, Some("FR")));
        // The re-baselined fallback mode should behave exactly like a fresh
        // `SessionBaseline::new(None)` that just observed "FR" for the first
        // time: FR is now the fallback baseline, so observing FR again does
        // not latch...
        assert!(!baseline.observe(Some("FR")));
        // ...but a genuine divergence still does.
        assert!(baseline.observe(Some("DE")));
    }

    #[test]
    fn re_baseline_with_no_current_country_code_does_not_latch() {
        // If the poller has no snapshot yet (or no country code in it) at
        // the moment the setting changes, there is nothing to compare
        // against — must not spuriously latch.
        let mut baseline = SessionBaseline::new(None);
        assert!(!baseline.re_baseline(Some("US".to_string()), None));
    }

    #[test]
    fn re_baseline_from_none_to_expected_latches_on_mismatch() {
        let mut baseline = SessionBaseline::new(None);
        baseline.observe(Some("US")); // sets fallback baseline to US, unlatched

        assert!(baseline.re_baseline(Some("FR".to_string()), Some("US")));
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
