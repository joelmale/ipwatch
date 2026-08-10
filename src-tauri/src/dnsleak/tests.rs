//! Unit tests for the `dnsleak` module using wiremock to stand in for
//! bash.ws. No test in this file (or `bashws`/`verdict`) touches the real
//! network: `BashWs::fetch_results` is exercised directly against wiremock,
//! and the one fake `LeakTestService` used to test `run_test` end-to-end
//! hands back zero probe hostnames, so the DNS-resolution phase has nothing
//! to look up.

use std::net::IpAddr;

use async_trait::async_trait;
use reqwest::Client;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn session(id: &str) -> LeakSession {
    LeakSession {
        id: id.to_string(),
        hostnames: Vec::new(),
    }
}

// ---------------------------------------------------------------------
// BashWs::fetch_results — response-shape handling
// ---------------------------------------------------------------------

const SAMPLE_SUCCESS_BODY: &str = r#"[
    {"ip":"203.0.113.9","country_name":"","asn":"","type":"ip"},
    {"ip":"1.1.1.1","country_name":"Australia","asn":"AS13335 Cloudflare, Inc.","type":"dns"},
    {"ip":"DNS is not leaking.","country_name":"","asn":"","type":"conclusion"}
]"#;

#[tokio::test]
async fn fetch_results_parses_valid_array_response_including_conclusion_quirk() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SAMPLE_SUCCESS_BODY, "application/json"))
        .mount(&server)
        .await;

    let client = Client::new();
    let service = BashWs::with_base_url(server.uri());

    let entries = service
        .fetch_results(&client, &session("abc123456789"))
        .await
        .expect("valid array response should parse");
    assert_eq!(entries.len(), 3, "all three entries, including conclusion, should deserialize");

    let (external_ip, resolvers) = split_entries(entries);
    assert_eq!(external_ip, Some("203.0.113.9".parse::<IpAddr>().unwrap()));

    // The "DNS is not leaking." conclusion entry must not become a resolver,
    // and must not blow up trying to parse its `ip` field as an address.
    assert_eq!(resolvers.len(), 1, "conclusion entry must not be misparsed as a resolver");
    assert_eq!(resolvers[0].ip, "1.1.1.1".parse::<IpAddr>().unwrap());
    assert_eq!(resolvers[0].country.as_deref(), Some("Australia"));
    assert_eq!(resolvers[0].asn.as_deref(), Some("AS13335 Cloudflare, Inc."));
}

