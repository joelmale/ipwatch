//! On-demand DNS leak check.
//!
//! A VPN can carry your traffic correctly while your DNS queries still exit
//! via your ISP's resolvers, because the OS resolver config is a separate
//! concern from the tunnel's routing table. When that happens, every domain
//! you visit is still visible to whoever runs those resolvers — a "DNS
//! leak". This module detects that by:
//!
//! 1. Generating a random per-test session id.
//! 2. Resolving a handful of `{n}.{id}.<probe domain>` names through the
//!    **system resolver** (`tokio::net::lookup_host`, i.e. `getaddrinfo`) —
//!    never a custom DNS client, because the whole point is to exercise
//!    whatever resolver the OS is actually configured to use. These lookups
//!    are expected to fail (NXDOMAIN): the point is only that the query
//!    reaches the probe domain's authoritative nameserver, which records
//!    which resolver asked.
//! 3. Asking a [`LeakTestService`] which resolvers it saw for that session,
//!    and comparing them against the current external IP's network.
//!
//! The bash.ws implementation lives in [`bashws`]; swap in a different
//! [`LeakTestService`] to point this at another provider. Verdict logic is a
//! pure function ([`verdict::evaluate`]) so it is testable without any I/O.

use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

pub mod bashws;
mod verdict;

#[cfg(test)]
mod tests;

pub use bashws::BashWs;
pub use verdict::Verdict;

/// Per-lookup ceiling during the DNS probe phase. Generous, since a failing
/// lookup (the expected outcome) can take a while to time out on its own.
const PER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Ceiling on the whole DNS probe phase, regardless of how many probe names
/// there are or how slow individual lookups are.
const DNS_PHASE_TIMEOUT: Duration = Duration::from_secs(8);

/// How long to wait after the DNS phase before querying the service, so its
/// backend has a moment to collect the nameserver hits.
const RESULT_SETTLE_DELAY: Duration = Duration::from_millis(1500);

/// Ceiling on the HTTP call that fetches results. Longer than
/// `providers::REQUEST_TIMEOUT` deliberately: unlike a plain IP lookup, the
/// service may itself be waiting on propagation before it can answer.
const FETCH_RESULTS_TIMEOUT: Duration = Duration::from_secs(10);

// Worst case: DNS_PHASE_TIMEOUT + RESULT_SETTLE_DELAY + FETCH_RESULTS_TIMEOUT
// = well under 30s, so the whole test can never hang the UI for long even if
// every step maxes out its budget.

/// Builds (once) the HTTP client used for the results fetch.
///
/// Deliberately not `providers::http_client()`: that client's 5s timeout is
/// tuned for a plain IP lookup, whereas the leak-test service may itself be
/// waiting on DNS propagation before it can answer — see
/// `FETCH_RESULTS_TIMEOUT`. Falls back to a default client on the
/// (practically unreachable) chance the configured builder fails, rather
/// than propagating an error for something this unlikely to fail in practice.
pub fn http_client() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .timeout(FETCH_RESULTS_TIMEOUT)
                .user_agent(concat!("ipwatch/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_else(|err| {
                    tracing::error!(%err, "failed to build dnsleak http client with custom config; falling back to reqwest defaults");
                    Client::new()
                })
        })
        .clone()
}

/// Resolver identified by the leak-test service as having queried for this
/// test session.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resolver {
    pub ip: IpAddr,
    pub country: Option<String>,
    pub asn: Option<String>,
}

/// Result of one DNS leak test.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LeakReport {
    /// The service's own view of the caller's external IP, when it reported one.
    pub external_ip: Option<IpAddr>,
    pub resolvers: Vec<Resolver>,
    pub verdict: Verdict,
}

#[derive(Debug, thiserror::Error)]
pub enum DnsLeakError {
    #[error("http error: {0}")]
    Http(String),

    #[error("could not parse response from the leak-test service: {0}")]
    Parse(String),

    /// The service answered but told us it couldn't run the test yet — e.g.
    /// bash.ws's `{"error": "No DNS servers found. Try again..."}` when
    /// queried before the probe lookups have propagated. Distinct from a
    /// transport error: the request succeeded, the *test* didn't.
    #[error("leak-test service reported an error: {0}")]
    ServiceError(String),

    #[error("timed out while {0}")]
    Timeout(&'static str),
}

/// One test session's identity: a random id plus the DNS names that must be
/// resolved through the system resolver before [`LeakTestService::fetch_results`]
/// will have anything meaningful to report.
#[derive(Debug, Clone)]
pub struct LeakSession {
    pub id: String,
    pub hostnames: Vec<String>,
}

/// One entry in the leak-test service's raw JSON response.
///
/// `kind` mirrors the verified bash.ws `type` field: `"ip"` (the caller's
/// external IP, in `ip`), `"dns"` (one entry per resolver that queried, with
/// `ip`/`country_name`/`asn` populated), or `"conclusion"` (a human-readable
/// sentence in `ip`, e.g. "DNS is not leaking." — *not* an address; see
/// `split_entries`, which deliberately does not try to parse it as one).
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceEntry {
    pub ip: String,
    pub country_name: Option<String>,
    pub asn: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
}

/// Abstracts the leak-test backend so bash.ws is swappable and testable.
///
/// Deliberately does *not* own the DNS-resolution step: that must always go
/// through the system resolver (see module docs), so it lives in `run_test`
/// rather than behind this trait. What *is* protocol-specific — the probe
/// hostname scheme and the results endpoint — belongs here.
#[async_trait]
pub trait LeakTestService: Send + Sync {
    fn name(&self) -> &'static str;

