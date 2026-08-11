//! VPN-drop toast notifications (PLAN.md Phase 3).
//!
//! Fired from the existing event bridge in `app::spawn_event_bridge` — this
//! module deliberately does not subscribe to `Poller::subscribe()` on its own.
//! The bridge is already the single persistence + emit point for every
//! `Change`, and a second subscriber would just be a second place to keep in
//! sync with the poller's semantics for no benefit.
//!
//! The decision (`should_notify`) and the message construction (`build_message`)
//! are kept as pure functions, separate from the actual `show()` call in
//! `notify_change`. Phase 4 adds a notifications on/off setting; because the
//! decision lives in one small pure function, that phase only has to gate the
//! call site (see `notify_change`), not touch the message logic.

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use crate::db::ChangeReason;
use crate::poller::{Change, Snapshot};

/// Decides whether a change with the given reason is worth a toast.
///
/// - `CountryChanged`: the real VPN-drop signal — the exit country itself
///   changed. Always notify.
/// - `IspChanged`: the country held steady but the ISP behind it changed,
///   which usually means the tunnel dropped to the bare underlying
///   connection while DNS/geo still resolved to a plausible-looking country.
///   Always notify.
/// - `Offline`: connectivity was lost outright, so VPN status can't be
///   verified at all. Always notify, worded as lost connectivity rather than
///   a leak (see `build_message`).
/// - `IpChanged` is deliberately excluded from notifications. It fires
///   routinely on ordinary VPN reconnects within the same exit country/ISP —
///   NOT a leak signal. Toasting every one of these would train the user to
///   dismiss ipwatch notifications reflexively, which destroys the value of
///   the notifications that actually matter. Do not "fix" this to notify.
/// - `Initial` is deliberately excluded. It is the app's first reading at
///   startup, not a change from a previous state — there is nothing to warn
///   the user about yet.
pub fn should_notify(reason: ChangeReason) -> bool {
    matches!(
        reason,
        ChangeReason::CountryChanged | ChangeReason::IspChanged | ChangeReason::Offline
    )
}

/// Title + body for a toast notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastMessage {
    pub title: String,
    pub body: String,
}

/// Builds an actionable toast message for a `Change`.
///
/// Every geo field on `Snapshot` is `Option` (free-tier geo APIs omit fields
/// unpredictably), so every lookup here falls back to plain language instead
/// of ever rendering "None"/"null" into a notification the user sees.
///
/// Handles all five `ChangeReason` variants for totality (this function must
/// never panic), even though in practice `notify_change` only calls it when
/// `should_notify` is true.
pub fn build_message(
    reason: ChangeReason,
    previous: Option<&Snapshot>,
    current: Option<&Snapshot>,
) -> ToastMessage {
    match reason {
        ChangeReason::Offline => ToastMessage {
            title: "ipwatch lost connectivity".to_string(),
            body: "Could not reach the network to verify your VPN status.".to_string(),
        },
        ChangeReason::CountryChanged => {
            let prev = previous
                .map(describe_country)
                .unwrap_or_else(unknown_country);
            let curr = current
                .map(describe_country)
                .unwrap_or_else(unknown_country);
            ToastMessage {
                title: "VPN may have dropped".to_string(),
                body: format!(
                    "Country changed: {prev} \u{2192} {curr}{ip}",
                    ip = ip_suffix(current)
                ),
            }
        }
        ChangeReason::IspChanged => {
            let prev = previous.map(describe_isp).unwrap_or_else(unknown_isp);
            let curr = current.map(describe_isp).unwrap_or_else(unknown_isp);
            let country = current
                .map(describe_country)
                .unwrap_or_else(unknown_country);
            ToastMessage {
                title: "VPN may have dropped".to_string(),
                body: format!(
                    "ISP changed in {country}: {prev} \u{2192} {curr}{ip}",
                    ip = ip_suffix(current)
                ),
            }
        }
        // Not reachable via `notify_change` (gated by `should_notify`), but
        // kept total rather than panicking so this function is safe to call
        // directly, e.g. from tests or a future caller.
        ChangeReason::IpChanged | ChangeReason::Initial => ToastMessage {
            title: "ipwatch".to_string(),
            body: "IP address changed.".to_string(),
        },
    }
}

fn describe_country(snapshot: &Snapshot) -> String {
    snapshot
        .geo
        .country
        .clone()
        .or_else(|| snapshot.geo.country_code.clone())
        .unwrap_or_else(unknown_country)
}

fn describe_isp(snapshot: &Snapshot) -> String {
    snapshot.geo.isp.clone().unwrap_or_else(unknown_isp)
}

fn unknown_country() -> String {
    "an unknown country".to_string()
}

fn unknown_isp() -> String {
    "an unknown ISP".to_string()
}

/// " (<ip>)" when `current` is available, empty string otherwise (e.g. an
/// `Offline` republish carries no fresh IP — though that reason never reaches
/// this helper today since it has its own message arm above).
fn ip_suffix(current: Option<&Snapshot>) -> String {
    current
        .map(|snapshot| format!(" ({})", snapshot.ip))
        .unwrap_or_default()
}

