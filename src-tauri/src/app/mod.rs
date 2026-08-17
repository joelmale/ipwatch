//! Tauri glue: wires the decoupled `Poller`/`Db` core into app state, bridges
//! `Poller::subscribe()` to frontend events, and exposes the invoke commands.
//!
//! Deliberately kept separate from `poller`/`db`/`providers`/`netinfo` — those
//! modules stay Tauri-free and unit-testable on their own; this module is the
//! only place that knows about `tauri::App`/`AppHandle`.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use reqwest::Client;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt;
use tokio::sync::{broadcast, Mutex as AsyncMutex};

use crate::db::{ChangeReason, Db, IpEvent};
use crate::dnsleak::{self, BashWs, LeakReport};
use crate::netinfo::{self, NetInfo};
use crate::poller::{Change, Poller, Snapshot};
use crate::providers::{default_geo_chain, default_ip_chain, http_client, GeoInfo};
use crate::settings::{self, Settings};

pub mod notify;
pub mod tiles;
pub mod tray;

/// `rusqlite::Connection` (inside `Db`) is `Send` but not `Sync`, so a bare
/// `Db` cannot go into Tauri managed state — state must be `Send + Sync`.
/// `Option` wraps it so a database that fails to open at startup degrades to
/// "no history" instead of taking the whole app down (see `open_db`).
///
/// A std `Mutex` is fine here: every access is a quick synchronous SQLite
/// call, contention is nil at a 60s poll interval, and no code path holds the
/// guard across an `.await`.
pub type SharedDb = Arc<StdMutex<Option<Db>>>;

/// `Settings` is plain data (no non-`Sync` handle inside, unlike `Db`), so it
/// needs no `Option` wrapper the way `SharedDb` does — `open_settings` always
/// produces a usable value, falling back to `Settings::default()` rather than
/// `None` on any failure (see `settings::load`).
pub type SharedSettings = Arc<StdMutex<Settings>>;

/// Guards `run_dns_leak_test` against overlapping invocations.
///
/// A `tokio::sync::Mutex` rather than the std one: the guard is held across
/// `.await` points for the several-second duration of a test, which a std
/// mutex cannot do soundly. See `run_dns_leak_test` for why a second
/// concurrent call is rejected outright rather than queued.
pub type DnsLeakGuard = Arc<AsyncMutex<()>>;

/// Response payload for the `get_details` command.
#[derive(Debug, Clone, Serialize)]
pub struct Details {
    pub snapshot: Option<Snapshot>,
    pub netinfo: NetInfo,
    pub online: bool,
}

