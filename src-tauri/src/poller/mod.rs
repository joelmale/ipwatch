//! Periodic refresh loop and IP-change detection.
//!
//! Deliberately decoupled from Tauri: the loop publishes over a broadcast
//! channel rather than calling `app.emit`, so the whole thing is unit-testable
//! without an app handle. Phase 2 bridges the channel to the frontend event.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, RwLock};
use tokio::task::JoinHandle;

use crate::db::ChangeReason;
use crate::providers::{Chain, GeoInfo, GeoProvider, IpProvider, ProviderError};

/// Default poll interval, per PLAN.md Phase 1.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Broadcast channel depth. Generous relative to the poll interval: a slow
/// subscriber only needs to catch up, never block the poller.
const CHANGE_CHANNEL_CAPACITY: usize = 32;

/// The most recent successful observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub ip: IpAddr,
    pub geo: GeoInfo,
    /// Unix seconds.
    pub observed_at: i64,
}

/// Published whenever the poller decides something changed.
#[derive(Debug, Clone)]
pub struct Change {
    pub previous: Option<Snapshot>,
    pub current: Option<Snapshot>,
    pub reason: ChangeReason,
}

/// Decides whether `current` differs from `previous` in a way worth recording.
///
/// Ordering matters: a country change is the VPN-drop signal and outranks a
/// bare IP change, which happens routinely on reconnect within the same exit.
pub fn classify(previous: Option<&Snapshot>, current: &Snapshot) -> Option<ChangeReason> {
    let Some(previous) = previous else {
        return Some(ChangeReason::Initial);
    };

    if known_and_differs(&previous.geo.country_code, &current.geo.country_code) {
        return Some(ChangeReason::CountryChanged);
    }

    if known_and_differs(&previous.geo.isp, &current.geo.isp) {
        return Some(ChangeReason::IspChanged);
    }

    if previous.ip != current.ip {
        return Some(ChangeReason::IpChanged);
    }

    None
}

/// Two optional fields count as "changed" only when both sides actually
/// reported a value and those values disagree. Free-tier geo providers omit
/// fields unpredictably; reading a missing field as "changed" would fire a
/// spurious VPN-drop alert on provider flakiness rather than a real drop, and
/// a noisy alert is worse than a missed one — users learn to ignore it.
fn known_and_differs(previous: &Option<String>, current: &Option<String>) -> bool {
    matches!((previous, current), (Some(p), Some(c)) if p != c)
}

/// Runs the refresh loop and tracks the last-known snapshot.
///
/// Holds no Tauri handle and does not own a `Db` — persistence is the
/// caller's job, driven off `subscribe()`. That keeps this module testable
/// in isolation and reusable if the storage layer ever changes.
pub struct Poller {
    ip_chain: Chain<dyn IpProvider>,
    geo_chain: Chain<dyn GeoProvider>,
    client: Client,
    interval: Duration,
    state: Arc<RwLock<Option<Snapshot>>>,
    /// Tracks whether the *last* tick failed, so a run of failures only
    /// publishes `Offline` once, on the transition into that state.
    offline: Arc<RwLock<bool>>,
    tx: broadcast::Sender<Change>,
}

impl Poller {
    /// Builds a poller with the default 60s interval.
    pub fn new(ip_chain: Chain<dyn IpProvider>, geo_chain: Chain<dyn GeoProvider>, client: Client) -> Self {
        Self::with_interval(ip_chain, geo_chain, client, DEFAULT_INTERVAL)
    }

    /// Builds a poller with an explicit interval.
    pub fn with_interval(
        ip_chain: Chain<dyn IpProvider>,
        geo_chain: Chain<dyn GeoProvider>,
        client: Client,
        interval: Duration,
    ) -> Self {
        let (tx, _rx) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        Self {
            ip_chain,
            geo_chain,
            client,
            interval,
            state: Arc::new(RwLock::new(None)),
            offline: Arc::new(RwLock::new(false)),
            tx,
        }
    }

