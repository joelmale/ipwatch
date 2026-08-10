//! Tauri glue: wires the decoupled `Poller`/`Db` core into app state, bridges
//! `Poller::subscribe()` to frontend events, and exposes the invoke commands.
//!
//! Deliberately kept separate from `poller`/`db`/`providers`/`netinfo` — those
//! modules stay Tauri-free and unit-testable on their own; this module is the
//! only place that knows about `tauri::App`/`AppHandle`.

use std::sync::{Arc, Mutex as StdMutex};

use reqwest::Client;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::broadcast;

use crate::db::{ChangeReason, Db, IpEvent};
use crate::netinfo::{self, NetInfo};
use crate::poller::{Change, Poller, Snapshot};
use crate::providers::{default_geo_chain, default_ip_chain, http_client};

/// `rusqlite::Connection` (inside `Db`) is `Send` but not `Sync`, so a bare
/// `Db` cannot go into Tauri managed state — state must be `Send + Sync`.
/// `Option` wraps it so a database that fails to open at startup degrades to
/// "no history" instead of taking the whole app down (see `open_db`).
///
/// A std `Mutex` is fine here: every access is a quick synchronous SQLite
/// call, contention is nil at a 60s poll interval, and no code path holds the
/// guard across an `.await`.
pub type SharedDb = Arc<StdMutex<Option<Db>>>;

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

    let client = http_client().unwrap_or_else(|err| {
        tracing::error!(%err, "failed to build the configured http client; falling back to a default reqwest client");
        Client::new()
    });
    let poller = Arc::new(Poller::new(default_ip_chain(), default_geo_chain(), client));
    app.manage(poller.clone());

    // The poller's own refresh loop; entirely inside poller/mod.rs, which
    // stays Tauri-free by design.
    let _poll_loop = poller.clone().spawn();

    spawn_event_bridge(handle, poller, shared_db);

    Ok(())
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
fn spawn_event_bridge(app: AppHandle, poller: Arc<Poller>, db: SharedDb) {
    tauri::async_runtime::spawn(async move {
        let mut rx = poller.subscribe();
        loop {
            match rx.recv().await {
                Ok(change) => {
                    persist_if_warranted(&db, &change);
                    if let Err(err) = app.emit("ip-changed", &change) {
                        tracing::error!(%err, "failed to emit ip-changed event");
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