/// Builds the DB + Poller, manages both as Tauri state, and starts the poll
/// loop plus the event bridge. Called from `Builder::setup`.
///
/// Never fails the app on a fresh machine: a DB that can't be created or
/// opened is logged and the app runs with monitoring but no history, rather
/// than panicking or returning `Err` (which would abort startup).
pub fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();

    let db = open_db(&handle);
    let shared_db: SharedDb = Arc::new(StdMutex::new(db));
    app.manage(shared_db.clone());

    // Loaded (and, if necessary, defaulted/clamped — see `settings::load`)
    // before the poller is built, so the very first `Poller` the app
    // constructs already reflects a persisted poll interval instead of
    // always starting at `DEFAULT_INTERVAL` and needing a follow-up
    // `set_interval` call.
    let settings = open_settings(&handle);
    let initial_interval = Duration::from_secs(settings.poll_interval_secs);
    let expected_country_code = settings.expected_country_code.clone();
    let launch_at_startup = settings.launch_at_startup;
    let start_minimised = settings.start_minimised;
    let shared_settings: SharedSettings = Arc::new(StdMutex::new(settings));
    app.manage(shared_settings.clone());

    // Applied every startup, not just when the setting changes from the UI —
    // otherwise the OS's actual registered-at-login state and the setting
    // the user sees could silently diverge (e.g. after a manual uninstall/
    // reinstall of the autostart entry, or a settings.json restored from
    // backup).
    apply_autostart(&handle, launch_at_startup);

    // `tauri.conf.json`'s main window is configured `"visible": false` so it
    // never paints on screen to begin with; this is the one deliberate place
    // that flips it to visible, unless the user opted into staying
    // minimised (PLAN.md brief 6.2). Hidden-by-config-then-shown is required
    // here, not show-then-hide: the latter paints the window for a frame
    // before hiding it again, which is exactly the startup flash this brief
    // exists to eliminate. Does not gate anything else below — the poller,
    // tray, event bridge, and tile cache prune all start unconditionally
    // regardless of whether this ever shows the window.
    show_main_window_unless_minimised(&handle, start_minimised);

    // Age/size-bounded, and run on its own background task so a slow or
    // failing prune can never delay startup or block a concurrent tile
    // request — see `tiles::spawn_cache_prune` for the limits and safety
    // rules.
    tiles::spawn_cache_prune(handle.clone());

    let client = http_client().unwrap_or_else(|err| {
        tracing::error!(%err, "failed to build the configured http client; falling back to a default reqwest client");
        Client::new()
    });
    let poller = Arc::new(Poller::with_interval(
        default_ip_chain(),
        default_geo_chain(),
        client,
        initial_interval,
    ));
    app.manage(poller.clone());

    let dns_leak_guard: DnsLeakGuard = Arc::new(AsyncMutex::new(()));
    app.manage(dns_leak_guard);

    // Carry the last persisted reading across the restart, so an unchanged IP
    // classifies as no-change instead of `Initial` and does not add a
    // duplicate row on every launch. Read synchronously here, applied inside
    // the task below so it lands before the first tick.
    let seed_snapshot = last_snapshot(&shared_db);

    // The refresh loop goes onto Tauri's runtime, not via `Poller::spawn`:
    // `setup` runs on the main thread with no ambient Tokio runtime, so the
    // bare `tokio::spawn` inside `spawn()` panics with "there is no reactor
    // running". The loop body itself still lives in poller/mod.rs, which
    // stays Tauri-free by design.
    let poll_handle = poller.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(snapshot) = seed_snapshot {
            poll_handle.seed(snapshot).await;
        }
        poll_handle.run().await;
    });

    tray::init(app, poller.clone(), expected_country_code)?;

    spawn_event_bridge(handle, poller, shared_db, shared_settings);

    Ok(())
}

/// Invoked by `tauri-plugin-single-instance` (registered first in the
/// builder chain in `lib.rs` — see that registration's doc comment for why
/// order matters) when a second instance is launched while this one is
/// already running. Brings the existing main window to the front instead of
/// letting a second process start, which would double the poll rate against
/// ip-api.com, open a second SQLite writer on the same file, and spawn a
/// second tray icon (PLAN.md brief 6.1).
///
/// `argv`/`cwd` describe the second instance's command line and working
/// directory. Both are ignored today — a future `--silent` flag (PLAN.md
/// brief 6.2) would be read out of `argv` here.
///
/// Deliberately does only window plumbing: never starts a poller, opens a DB
/// handle, or touches settings. Those are already running in this — the
/// first — process, and the second process exits right after this callback
/// returns.
#[cfg(desktop)]
pub fn on_second_instance(app: &AppHandle, _argv: Vec<String>, _cwd: String) {
    // Delegates rather than reimplementing: the tray's Details item wants the
    // same "bring the existing window to the front" behaviour, and duplicating
    // it would mean two copies of the window label and two orderings of
    // unminimize/show/focus that could silently diverge.
    tray::show_details_window(app);
}

/// Rebuilds a `Snapshot` from the newest persisted `ip_events` row, for
/// seeding change detection across a restart.
///
/// Necessarily partial: `ip_events` stores country, country code and ISP but
/// not city, coordinates, timezone or org. That is sufficient, because those
/// three fields are exactly what `classify` compares, and the poller replaces
/// the whole snapshot on its first live tick.
///
/// Returns `None` when there is no history, the database is unavailable, or
/// the stored address will not parse — all of which simply mean "no baseline",
/// leaving the first reading to classify as `Initial` as it did before.
fn last_snapshot(db: &SharedDb) -> Option<Snapshot> {
    let guard = match db.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    let event = match guard.as_ref()?.latest_event() {
        Ok(event) => event?,
        Err(err) => {
            tracing::warn!(%err, "could not read the last ip event; starting without a baseline");
            return None;
        }
    };

    let ip = match event.external_ip.parse() {
        Ok(ip) => ip,
        Err(err) => {
            tracing::warn!(
                %err,
                stored = %event.external_ip,
                "stored external ip will not parse; starting without a baseline"
            );
            return None;
        }
    };

    Some(Snapshot {
        ip,
        geo: GeoInfo {
            ip: Some(ip),
            country: event.country,
            country_code: event.country_code,
            isp: event.isp,
            ..GeoInfo::default()
        },
        observed_at: event.ts,
    })
}