    /// Starts a fresh test session: a random id and the hostnames the caller
    /// must resolve (through the system resolver) before calling
    /// `fetch_results`.
    fn new_session(&self) -> LeakSession;

    /// Fetches the service's view of who resolved this session's hostnames.
    async fn fetch_results(
        &self,
        client: &Client,
        session: &LeakSession,
    ) -> Result<Vec<ServiceEntry>, DnsLeakError>;
}

/// Runs one full DNS leak test against `service`: starts a session, resolves
/// its probe hostnames through the system resolver, fetches results, and
/// draws a verdict against `expected_asn` (the ASN of the current external
/// IP — the VPN exit, when the tunnel is actually up).
///
/// Individual probe-hostname lookups are expected to fail (NXDOMAIN) and
/// their errors are deliberately discarded — see module docs. Only a failure
/// to *fetch results* is surfaced as an `Err`, since that means the test
/// itself could not be completed (as opposed to "completed and found no
/// resolvers", which is a valid, reportable outcome via `Verdict::NoResolvers`).
pub async fn run_test(
    service: &dyn LeakTestService,
    client: &Client,
    expected_asn: Option<&str>,
) -> Result<LeakReport, DnsLeakError> {
    let session = service.new_session();

    if tokio::time::timeout(
        DNS_PHASE_TIMEOUT,
        resolve_probe_hostnames(&session.hostnames),
    )
    .await
    .is_err()
    {
        tracing::warn!(
            service = service.name(),
            "dns leak probe phase hit its overall timeout; proceeding with whatever queries went out"
        );
    }

    tokio::time::sleep(RESULT_SETTLE_DELAY).await;

    let entries = match tokio::time::timeout(
        FETCH_RESULTS_TIMEOUT,
        service.fetch_results(client, &session),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => return Err(DnsLeakError::Timeout("fetching dnsleak results")),
    };

    let (external_ip, resolvers) = split_entries(entries);
    let verdict = verdict::evaluate(expected_asn, &resolvers);

    Ok(LeakReport {
        external_ip,
        resolvers,
        verdict,
    })
}

/// Fires every probe-hostname lookup concurrently through the system
/// resolver and waits for them all to finish (or individually time out).
/// Failures — including NXDOMAIN, the expected outcome — are intentionally
/// not collected or reported: reaching the authoritative nameserver is the
/// entire point, not getting an address back.
async fn resolve_probe_hostnames(hostnames: &[String]) {
    let mut set = tokio::task::JoinSet::new();
    for host in hostnames {
        // Each spawned task needs its own owned copy to move into the future.
        let host = host.clone();
        set.spawn(async move {
            // Port 0 is a placeholder; `lookup_host` needs a socket-address
            // shaped target but nothing here ever connects to it.
            let target = format!("{host}:0");
            let _ = tokio::time::timeout(PER_LOOKUP_TIMEOUT, tokio::net::lookup_host(target)).await;
        });
    }
    while set.join_next().await.is_some() {}
}

/// Splits a leak-test service's raw entries into the external IP it saw and
/// the list of resolvers that queried. Pure and I/O-free, so it is testable
/// directly against hand-built `ServiceEntry` fixtures.
///
/// - `"ip"` entries populate `external_ip`. `"dns"` entries become
///   `Resolver`s. `"conclusion"` entries are logged and otherwise ignored —
///   verdict is always derived independently via `verdict::evaluate`, never
///   by trusting the service's own prose conclusion.
/// - Any entry whose `ip` field does not parse as an `IpAddr` is dropped
///   with a warning rather than causing the whole response to fail — most
///   importantly, this is what keeps a `"conclusion"` entry's sentence (e.g.
///   "DNS is not leaking.") from ever being fed through `IpAddr::parse`.
fn split_entries(entries: Vec<ServiceEntry>) -> (Option<IpAddr>, Vec<Resolver>) {
    let mut external_ip = None;
    let mut resolvers = Vec::new();

    for entry in entries {
        match entry.kind.as_str() {
            "ip" => match entry.ip.parse::<IpAddr>() {
                Ok(ip) => external_ip = Some(ip),
                Err(_) => tracing::warn!(
                    raw = %entry.ip,
                    "dnsleak service 'ip' entry was not a parseable address; ignoring"
                ),
            },
            "dns" => match entry.ip.parse::<IpAddr>() {
                Ok(ip) => resolvers.push(Resolver {
                    ip,
                    country: entry.country_name,
                    asn: entry.asn,
                }),
                Err(_) => tracing::warn!(
                    raw = %entry.ip,
                    "dnsleak service 'dns' entry was not a parseable address; ignoring"
                ),
            },
            "conclusion" => {
                tracing::debug!(
                    message = %entry.ip,
                    "dnsleak service conclusion (informational only; verdict is derived independently)"
                );
            }
            other => {
                tracing::debug!(
                    kind = other,
                    "dnsleak service returned an entry of unknown type; ignoring"
                );
            }
        }
    }

    (external_ip, resolvers)
}