/// Shows a toast for `change` if `should_notify` says it's warranted.
///
/// Never lets a notification failure affect monitoring: any error from the
/// plugin is logged and swallowed, never propagated. This is called from
/// `spawn_event_bridge` after persistence and the `ip-changed` emit, so a
/// broken notification backend must not skip a DB write or stop the bridge
/// loop.
///
/// Phase 4's notifications-on/off setting gates here: `should_notify(...)`
/// becomes `settings.notifications_enabled && should_notify(...)`.
pub fn notify_change(app: &AppHandle, change: &Change) {
    if !should_notify(change.reason) {
        return;
    }

    let message = build_message(
        change.reason,
        change.previous.as_ref(),
        change.current.as_ref(),
    );

    if let Err(err) = app
        .notification()
        .builder()
        .title(message.title)
        .body(message.body)
        .show()
    {
        tracing::error!(%err, "failed to show VPN-drop notification");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::GeoInfo;

    fn snapshot(
        ip: &str,
        country: Option<&str>,
        country_code: Option<&str>,
        isp: Option<&str>,
    ) -> Snapshot {
        Snapshot {
            ip: ip.parse().unwrap(),
            geo: GeoInfo {
                country: country.map(String::from),
                country_code: country_code.map(String::from),
                isp: isp.map(String::from),
                ..Default::default()
            },
            observed_at: 0,
        }
    }

    #[test]
    fn should_notify_country_changed() {
        assert!(should_notify(ChangeReason::CountryChanged));
    }

    #[test]
    fn should_notify_isp_changed() {
        assert!(should_notify(ChangeReason::IspChanged));
    }

    #[test]
    fn should_notify_offline() {
        assert!(should_notify(ChangeReason::Offline));
    }

    #[test]
    fn should_not_notify_ip_changed() {
        assert!(!should_notify(ChangeReason::IpChanged));
    }

    #[test]
    fn should_not_notify_initial() {
        assert!(!should_notify(ChangeReason::Initial));
    }

    #[test]
    fn offline_message_is_worded_as_connectivity_not_leak() {
        let msg = build_message(ChangeReason::Offline, None, None);
        assert_eq!(msg.title, "ipwatch lost connectivity");
        assert!(!msg.body.to_lowercase().contains("vpn may have dropped"));
    }

    #[test]
    fn country_changed_message_includes_previous_and_current() {
        let previous = snapshot(
            "198.51.100.1",
            Some("Netherlands"),
            Some("NL"),
            Some("NordVPN"),
        );
        let current = snapshot(
            "203.0.113.7",
            Some("United States"),
            Some("US"),
            Some("NordVPN"),
        );

        let msg = build_message(
            ChangeReason::CountryChanged,
            Some(&previous),
            Some(&current),
        );

        assert_eq!(msg.title, "VPN may have dropped");
        assert_eq!(
            msg.body,
            "Country changed: Netherlands \u{2192} United States (203.0.113.7)"
        );
    }

    #[test]
    fn isp_changed_message_includes_country_and_isps() {
        let previous = snapshot(
            "198.51.100.1",
            Some("United States"),
            Some("US"),
            Some("NordVPN"),
        );
        let current = snapshot(
            "203.0.113.7",
            Some("United States"),
            Some("US"),
            Some("Comcast"),
        );

        let msg = build_message(ChangeReason::IspChanged, Some(&previous), Some(&current));

        assert_eq!(msg.title, "VPN may have dropped");
        assert_eq!(
            msg.body,
            "ISP changed in United States: NordVPN \u{2192} Comcast (203.0.113.7)"
        );
    }

    #[test]
    fn all_none_geo_never_renders_none_or_null() {
        let previous = snapshot("198.51.100.1", None, None, None);
        let current = snapshot("203.0.113.7", None, None, None);

        let msg = build_message(
            ChangeReason::CountryChanged,
            Some(&previous),
            Some(&current),
        );

        assert_eq!(
            msg.body,
            "Country changed: an unknown country \u{2192} an unknown country (203.0.113.7)"
        );
        assert!(!msg.body.to_lowercase().contains("none"));
        assert!(!msg.body.to_lowercase().contains("null"));

        let isp_msg = build_message(ChangeReason::IspChanged, Some(&previous), Some(&current));
        assert!(!isp_msg.body.to_lowercase().contains("none"));
        assert!(!isp_msg.body.to_lowercase().contains("null"));
    }

    #[test]
    fn country_changed_falls_back_to_country_code_when_name_missing() {
        let previous = snapshot("198.51.100.1", None, Some("NL"), None);
        let current = snapshot("203.0.113.7", None, Some("US"), None);

        let msg = build_message(
            ChangeReason::CountryChanged,
            Some(&previous),
            Some(&current),
        );

        assert_eq!(msg.body, "Country changed: NL \u{2192} US (203.0.113.7)");
    }

    #[test]
    fn country_changed_with_no_previous_snapshot() {
        let current = snapshot("203.0.113.7", Some("United States"), Some("US"), None);

        let msg = build_message(ChangeReason::CountryChanged, None, Some(&current));

        assert_eq!(
            msg.body,
            "Country changed: an unknown country \u{2192} United States (203.0.113.7)"
        );
    }
}