/// Resolves `app_data_dir()`, creates it if this is a fresh install, and
/// opens `ipwatch.db` inside it. Any failure along the way is logged and
/// answered with `None` rather than propagated — see `setup`'s doc comment.
fn open_db(handle: &AppHandle) -> Option<Db> {
    let app_data_dir = match handle.path().app_data_dir() {
        Ok(dir) => dir,
        Err(err) => {
            tracing::error!(%err, "could not resolve app data dir; ip history will be unavailable this session");
            return None;
        }
    };

    if let Err(err) = std::fs::create_dir_all(&app_data_dir) {
        tracing::error!(
            %err,
            dir = %app_data_dir.display(),
            "could not create app data dir; ip history will be unavailable this session"
        );
        return None;
    }

    let db_path = app_data_dir.join("ipwatch.db");
    match Db::open(&db_path) {
        Ok(db) => Some(db),
        Err(err) => {
            tracing::error!(
                %err,
                path = %db_path.display(),
                "could not open database; ip history will be unavailable this session"
            );
            None
        }
    }
}

/// Resolves where `settings.json` lives (`app_data_dir()/settings.json`),
/// creating the directory if this is a fresh install. `None` only when the
/// app data dir itself can't be resolved or created — logged, and treated by
/// every caller the same way `open_db` treats a database it couldn't open:
/// degrade, don't fail startup.
fn settings_file_path(handle: &AppHandle) -> Option<std::path::PathBuf> {
    let app_data_dir = match handle.path().app_data_dir() {
        Ok(dir) => dir,
        Err(err) => {
            tracing::error!(%err, "could not resolve app data dir; settings will not persist this session");
            return None;
        }
    };

    if let Err(err) = std::fs::create_dir_all(&app_data_dir) {
        tracing::error!(
            %err,
            dir = %app_data_dir.display(),
            "could not create app data dir; settings will not persist this session"
        );
        return None;
    }

    Some(app_data_dir.join("settings.json"))
}

/// Loads settings for this session, falling back to `Settings::default()`
/// (never failing startup) both when `settings_file_path` can't resolve a
/// path and — inside `settings::load` itself — when the file is missing,
/// unreadable, or corrupt. See the `settings` module doc comment for why
/// this is a plain JSON file rather than `tauri-plugin-store`.
fn open_settings(handle: &AppHandle) -> Settings {
    match settings_file_path(handle) {
        Some(path) => settings::load(path),
        None => Settings::default(),
    }
}

/// Enables or disables the OS "launch at startup" registration to match
/// `enabled`. Failures are logged, not propagated — the same policy as every
/// other best-effort side effect in this module (tray/notification updates):
/// a platform quirk here must not be able to take down setup or a
/// `set_settings` call.
fn apply_autostart(app: &AppHandle, enabled: bool) {
    let manager = app.autolaunch();

    // Only act when the OS state actually differs. `disable()` on an entry
    // that was never registered fails with "the system cannot find the file
    // specified", so calling it unconditionally at every startup logged an
    // error on every launch for the default (disabled) case — noise that
    // would eventually mask a real failure here.
    match manager.is_enabled() {
        Ok(current) if current == enabled => return,
        Ok(_) => {}
        Err(err) => {
            // Could not read the current state; fall through and try to apply
            // the desired one anyway rather than silently skipping it.
            tracing::debug!(%err, "could not read launch-at-startup state; applying anyway");
        }
    }

    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    if let Err(err) = result {
        tracing::error!(%err, enabled, "failed to apply launch-at-startup setting");
    }
}