    /// The configured poll interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Subscribes to published changes. Each subscriber gets its own queue.
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.tx.subscribe()
    }

    /// Reads the current last-known snapshot without waiting for a tick.
    ///
    /// Stays populated with the last-known-good value through an offline
    /// stretch — callers should pair this with a subscription to `Change`s
    /// carrying `ChangeReason::Offline` to know when to mark it stale in the
    /// UI, rather than inferring staleness from an absence here.
    pub async fn current(&self) -> Option<Snapshot> {
        self.state.read().await.clone()
    }

    /// Performs exactly one refresh: resolve IP, resolve geo, classify
    /// against the previous snapshot, and publish if something changed.
    pub async fn refresh_once(&self) {
        match self.resolve_snapshot().await {
            Ok(current) => self.handle_success(current).await,
            Err(err) => self.handle_failure(&err).await,
        }
    }

    async fn resolve_snapshot(&self) -> Result<Snapshot, ProviderError> {
        let ip = self.ip_chain.fetch_ip(&self.client).await?;

        let geo = match self.geo_chain.fetch_geo(&self.client, Some(ip)).await {
            Ok(geo) => geo,
            Err(err) => {
                // A failed geo lookup on a resolved IP isn't "offline" — the
                // network is clearly up. Fall back to an all-unknown GeoInfo
                // so classify() treats those fields as unchanged rather than
                // manufacturing a false CountryChanged/IspChanged.
                tracing::warn!(%err, "geo lookup failed; continuing with unknown geo fields");
                GeoInfo::default()
            }
        };

        Ok(Snapshot {
            ip,
            geo,
            observed_at: now_unix(),
        })
    }

    async fn handle_success(&self, current: Snapshot) {
        {
            let mut offline = self.offline.write().await;
            *offline = false;
        }

        let previous = self.state.read().await.clone();

        let Some(reason) = classify(previous.as_ref(), &current) else {
            return;
        };

        {
            let mut state = self.state.write().await;
            *state = Some(current.clone());
        }

        let _ = self.tx.send(Change {
            previous,
            current: Some(current),
            reason,
        });
    }

    async fn handle_failure(&self, err: &ProviderError) {
        tracing::warn!(%err, "refresh failed; treating as offline");

        let mut offline = self.offline.write().await;
        if *offline {
            // Already known offline: don't spam subscribers on every
            // subsequent failing tick.
            return;
        }
        *offline = true;
        drop(offline);

        // Keep the last-known snapshot in shared state untouched — the UI
        // shows the last good value marked stale, not a blank slate.
        let previous = self.state.read().await.clone();
        let _ = self.tx.send(Change {
            previous: previous.clone(),
            current: previous,
            reason: ChangeReason::Offline,
        });
    }

    /// Runs the refresh loop on its own tokio task until dropped.
    pub fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                self.refresh_once().await;
            }
        })
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    fn snapshot(ip: &str, country_code: Option<&str>, isp: Option<&str>) -> Snapshot {
        Snapshot {
            ip: ip.parse().unwrap(),
            geo: GeoInfo {
                country_code: country_code.map(String::from),
                isp: isp.map(String::from),
                ..Default::default()
            },
            observed_at: 0,
        }
    }

    #[test]
    fn first_observation_is_initial() {
        let current = snapshot("1.1.1.1", Some("US"), Some("Cloudflare"));
        assert_eq!(classify(None, &current), Some(ChangeReason::Initial));
    }

    #[test]
    fn identical_snapshot_is_no_change() {
        let s = snapshot("1.1.1.1", Some("US"), Some("Cloudflare"));
        assert_eq!(classify(Some(&s), &s.clone()), None);
    }

    #[test]
    fn ip_changed_same_country_and_isp() {
        let previous = snapshot("1.1.1.1", Some("US"), Some("Cloudflare"));
        let current = snapshot("2.2.2.2", Some("US"), Some("Cloudflare"));
        assert_eq!(
            classify(Some(&previous), &current),
            Some(ChangeReason::IpChanged)
        );
    }

    #[test]
    fn country_change_outranks_simultaneous_ip_change() {
        // Both the IP and the country differ in the same tick — exactly what
        // a VPN drop looks like: the tunnel falls and traffic falls back to
        // the raw ISP exit, which is both a new address and a new country.
        // CountryChanged must win, not IpChanged.
        let previous = snapshot("1.1.1.1", Some("US"), Some("Cloudflare"));
        let current = snapshot("2.2.2.2", Some("FR"), Some("Cloudflare"));
        assert_eq!(
            classify(Some(&previous), &current),
            Some(ChangeReason::CountryChanged)
        );
    }

    #[test]
    fn isp_changed_same_country() {
        let previous = snapshot("1.1.1.1", Some("US"), Some("Cloudflare"));
        let current = snapshot("1.1.1.1", Some("US"), Some("Comcast"));
        assert_eq!(
            classify(Some(&previous), &current),
            Some(ChangeReason::IspChanged)
        );
    }

    #[test]
    fn unknown_previous_country_code_is_not_treated_as_a_change() {
        // The previous poll's provider omitted country_code (common on free
        // tiers); this poll's provider reported one. That's a provider gap,
        // not evidence the country changed, so we must not read it as one —
        // an alert here would be a false VPN-drop warning caused by nothing
        // more than API flakiness. Everything else matches, so the correct
        // answer is "no change" rather than guessing in either direction.
        let previous = snapshot("1.1.1.1", None, Some("Cloudflare"));
        let current = snapshot("1.1.1.1", Some("US"), Some("Cloudflare"));
        assert_eq!(classify(Some(&previous), &current), None);
    }

    #[test]
    fn both_country_codes_unknown_is_not_a_country_change() {
        let previous = snapshot("1.1.1.1", None, Some("Cloudflare"));
        let current = snapshot("1.1.1.1", None, Some("Cloudflare"));
        assert_eq!(classify(Some(&previous), &current), None);
    }

    /// Fails immediately without touching the network, so tests can exercise
    /// the offline path deterministically.
    struct FailingIpProvider;

    #[async_trait]
    impl IpProvider for FailingIpProvider {
        fn name(&self) -> &'static str {
            "failing-test-provider"
        }

        async fn fetch_ip(&self, _client: &Client) -> Result<IpAddr, ProviderError> {
            Err(ProviderError::Http("simulated offline".into()))
        }
    }

    #[tokio::test]
    async fn offline_is_published_only_on_transition() {
        let ip_chain: Chain<dyn IpProvider> =
            Chain::new(vec![Box::new(FailingIpProvider) as Box<dyn IpProvider>]);
        let geo_chain: Chain<dyn GeoProvider> = Chain::new(Vec::new());
        let client = crate::providers::http_client().expect("client builds without network access");
        let poller = Poller::with_interval(ip_chain, geo_chain, client, Duration::from_secs(60));
        let mut changes = poller.subscribe();

        poller.refresh_once().await;
        poller.refresh_once().await;
        poller.refresh_once().await;

        let change = changes
            .try_recv()
            .expect("expected exactly one Offline change");
        assert_eq!(change.reason, ChangeReason::Offline);
        assert!(
            changes.try_recv().is_err(),
            "must not republish Offline on repeated failing ticks"
        );
    }
}
