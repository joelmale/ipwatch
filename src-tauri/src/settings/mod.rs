//! Persisted user settings (PLAN.md Phase 4).
//!
//! **Deliberate deviation from PLAN.md's original Swift→Tauri mapping**, which
//! pointed at `tauri-plugin-store` for `AppSettings`. This module instead
//! reads/writes a plain `serde_json` file at `app_data_dir()/settings.json`.
//! Reason: the poll interval and the notifications toggle must be known in
//! Rust *before* any webview exists (the poller starts inside `app::setup`,
//! ahead of the frontend), and `tauri-plugin-store` is primarily a JS-facing
//! API — reading it from pure Rust at that point is awkward. A plain JSON
//! file read with `std::fs` has no such ordering dependency.
//!
//! Deliberately Tauri-free, like `db` and `poller`: this module only knows
//! about a `Path` and a `Settings` value. The `app_data_dir()`-aware wrapper
//! (resolving the path, creating the directory, managing Tauri state) lives
//! in `app::open_settings`, mirroring `app::open_db`'s treatment of `Db`.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Floor for `poll_interval_secs`, enforced on every load and every
/// `set_settings` call. A user-supplied (or corrupted) `0` would spin the
/// poll loop hammering the IP/geo providers with no backoff, which is a
/// realistic way to get them rate-limited or banned — that must be
/// impossible regardless of what ends up in the settings file.
pub const MIN_POLL_INTERVAL_SECS: u64 = 10;

/// Ceiling for `poll_interval_secs`. An hour keeps "how stale can the tray
/// get" bounded to something a user would recognize as intentional rather
/// than a hung app.
pub const MAX_POLL_INTERVAL_SECS: u64 = 3600;

/// Default poll interval, matching `poller::DEFAULT_INTERVAL`.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;

/// User-configurable settings (PLAN.md Phase 4).
///
/// `#[serde(default)]` on the struct means a JSON file missing any of these
/// fields (e.g. one written by an older version of ipwatch, before a field
/// existed) still deserializes successfully — the missing field falls back
/// to its `Default` value instead of failing the whole parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub poll_interval_secs: u64,
    pub notifications_enabled: bool,
    pub launch_at_startup: bool,
    pub expected_country_code: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            notifications_enabled: true,
            launch_at_startup: false,
            expected_country_code: None,
        }
    }
}

impl Settings {
    /// Clamps `poll_interval_secs` into `[MIN_POLL_INTERVAL_SECS,
    /// MAX_POLL_INTERVAL_SECS]` in place. Called by both `load` (a hand-edited
    /// file might contain anything) and the `set_settings` command (a
    /// misbehaving or malicious frontend might send anything) — validation
    /// happens at both entry points, not just one.
    pub fn clamp(&mut self) {
        self.poll_interval_secs = clamp_poll_interval_secs(self.poll_interval_secs);
    }
}

/// Pure clamp helper, exposed separately so callers (and tests) can clamp a
/// bare value without constructing a whole `Settings`.
pub fn clamp_poll_interval_secs(secs: u64) -> u64 {
    secs.clamp(MIN_POLL_INTERVAL_SECS, MAX_POLL_INTERVAL_SECS)
}