/// Shows the main window unless `start_minimised` is `true` (PLAN.md brief
/// 6.2). See the call site in `setup` for why the window must be configured
/// hidden (`tauri.conf.json`) and shown deliberately here, rather than shown
/// then hidden.
///
/// Failures are logged, not propagated — the same "never fail startup over a
/// best-effort side effect" policy as `apply_autostart`. A window that fails
/// to show still exists and can be reached later via the tray's Details item
/// (`tray::show_details_window`), which does its own `show()`/`set_focus()`
/// and does not depend on this call having succeeded.
fn show_main_window_unless_minimised(handle: &AppHandle, start_minimised: bool) {
    if start_minimised {
        return;
    }

    match handle.get_webview_window(tray::MAIN_WINDOW_LABEL) {
        Some(window) => {
            if let Err(err) = window.show() {
                tracing::error!(%err, "failed to show main window at startup");
            } else if let Err(err) = window.set_focus() {
                // Non-fatal: the window is visible either way, just possibly
                // not focused. Mirrors `tray::show_details_window`'s
                // treatment of a failed `set_focus`.
                tracing::warn!(%err, "failed to focus main window at startup");
            }
        }
        None => tracing::error!("main window not found at startup; cannot show it"),
    }
}

/// Subscribes to `poller.subscribe()` and, for each `Change`: persists a row
/// (when warranted, see `persist_if_warranted`) and emits `ip-changed` to the
/// frontend.
///
/// Online-ness is deliberately NOT tracked here. It is read straight off
/// `Poller::is_online`, because recovering to the same IP publishes no
/// `Change` at all — a flag maintained from this stream would latch offline
/// forever after such a recovery.
///
/// Runs for the lifetime of the app. A lagged receiver logs and keeps going —
/// exiting on `Lagged` would silently stop all UI updates for the rest of the
/// session, which is worse than missing a few intermediate change events.
fn spawn_event_bridge(app: AppHandle, poller: Arc<Poller>, db: SharedDb, settings: SharedSettings) {
    tauri::async_runtime::spawn(async move {
        let mut rx = poller.subscribe();
        loop {
            match rx.recv().await {
                Ok(change) => {
                    persist_if_warranted(&db, &change);
                    if let Err(err) = app.emit("ip-changed", &change) {
                        tracing::error!(%err, "failed to emit ip-changed event");
                    }
                    // The on/off toggle is gated here at the call site, not
                    // inside `notify::should_notify` — that function stays a
                    // pure, settings-free predicate over `ChangeReason` alone,
                    // so it's trivially unit-testable without any Tauri state.
                    // Never allowed to affect monitoring: notify::notify_change
                    // logs and swallows its own errors internally.
                    if notifications_enabled(&settings) {
                        notify::notify_change(&app, &change);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        "event bridge lagged behind the poller; some Change events were dropped"
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::error!(
                        "poller broadcast channel closed; event bridge is exiting, UI will stop updating"
                    );
                    break;
                }
            }
        }
    });
}

/// Reads the current `notifications_enabled` flag out of `SharedSettings`.
/// A poisoned lock (only possible if some other holder panicked while
/// holding it) still yields a usable value rather than propagating — a
/// stuck notification gate must never be able to stall the event bridge.
fn notifications_enabled(settings: &SharedSettings) -> bool {
    let guard = match settings.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.notifications_enabled
}

/// Decides whether a `Change` is worth a row in `ip_events`.
///
/// Policy: `ChangeReason::Offline` changes are never persisted, even though
/// they are always emitted to the frontend (staleness must show in the UI).
/// The `current` snapshot on an Offline change is not a fresh observation —
/// `Poller::handle_failure` republishes the last known-good snapshot
/// unchanged. Inserting it would add a row whose `ts` claims a new
/// observation happened when in fact nothing was observed that tick,
/// corrupting the meaning of the history log (which otherwise records only
/// actual, successful lookups). The poller already suppresses repeat Offline
/// publishes, so this only skips the single transition event, not a stream of
/// them.
fn persist_if_warranted(db: &SharedDb, change: &Change) {
    if change.reason == ChangeReason::Offline {
        return;
    }

    let Some(current) = &change.current else {
        return;
    };

    let event = IpEvent {
        id: None,
        ts: current.observed_at,
        external_ip: current.ip.to_string(),
        country: current.geo.country.clone(),
        country_code: current.geo.country_code.clone(),
        isp: current.geo.isp.clone(),
        change_reason: change.reason,
    };

    let guard = match db.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.as_ref() {
        Some(db) => {
            if let Err(err) = db.insert_event(&event) {
                tracing::error!(%err, "failed to persist ip event");
            }
        }
        None => tracing::debug!("no database available; skipping history write"),
    }
}

/// Current snapshot, local network facts, and online status for the details
/// window. `netinfo::collect()` runs fresh on every call — it's local-only
/// (no network I/O), so there's no reason to cache it.
#[tauri::command]
pub async fn get_details(poller: State<'_, Arc<Poller>>) -> Result<Details, String> {
    let snapshot = poller.current().await;
    let netinfo = netinfo::collect().map_err(|err| err.to_string())?;
    Ok(Details {
        snapshot,
        netinfo,
        online: poller.is_online().await,
    })
}

/// Forces an immediate poll outside the regular interval. Always emits
/// `refresh-done`, even though `Poller::refresh_once` cannot itself fail
/// (failures are handled internally as an `Offline` publish) — if it ever
/// gained a fallible path, a missing `refresh-done` would hang the UI
/// spinner forever, so the emit is unconditional rather than tied to success.
#[tauri::command]
pub async fn refresh(app: AppHandle, poller: State<'_, Arc<Poller>>) -> Result<(), String> {
    if let Err(err) = app.emit("refresh-started", ()) {
        tracing::error!(%err, "failed to emit refresh-started event");
    }
    poller.refresh_once().await;
    if let Err(err) = app.emit("refresh-done", ()) {
        tracing::error!(%err, "failed to emit refresh-done event");
    }
    Ok(())
}

/// Most recent history rows, newest first. Answers an empty list rather than
/// an error when no database is available (see `open_db`) — degraded mode,
/// not a failure.
#[tauri::command]
pub fn get_history(limit: u32, db: State<'_, SharedDb>) -> Result<Vec<IpEvent>, String> {
    let guard = match db.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.as_ref() {
        Some(db) => db.recent_events(limit).map_err(|err| err.to_string()),
        None => Ok(Vec::new()),
    }
}

/// Runs an on-demand DNS leak test (PLAN.md Phase 3): resolves bash.ws probe
/// subdomains through the system resolver, asks bash.ws which resolvers
/// reached it, and compares those resolvers' ASN against the current
/// external IP's ASN (from the last successful poll, if any) to draw a
/// verdict. See `dnsleak` module docs for the full protocol and comparison
/// rule.
///
/// Concurrency: guarded by `DnsLeakGuard::try_lock`, which rejects a second
/// concurrent invocation immediately rather than queuing it. Each test takes
/// several seconds and is tied to one randomly-generated bash.ws session; two
/// running at once from the same app instance has no legitimate use (the UI
/// can only show one result at a time), and silently queuing the second call
/// would leave its caller waiting with no feedback for no benefit. Sequential
/// calls — the normal "run it again" case — are always safe: the guard is
/// released as soon as the previous call's future completes or is dropped.
#[tauri::command]
pub async fn run_dns_leak_test(
    poller: State<'_, Arc<Poller>>,
    guard: State<'_, DnsLeakGuard>,
) -> Result<LeakReport, String> {
    let _permit = guard
        .try_lock()
        .map_err(|_| "a DNS leak test is already running".to_string())?;

    let expected_asn = poller.current().await.and_then(|snapshot| snapshot.geo.asn);
    let client = dnsleak::http_client();
    let service = BashWs::default();

    dnsleak::run_test(&service, &client, expected_asn.as_deref())
        .await
        .map_err(|err| err.to_string())
}

/// Returns the currently effective settings (PLAN.md Phase 4).
#[tauri::command]
pub fn get_settings(settings: State<'_, SharedSettings>) -> Result<Settings, String> {
    let guard = match settings.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    Ok(guard.clone())
}

/// Validates/clamps `new_settings`, persists them, applies their side
/// effects to the running session (poll interval, autostart registration),
/// and returns the settings as actually applied.
///
/// Returning the *effective* settings (not just echoing the input back) is
/// deliberate: the caller may have sent an out-of-range `poll_interval_secs`
/// (a stray keystroke, a bug, or just the clamp differing from what the UI
/// let the user type), and the frontend should reflect what's actually
/// running, not what it optimistically asked for.
///
/// A failed write to `settings.json` is logged, not returned as an error —
/// the in-memory state and running side effects are updated regardless
/// (the setting is applied for the rest of this session even if it won't
/// survive a restart), mirroring how a failed history write in
/// `persist_if_warranted` doesn't stop monitoring either.
///
/// `expected_country_code` is applied live (PLAN.md Phase 4): when it
/// changes, `tray::TrayLiveState::set_expected_country` re-baselines the
/// tray's warn latch against the poller's *current* snapshot and pushes a
/// fresh icon/tooltip immediately, rather than waiting for the next `Change`
/// off the poller — which could be up to a full poll interval away. Async
/// only because that call is: every other side effect here stays
/// synchronous.
#[tauri::command]
pub async fn set_settings(
    app: AppHandle,
    new_settings: Settings,
    settings: State<'_, SharedSettings>,
    poller: State<'_, Arc<Poller>>,
    tray: State<'_, tray::SharedTray>,
) -> Result<Settings, String> {
    let mut effective = new_settings;
    effective.clamp();

    let previous_expected_country_code = {
        let mut guard = match settings.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let previous = guard.expected_country_code.clone();
        *guard = effective.clone();
        previous
    };

    match settings_file_path(&app) {
        Some(path) => {
            if let Err(err) = settings::save(&path, &effective) {
                tracing::error!(%err, path = %path.display(), "failed to persist settings");
            }
        }
        None => tracing::warn!("no app data dir available; settings applied for this session only"),
    }

    poller.set_interval(Duration::from_secs(effective.poll_interval_secs));
    apply_autostart(&app, effective.launch_at_startup);

    if effective.expected_country_code != previous_expected_country_code {
        tray.set_expected_country(effective.expected_country_code.clone())
            .await;
    }

    Ok(effective)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::poller::classify;

    /// Guards the exact interaction brief 6.1 calls out: a second `Initial`
    /// classification for an unchanged IP must stay suppressed once the
    /// observation round-trips through a real `Db` via `last_snapshot`, not
    /// just when a `Snapshot` is constructed by hand (see
    /// `poller::tests::a_seeded_baseline_makes_an_unchanged_restart_silent`
    /// for that narrower, Tauri-free version). Without the Phase 3
    /// poller-seeding fix this reconstructs to a fresh session with no
    /// baseline, `classify` returns `Initial` again, and every restart — or
    /// every second process a missing single-instance guard would otherwise
    /// let through — adds a duplicate `ip_events` row.
    #[test]
    fn a_second_initial_for_an_unchanged_ip_is_suppressed_after_reload() {
        let db = Db::open(":memory:").expect("in-memory db opens");
        db.insert_event(&IpEvent {
            id: None,
            ts: 1_700_000_000,
            external_ip: "203.0.113.7".to_string(),
            country: Some("United States".to_string()),
            country_code: Some("US".to_string()),
            isp: Some("Example ISP".to_string()),
            change_reason: ChangeReason::Initial,
        })
        .unwrap();

        let shared: SharedDb = Arc::new(StdMutex::new(Some(db)));
        let baseline =
            last_snapshot(&shared).expect("a baseline is rebuilt from the stored row");

        // What the poller would observe on the very next tick if nothing
        // about the connection has actually changed: same ip/country/isp,
        // only `observed_at` moves forward.
        let fresh = Snapshot {
            observed_at: 1_700_000_999,
            ..baseline.clone()
        };

        assert_eq!(
            classify(Some(&baseline), &fresh),
            None,
            "an unchanged ip/country/isp must not classify as a change, or a second \
             process (or restart) would double-write ip_events"
        );
    }
}