#[tokio::test]
async fn fetch_results_yields_clean_service_error_for_the_error_object_shape() {
    // Verified real-world response when queried before the probe lookups
    // have propagated: an object, not an array. A naive
    // `Vec<ServiceEntry>` deserialize would fail confusingly here.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"error": "No DNS servers found. Try again..."}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let client = Client::new();
    let service = BashWs::with_base_url(server.uri());

    let err = service
        .fetch_results(&client, &session("abc123456789"))
        .await
        .expect_err("error-object shape must not be parsed as a result array");

    match err {
        DnsLeakError::ServiceError(message) => {
            assert_eq!(message, "No DNS servers found. Try again...");
        }
        other => panic!("expected ServiceError, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_results_yields_clean_error_for_unrecognized_object_shape() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"unexpected":"shape"}"#, "application/json"))
        .mount(&server)
        .await;

    let client = Client::new();
    let service = BashWs::with_base_url(server.uri());

    let err = service
        .fetch_results(&client, &session("abc123456789"))
        .await
        .expect_err("object without a recognizable error field must still be a clean error");

    assert!(
        matches!(err, DnsLeakError::ServiceError(_)),
        "expected ServiceError, got {err:?}"
    );
}

#[tokio::test]
async fn fetch_results_yields_http_error_for_non_success_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let client = Client::new();
    let service = BashWs::with_base_url(server.uri());

    let err = service
        .fetch_results(&client, &session("abc123456789"))
        .await
        .expect_err("503 should be a clean http error");

    match err {
        DnsLeakError::Http(message) => assert!(message.contains("503")),
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_results_yields_parse_error_for_malformed_json_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("not json at all", "application/json"))
        .mount(&server)
        .await;

    let client = Client::new();
    let service = BashWs::with_base_url(server.uri());

    let err = service
        .fetch_results(&client, &session("abc123456789"))
        .await
        .expect_err("malformed body should not panic");
    assert!(matches!(err, DnsLeakError::Parse(_)), "expected Parse error, got {err:?}");
}

// ---------------------------------------------------------------------
// split_entries — pure, no I/O
// ---------------------------------------------------------------------

#[test]
fn split_entries_empty_input_yields_no_ip_and_no_resolvers() {
    let (ip, resolvers) = split_entries(Vec::new());
    assert_eq!(ip, None);
    assert!(resolvers.is_empty());
}

#[test]
fn split_entries_ignores_unparseable_ip_field_on_ip_entry() {
    let entries = vec![ServiceEntry {
        ip: "not-an-address".to_string(),
        country_name: None,
        asn: None,
        kind: "ip".to_string(),
    }];
    let (ip, resolvers) = split_entries(entries);
    assert_eq!(ip, None, "unparseable ip entry must be dropped, not surfaced as garbage");
    assert!(resolvers.is_empty());
}

#[test]
fn split_entries_ignores_entries_of_unknown_type() {
    let entries = vec![ServiceEntry {
        ip: "1.2.3.4".to_string(),
        country_name: None,
        asn: None,
        kind: "something-new".to_string(),
    }];
    let (ip, resolvers) = split_entries(entries);
    assert_eq!(ip, None);
    assert!(resolvers.is_empty());
}

// ---------------------------------------------------------------------
// run_test — end-to-end orchestration against a fake service
// ---------------------------------------------------------------------

/// A `LeakTestService` whose `new_session` hands back zero probe
/// hostnames, so `run_test`'s DNS-resolution phase has nothing to look up
/// and this stays fully offline while still exercising the real
/// orchestration logic (timeouts, settle delay, result parsing, verdict).
struct NoProbeService {
    base_url: String,
}

#[async_trait]
impl LeakTestService for NoProbeService {
    fn name(&self) -> &'static str {
        "no-probe-test-double"
    }

    fn new_session(&self) -> LeakSession {
        session("fixed-session-id")
    }

    async fn fetch_results(&self, client: &Client, session: &LeakSession) -> Result<Vec<ServiceEntry>, DnsLeakError> {
        let url = format!("{}/dnsleak/test/{}?json", self.base_url, session.id);
        let resp = client.get(&url).send().await.map_err(|e| DnsLeakError::Http(e.to_string()))?;
        let value: serde_json::Value = resp.json().await.map_err(|e| DnsLeakError::Parse(e.to_string()))?;
        match value {
            serde_json::Value::Array(items) => items
                .into_iter()
                .map(|item| {
                    serde_json::from_value::<ServiceEntry>(item).map_err(|e| DnsLeakError::Parse(e.to_string()))
                })
                .collect(),
            serde_json::Value::Object(map) => Err(DnsLeakError::ServiceError(
                map.get("error").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
            )),
            _ => Err(DnsLeakError::Parse("unexpected shape".to_string())),
        }
    }
}

#[tokio::test]
async fn run_test_with_empty_resolver_list_reports_no_resolvers_verdict() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("[]", "application/json"))
        .mount(&server)
        .await;

    let client = Client::new();
    let service = NoProbeService { base_url: server.uri() };

    let report = run_test(&service, &client, Some("AS7922 Comcast Cable"))
        .await
        .expect("empty result set is a valid, successful outcome");

    assert!(report.resolvers.is_empty());
    assert_eq!(report.verdict, Verdict::NoResolvers);
}

#[tokio::test]
async fn run_test_draws_consistent_verdict_when_resolver_asn_matches_expected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"[{"ip":"10.0.0.1","country_name":"United States","asn":"AS7922 Comcast Cable","type":"dns"}]"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let client = Client::new();
    let service = NoProbeService { base_url: server.uri() };

    let report = run_test(&service, &client, Some("AS7922 Comcast Cable"))
        .await
        .expect("should succeed");

    assert_eq!(report.verdict, Verdict::Consistent);
}

#[tokio::test]
async fn run_test_propagates_service_error_shape_as_err_not_panic() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(r#"{"error": "No DNS servers found. Try again..."}"#, "application/json"),
        )
        .mount(&server)
        .await;

    let client = Client::new();
    let service = NoProbeService { base_url: server.uri() };

    let err = run_test(&service, &client, None)
        .await
        .expect_err("service error shape must surface as a typed Err, not a panic");
    assert!(matches!(err, DnsLeakError::ServiceError(_)), "expected ServiceError, got {err:?}");
}