/// Loads settings from `path`.
///
/// Follows the same "never fail startup" policy as `db::open_db`: a missing
/// file, an unreadable file, or corrupt/invalid JSON all fall back to
/// `Settings::default()` rather than propagating an error. The failure (if
/// any, other than a simple missing file — that's the expected first-run
/// case) is logged so it's visible in diagnostics, but a user who hand-edits
/// `settings.json` into something broken gets a working app with default
/// settings on next launch, not a dead one.
///
/// Always returns an already-clamped `Settings`, so callers never need to
/// clamp again after a fresh load.
pub fn load<P: AsRef<Path>>(path: P) -> Settings {
    let path = path.as_ref();

    let mut settings = match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str::<Settings>(&contents) {
            Ok(settings) => settings,
            Err(err) => {
                tracing::warn!(
                    %err,
                    path = %path.display(),
                    "settings.json is corrupt or invalid; falling back to defaults"
                );
                Settings::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(err) => {
            tracing::warn!(
                %err,
                path = %path.display(),
                "could not read settings.json; falling back to defaults"
            );
            Settings::default()
        }
    };

    settings.clamp();
    settings
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persists `settings` to `path` as pretty-printed JSON, creating the parent
/// directory if it does not already exist.
///
/// Unlike `load`, failures here ARE propagated: the caller (`app::set_settings`)
/// decides how to react to a failed write (log it, still apply the setting
/// for the running session) rather than this module silently pretending the
/// write succeeded.
pub fn save<P: AsRef<Path>>(path: P, settings: &Settings) -> Result<(), SettingsError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    std::fs::write(path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("ipwatch-settings-test");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    #[test]
    fn defaults_when_file_absent() {
        let path = temp_path("does-not-exist.json");
        let _ = std::fs::remove_file(&path);

        let settings = load(&path);

        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn corrupt_json_falls_back_to_defaults_without_error() {
        let path = temp_path("corrupt.json");
        std::fs::write(&path, "{ not valid json at all").unwrap();

        let settings = load(&path);

        assert_eq!(settings, Settings::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn valid_but_wrong_shaped_json_falls_back_to_defaults() {
        // A JSON value that parses fine but isn't an object (e.g. a bare
        // array) should also degrade to defaults, not panic or bubble up.
        let path = temp_path("wrong-shape.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();

        let settings = load(&path);

        assert_eq!(settings, Settings::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let path = temp_path("round-trip.json");
        let original = Settings {
            poll_interval_secs: 120,
            notifications_enabled: false,
            launch_at_startup: true,
            expected_country_code: Some("NL".to_string()),
        };

        save(&path, &original).unwrap();
        let loaded = load(&path);

        assert_eq!(loaded, original);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_newer_fields_still_load_via_serde_default() {
        // Simulates a settings.json written by an older ipwatch version,
        // before `expected_country_code` existed.
        let path = temp_path("missing-field.json");
        std::fs::write(
            &path,
            r#"{"poll_interval_secs": 90, "notifications_enabled": false, "launch_at_startup": true}"#,
        )
        .unwrap();

        let settings = load(&path);

        assert_eq!(settings.poll_interval_secs, 90);
        assert!(!settings.notifications_enabled);
        assert!(settings.launch_at_startup);
        assert_eq!(settings.expected_country_code, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_json_object_loads_all_defaults_via_serde_default() {
        let path = temp_path("empty-object.json");
        std::fs::write(&path, "{}").unwrap();

        let settings = load(&path);

        assert_eq!(settings, Settings::default());
        let _ = std::fs::remove_file(&path);
    }

    // --- clamping ---

    #[test]
    fn clamp_zero_goes_to_minimum() {
        assert_eq!(clamp_poll_interval_secs(0), MIN_POLL_INTERVAL_SECS);
    }

    #[test]
    fn clamp_five_goes_to_minimum() {
        assert_eq!(clamp_poll_interval_secs(5), MIN_POLL_INTERVAL_SECS);
    }

    #[test]
    fn clamp_sixty_is_unchanged() {
        assert_eq!(clamp_poll_interval_secs(60), 60);
    }

    #[test]
    fn clamp_999999_goes_to_maximum() {
        assert_eq!(clamp_poll_interval_secs(999_999), MAX_POLL_INTERVAL_SECS);
    }

    #[test]
    fn clamp_is_applied_on_load() {
        let path = temp_path("needs-clamping.json");
        std::fs::write(&path, r#"{"poll_interval_secs": 0}"#).unwrap();

        let settings = load(&path);

        assert_eq!(settings.poll_interval_secs, MIN_POLL_INTERVAL_SECS);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clamp_is_applied_on_load_for_out_of_range_high_value() {
        let path = temp_path("needs-clamping-high.json");
        std::fs::write(&path, r#"{"poll_interval_secs": 999999}"#).unwrap();

        let settings = load(&path);

        assert_eq!(settings.poll_interval_secs, MAX_POLL_INTERVAL_SECS);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_clamp_method_clamps_in_place() {
        let mut settings = Settings { poll_interval_secs: 0, ..Settings::default() };
        settings.clamp();
        assert_eq!(settings.poll_interval_secs, MIN_POLL_INTERVAL_SECS);

        let mut settings = Settings { poll_interval_secs: 999_999, ..Settings::default() };
        settings.clamp();
        assert_eq!(settings.poll_interval_secs, MAX_POLL_INTERVAL_SECS);
    }
}
