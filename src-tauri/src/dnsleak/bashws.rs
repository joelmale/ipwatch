//! bash.ws implementation of [`LeakTestService`].
//!
//! Protocol (verified live against the real service):
//! 1. A random session id.
//! 2. `N` probe hostnames of the form `{i}.{id}.{probe_domain}` — resolving
//!    them (elsewhere, through the system resolver; see `super::run_test`)
//!    is what causes bash.ws's authoritative nameserver to record which
//!    resolvers asked.
//! 3. `GET {base_url}/dnsleak/test/{id}?json`, which returns either a JSON
//!    array of result entries, or `{"error": "..."}` if queried before the
//!    probe lookups have been recorded.
//!
//! `base_url` and `probe_domain` are both configurable so tests can point
//! this at a wiremock server instead of the real service.

use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use uuid::Uuid;

use super::{DnsLeakError, LeakSession, LeakTestService, ServiceEntry};

/// Number of probe hostnames resolved per test. bash.ws's own tooling uses
/// small values in this range; 8 is what was verified live.
const DEFAULT_PROBE_COUNT: u32 = 8;

/// Length of the session id, in lowercase hex characters, matching the
/// verified live protocol.
const SESSION_ID_LEN: usize = 12;

pub struct BashWs {
    /// Base URL for the results HTTP endpoint, e.g. `https://bash.ws`.
    base_url: String,
    /// Domain the numbered probe hostnames are generated under, e.g.
    /// `bash.ws`. Kept separate from `base_url` because it names something
    /// resolved over DNS, not fetched over HTTP.
    probe_domain: String,
    probe_count: u32,
}

impl Default for BashWs {
    fn default() -> Self {
        Self {
            base_url: "https://bash.ws".to_string(),
            probe_domain: "bash.ws".to_string(),
            probe_count: DEFAULT_PROBE_COUNT,
        }
    }
}

impl BashWs {
    /// Points the results HTTP call at another base URL — used by tests to
    /// aim it at a wiremock server. The probe domain (used only for DNS
    /// hostnames, never fetched) is left at the real `bash.ws`.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            ..Default::default()
        }
    }

    fn results_url(&self, session_id: &str) -> String {
        format!(
            "{}/dnsleak/test/{}?json",
            self.base_url.trim_end_matches('/'),
            session_id
        )
    }
}

#[async_trait]
impl LeakTestService for BashWs {
    fn name(&self) -> &'static str {
        "bash.ws"
    }

    fn new_session(&self) -> LeakSession {
        let id = generate_session_id();
        let hostnames = (1..=self.probe_count)
            .map(|i| format!("{i}.{id}.{}", self.probe_domain))
            .collect();
        LeakSession { id, hostnames }
    }

    async fn fetch_results(
        &self,
        client: &Client,
        session: &LeakSession,
    ) -> Result<Vec<ServiceEntry>, DnsLeakError> {
        let url = self.results_url(&session.id);

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| DnsLeakError::Http(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(DnsLeakError::Http(format!("HTTP {status}")));
        }

        let value: Value = resp
            .json()
            .await
            .map_err(|e| DnsLeakError::Parse(e.to_string()))?;

        parse_response(value)
    }
}

/// Parses the results endpoint's body, handling both documented shapes
/// explicitly:
/// - a JSON array of result entries (the success case), or
/// - a JSON object carrying `{"error": "..."}` (returned when queried before
///   the probe lookups have propagated — the most likely real-world failure).
///
/// A naive `serde_json::from_slice::<Vec<ServiceEntry>>` would turn the
/// error-object shape into an opaque deserialize failure; this checks the
/// top-level shape first so that case gets its own clear error variant.
fn parse_response(value: Value) -> Result<Vec<ServiceEntry>, DnsLeakError> {
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(|item| serde_json::from_value::<ServiceEntry>(item).map_err(|e| DnsLeakError::Parse(e.to_string())))
            .collect(),
        Value::Object(map) => {
            let message = map
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("service returned an object response with no recognizable error field")
                .to_string();
            Err(DnsLeakError::ServiceError(message))
        }
        other => Err(DnsLeakError::Parse(format!(
            "unexpected top-level JSON shape: {other}"
        ))),
    }
}

/// A random 12-character lowercase-hex session id. Built from a UUIDv4
/// (truncated) rather than anything timestamp-derived: a predictable id
/// could collide with another user's concurrent bash.ws test and return
/// their resolvers instead of ours.
fn generate_session_id() -> String {
    let full = Uuid::new_v4().simple().to_string();
    full[..SESSION_ID_LEN].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_session_ids_are_twelve_lowercase_hex_chars() {
        for _ in 0..20 {
            let id = generate_session_id();
            assert_eq!(id.len(), SESSION_ID_LEN);
            assert!(
                id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "expected lowercase hex, got {id:?}"
            );
        }
    }

    #[test]
    fn generated_session_ids_are_not_constant() {
        let a = generate_session_id();
        let b = generate_session_id();
        assert_ne!(a, b, "session ids must be random, not fixed/predictable");
    }

    #[test]
    fn new_session_builds_expected_probe_hostnames() {
        let service = BashWs::default();
        let session = service.new_session();

        assert_eq!(session.hostnames.len(), DEFAULT_PROBE_COUNT as usize);
        for (i, host) in session.hostnames.iter().enumerate() {
            assert_eq!(host, &format!("{}.{}.bash.ws", i + 1, session.id));
        }
    }

    #[test]
    fn results_url_uses_configured_base_and_strips_trailing_slash() {
        let service = BashWs::with_base_url("http://127.0.0.1:9999/");
        assert_eq!(
            service.results_url("abc123"),
            "http://127.0.0.1:9999/dnsleak/test/abc123?json"
        );
    }
}
