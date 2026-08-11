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
use tokio::sync::{broadcast, watch, RwLock};
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
#[derive(Debug, Clone, Serialize)]
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
    /// Held in a `watch` channel rather than a plain field so `run()` can
    /// react to a change immediately (see `run`'s `tokio::select!`) instead
    /// of only picking up a new value the next time it happens to read it.
    interval_tx: watch::Sender<Duration>,
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
        let (interval_tx, _interval_rx) = watch::channel(interval);
        Self {
            ip_chain,
            geo_chain,
            client,
            interval_tx,
            state: Arc::new(RwLock::new(None)),
            offline: Arc::new(RwLock::new(false)),
            tx,
        }
    }

    /// The currently configured poll interval.
    pub fn interval(&self) -> Duration {
        *self.interval_tx.borrow()
    }

    /// Updates the poll interval, taking effect immediately rather than only
    /// after the current tick's (possibly up to an hour long) sleep elapses —
    /// see `run`'s `tokio::select!` for how the new value preempts the old
    /// countdown.
    ///
    /// Deliberately Tauri-free: this takes a plain `Duration`, not a
    /// `Settings`. Validating/clamping a user-supplied value is the caller's
    /// job (see `settings::clamp_poll_interval_secs`), not the poller's — the
    /// poller should not need to learn what "settings" even are.
    pub fn set_interval(&self, interval: Duration) {
        // `send_replace` (unlike `send`) succeeds even if `run()` hasn't
        // started yet and no receiver has subscribed, so this is safe to
        // call at any point in the poller's lifetime.
        self.interval_tx.send_replace(interval);
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

    /// Installs a starting point for change detection, without publishing a
    /// `Change` or writing history.
    ///
    /// Used at startup to carry the last persisted reading across a restart.
    /// Without it the poller begins every session with no previous snapshot,
    /// so `classify` returns `Initial` and a fresh row is written on every
    /// launch even when nothing actually changed — which fills the history
    /// view with duplicates.
    ///
    /// A seeded snapshot is intentionally partial: `ip_events` stores only
    /// country, country code and ISP, which is exactly what `classify`
    /// compares, and `handle_success` replaces it wholesale on the first live
    /// poll. Does nothing if a snapshot is already present, so this can never
    /// clobber a live reading.
    pub async fn seed(&self, snapshot: Snapshot) {
        let mut state = self.state.write().await;
        if state.is_none() {
            *state = Some(snapshot);
        }
    }

    /// Whether the most recent tick succeeded.
    ///
    /// Deliberately distinct from `current().is_some()`: the last-known
    /// snapshot survives an outage on purpose, so only this reports live
    /// reachability. Read this rather than deriving online-ness from the
    /// `Change` stream — recovering to the *same* IP produces no `Change`
    /// (`classify` returns `None`), so a stream-derived flag would stay stuck
    /// offline after a real recovery.
    pub async fn is_online(&self) -> bool {
        !*self.offline.read().await
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
        let reason = classify(previous.as_ref(), &current);

        // Store the fresh snapshot even when nothing changed. Two reasons:
        // `observed_at` must advance so the UI can say when the reading was
        // last *verified* rather than when it last *changed*; and a snapshot
        // seeded from the database carries only the columns `ip_events` has,
        // so it must be replaced by a complete one on the first live poll or
        // the details window would show blank city/coordinates/ASN forever.
        {
            let mut state = self.state.write().await;
            *state = Some(current.clone());
        }

        let Some(reason) = reason else {
            return;
        };

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

    /// The refresh loop itself, as a plain future.
    ///
    /// Kept separate from `spawn` so a caller can put it on whichever executor
    /// it already has. Tauri's `setup` runs on the main thread with no ambient
    /// Tokio runtime, where `tokio::spawn` panics outright — such callers
    /// should drive this directly instead.
    ///
    /// `tokio::select!`s between the regular tick and the interval `watch`
    /// channel changing, rather than just reading `interval_tx`'s value fresh
    /// on every tick. That distinction matters: with a 1-hour interval, a
    /// settings change to (say) 10s must not sit ignored until the current
    /// hour-long sleep finishes — `interval_rx.changed()` wakes the loop
    /// immediately and reschedules the ticker from *now*, so the new interval
    /// governs the very next tick rather than the one after.
    pub async fn run(self: Arc<Self>) {
        let mut interval_rx = self.interval_tx.subscribe();
        let mut ticker = tokio::time::interval(*interval_rx.borrow());
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.refresh_once().await;
                }
                Ok(()) = interval_rx.changed() => {
                    let new_interval = *interval_rx.borrow();
                    // `interval_at` (not `interval`) so the next tick lands
                    // exactly `new_interval` from *now*, rather than firing
                    // immediately the way a freshly built `tokio::time::interval`
                    // always does on its first `.tick()`.
                    ticker = tokio::time::interval_at(
                        tokio::time::Instant::now() + new_interval,
                        new_interval,
                    );
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                }
            }
        }
    }

    /// Spawns `run` onto the ambient Tokio runtime.
    ///
    /// Requires an active runtime context. From a sync context that has none
    /// (notably Tauri's `setup`), spawn `run()` on that host's executor rather
    /// than calling this.
    pub fn spawn(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(self.run())
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

    fn idle_poller() -> Poller {
        // No providers, so no tick can ever succeed or touch the network.
        // Enough to exercise seed/state handling directly.
        let client = crate::providers::http_client().expect("client builds without network access");
        Poller::with_interval(
            Chain::new(Vec::new()),
            Chain::new(Vec::new()),
            client,
            Duration::from_secs(60),
        )
    }

    #[tokio::test]
    async fn seed_installs_a_baseline_without_publishing() {
        let poller = idle_poller();
        let mut changes = poller.subscribe();

        poller
            .seed(snapshot("1.1.1.1", Some("US"), Some("Cloudflare")))
            .await;

        assert_eq!(poller.current().await.map(|s| s.ip.to_string()).as_deref(), Some("1.1.1.1"));
        assert!(
            changes.try_recv().is_err(),
            "seeding is bookkeeping, not an observation: it must not publish a Change"
        );
    }

    #[tokio::test]
    async fn seed_never_clobbers_a_live_reading() {
        let poller = idle_poller();

        poller.seed(snapshot("1.1.1.1", Some("US"), None)).await;
        poller.seed(snapshot("2.2.2.2", Some("DE"), None)).await;

        assert_eq!(
            poller.current().await.map(|s| s.ip.to_string()).as_deref(),
            Some("1.1.1.1"),
            "a second seed must not overwrite what is already there"
        );
    }

    #[tokio::test]
    async fn a_seeded_baseline_makes_an_unchanged_restart_silent() {
        // The restart-duplicate bug: without a baseline the first reading of
        // every session classified as Initial and wrote another history row.
        let poller = idle_poller();
        let stored = snapshot("1.1.1.1", Some("US"), Some("Cloudflare"));
        poller.seed(stored.clone()).await;

        let fresh = Snapshot { observed_at: 12_345, ..stored.clone() };

        assert_eq!(
            classify(poller.current().await.as_ref(), &fresh),
            None,
            "same ip, country and isp after a restart is not a change"
        );
    }

    #[tokio::test]
    async fn an_unchanged_tick_still_advances_the_stored_snapshot() {
        // `observed_at` must track when the reading was last *verified*, not
        // when it last *changed*, and a partial seeded snapshot has to be
        // replaced by a complete one on the first live poll.
        let poller = idle_poller();
        poller.seed(snapshot("1.1.1.1", Some("US"), Some("Cloudflare"))).await;

        let mut fresh = snapshot("1.1.1.1", Some("US"), Some("Cloudflare"));
        fresh.observed_at = 99_999;
        fresh.geo.city = Some("Manassas".into());

        poller.handle_success(fresh).await;

        let stored = poller.current().await.expect("a snapshot is stored");
        assert_eq!(stored.observed_at, 99_999, "observed_at must advance on an unchanged tick");
        assert_eq!(
            stored.geo.city.as_deref(),
            Some("Manassas"),
            "the fuller live snapshot must replace the partial seeded one"
        );
    }

    // --- live-updatable interval (PLAN.md Phase 4) ---

    #[test]
    fn interval_reports_the_constructor_value() {
        let poller = Poller::with_interval(
            Chain::new(Vec::new()),
            Chain::new(Vec::new()),
            crate::providers::http_client().expect("client builds without network access"),
            Duration::from_secs(45),
        );
        assert_eq!(poller.interval(), Duration::from_secs(45));
    }

    #[test]
    fn set_interval_updates_the_observable_value() {
        let poller = idle_poller();
        assert_eq!(poller.interval(), Duration::from_secs(60));

        poller.set_interval(Duration::from_secs(5));

        assert_eq!(poller.interval(), Duration::from_secs(5));
    }

    #[test]
    fn set_interval_before_run_starts_does_not_panic() {
        // `send_replace` must not require a live receiver — `run()` may not
        // have subscribed yet (or may never be called at all, e.g. in a unit
        // test like this one).
        let poller = idle_poller();
        poller.set_interval(Duration::from_secs(30));
        assert_eq!(poller.interval(), Duration::from_secs(30));
    }

    /// A provider that returns a fresh IP on every call, so every successful
    /// tick classifies as `IpChanged` and is guaranteed to publish a `Change`
    /// — a simple, deterministic way to count ticks via the broadcast
    /// channel in the test below.
    struct SequentialIpProvider(std::sync::atomic::AtomicU32);

    #[async_trait]
    impl IpProvider for SequentialIpProvider {
        fn name(&self) -> &'static str {
            "sequential-test-provider"
        }

        async fn fetch_ip(&self, _client: &Client) -> Result<IpAddr, ProviderError> {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(IpAddr::from([10, 0, (n >> 8) as u8, n as u8]))
        }
    }

    /// Proves the "immediately" half of `set_interval`'s contract: a change
    /// reschedules the next tick from *now*, rather than sitting ignored
    /// until the previous (here, one-hour) sleep finishes.
    ///
    /// Uses a paused/auto-advancing clock (`start_paused = true`) so the test
    /// doesn't burn a real hour: with a paused clock, Tokio jumps straight to
    /// the earliest pending timer whenever the runtime has nothing else to
    /// do. If `run()` merely re-read a plain field on each tick instead of
    /// reacting to the `watch` channel, the earliest pending timer would
    /// still be the original ~3600s tick, the inner `timeout` below (30
    /// simulated seconds) would win the race, and the test would fail.
    #[tokio::test(start_paused = true)]
    async fn set_interval_takes_effect_before_the_old_interval_elapses() {
        let ip_chain: Chain<dyn IpProvider> = Chain::new(vec![Box::new(SequentialIpProvider(
            std::sync::atomic::AtomicU32::new(1),
        )) as Box<dyn IpProvider>]);
        let geo_chain: Chain<dyn GeoProvider> = Chain::new(Vec::new());
        let client = crate::providers::http_client().expect("client builds without network access");
        let poller = Arc::new(Poller::with_interval(
            ip_chain,
            geo_chain,
            client,
            Duration::from_secs(3600),
        ));

        let mut changes = poller.subscribe();
        let run_handle = tokio::spawn(poller.clone().run());

        // `tokio::time::interval`'s first tick always fires immediately, so
        // the first (Initial) change arrives without any time advance.
        let first = changes.recv().await.expect("first change");
        assert_eq!(first.reason, ChangeReason::Initial);

        let before = tokio::time::Instant::now();
        poller.set_interval(Duration::from_secs(5));

        let second = tokio::time::timeout(Duration::from_secs(30), changes.recv())
            .await
            .expect("second change must arrive without waiting out the old 3600s interval")
            .expect("second change");
        assert_eq!(second.reason, ChangeReason::IpChanged);

        let elapsed = tokio::time::Instant::now() - before;
        assert!(
            elapsed <= Duration::from_secs(6),
            "expected the rescheduled 5s interval to govern the next tick, got {elapsed:?} of simulated time"
        );

        run_handle.abort();
    }
}
