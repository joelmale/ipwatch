//! Unit tests for the `providers` module using wiremock to stand in for the
//! real HTTP endpoints. Covers chain failover semantics and per-provider
//! response parsing.

use super::*;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A realistic ip-api.com success payload, matching the fields the real
/// service returns (including the ones renamed by serde).
const IP_API_SUCCESS_BODY: &str = r#"{
    "status": "success",
    "country": "United States",
    "countryCode": "US",
    "region": "CA",
    "regionName": "California",
    "city": "Mountain View",
    "zip": "94043",
    "lat": 37.4056,
    "lon": -122.0775,
    "timezone": "America/Los_Angeles",
    "isp": "Google LLC",
    "org": "Google LLC",
    "as": "AS15169 Google LLC",
    "query": "142.250.72.14"
}"#;

// ---------------------------------------------------------------------
// Chain failover
// ---------------------------------------------------------------------

#[tokio::test]
async fn chain_falls_back_to_secondary_when_primary_returns_server_error() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&primary)
        .await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("203.0.113.5"))
        .mount(&secondary)
        .await;

    let client = http_client().expect("client builds");
    let chain: Chain<dyn IpProvider> = Chain::new(vec![
        Box::new(Ipify::with_url(primary.uri())) as Box<dyn IpProvider>,
        Box::new(IcanHazIp::with_url(secondary.uri())) as Box<dyn IpProvider>,
    ]);

    let ip = chain.fetch_ip(&client).await.expect("secondary should succeed");
    assert_eq!(ip, "203.0.113.5".parse::<IpAddr>().unwrap());
}

#[tokio::test]
async fn chain_reports_all_failed_with_one_named_error_per_provider() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&primary)
        .await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&secondary)
        .await;

    let client = http_client().expect("client builds");
    let chain: Chain<dyn IpProvider> = Chain::new(vec![
        Box::new(Ipify::with_url(primary.uri())) as Box<dyn IpProvider>,
        Box::new(IcanHazIp::with_url(secondary.uri())) as Box<dyn IpProvider>,
    ]);

    let err = chain.fetch_ip(&client).await.expect_err("both providers fail");
    match err {
        ProviderError::AllFailed(errors) => {
            assert_eq!(errors.len(), 2, "one entry per provider: {errors:?}");
            assert!(
                errors.iter().any(|e| e.contains("ipify")),
                "missing ipify entry: {errors:?}"
            );
            assert!(
                errors.iter().any(|e| e.contains("icanhazip")),
                "missing icanhazip entry: {errors:?}"
            );
        }
        other => panic!("expected AllFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn chain_treats_unparseable_success_body_as_parse_error_and_continues() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>not an ip</html>"))
        .mount(&primary)
        .await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("203.0.113.9"))
        .mount(&secondary)
        .await;

    let client = http_client().expect("client builds");
    let chain: Chain<dyn IpProvider> = Chain::new(vec![
        Box::new(Ipify::with_url(primary.uri())) as Box<dyn IpProvider>,
        Box::new(IcanHazIp::with_url(secondary.uri())) as Box<dyn IpProvider>,
    ]);

    let ip = chain.fetch_ip(&client).await.expect("secondary should succeed");
    assert_eq!(ip, "203.0.113.9".parse::<IpAddr>().unwrap());
}

#[tokio::test]
async fn chain_never_calls_later_providers_once_the_first_succeeds() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("203.0.113.1"))
        .mount(&primary)
        .await;

    // If the chain short-circuits correctly, this mock is never hit; wiremock
    // verifies `.expect(0)` when `secondary` drops at the end of the test.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("203.0.113.2"))
        .expect(0)
        .mount(&secondary)
        .await;

    let client = http_client().expect("client builds");
    let chain: Chain<dyn IpProvider> = Chain::new(vec![
        Box::new(Ipify::with_url(primary.uri())) as Box<dyn IpProvider>,
        Box::new(IcanHazIp::with_url(secondary.uri())) as Box<dyn IpProvider>,
    ]);

    let ip = chain.fetch_ip(&client).await.expect("primary should succeed");
    assert_eq!(ip, "203.0.113.1".parse::<IpAddr>().unwrap());
}

// ---------------------------------------------------------------------
// Plaintext IP providers
// ---------------------------------------------------------------------

#[tokio::test]
async fn plaintext_body_with_trailing_newline_parses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("203.0.113.7\n"))
        .mount(&server)
        .await;

    let client = http_client().expect("client builds");
    let provider = Ipify::with_url(server.uri());

    let ip = provider.fetch_ip(&client).await.expect("should parse");
    assert_eq!(ip, "203.0.113.7".parse::<IpAddr>().unwrap());
}

#[tokio::test]
async fn plaintext_body_with_surrounding_whitespace_parses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("  203.0.113.7  \n"))
        .mount(&server)
        .await;

    let client = http_client().expect("client builds");
    let provider = Ipify::with_url(server.uri());

    let ip = provider.fetch_ip(&client).await.expect("should parse");
    assert_eq!(ip, "203.0.113.7".parse::<IpAddr>().unwrap());
}

#[tokio::test]
async fn plaintext_garbage_body_yields_parse_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("this is not an ip address"))
        .mount(&server)
        .await;

    let client = http_client().expect("client builds");
    let provider = Ipify::with_url(server.uri());

    let err = provider.fetch_ip(&client).await.expect_err("garbage body should fail to parse");
    match err {
        ProviderError::Parse { provider, .. } => assert_eq!(provider, "ipify"),
        other => panic!("expected Parse error, got {other:?}"),
    }
}

#[tokio::test]
async fn plaintext_ipv6_body_parses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("2001:db8::1"))
        .mount(&server)
        .await;

    let client = http_client().expect("client builds");
    let provider = Ipify::with_url(server.uri());

    let ip = provider.fetch_ip(&client).await.expect("should parse ipv6");
    assert_eq!(ip, "2001:db8::1".parse::<IpAddr>().unwrap());
}

// ---------------------------------------------------------------------
// ip-api.com geo provider
// ---------------------------------------------------------------------

#[tokio::test]
async fn geo_success_payload_maps_onto_geo_info_including_renamed_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(IP_API_SUCCESS_BODY, "application/json"),
        )
        .mount(&server)
        .await;

    let client = http_client().expect("client builds");
    let provider = IpApiCom::with_base_url(server.uri());

    let geo = provider.fetch_geo(&client, None).await.expect("should parse success payload");

    assert_eq!(geo.country_code.as_deref(), Some("US"), "countryCode -> country_code");
    assert_eq!(geo.region.as_deref(), Some("California"), "regionName -> region");
    assert_eq!(geo.asn.as_deref(), Some("AS15169 Google LLC"), "as -> asn");
    assert_eq!(geo.country.as_deref(), Some("United States"));
    assert_eq!(geo.city.as_deref(), Some("Mountain View"));
    assert_eq!(geo.isp.as_deref(), Some("Google LLC"));
    assert_eq!(geo.org.as_deref(), Some("Google LLC"));
    assert_eq!(geo.timezone.as_deref(), Some("America/Los_Angeles"));
    assert_eq!(geo.lat, Some(37.4056));
    assert_eq!(geo.lon, Some(-122.0775));
    assert_eq!(geo.ip, Some("142.250.72.14".parse::<IpAddr>().unwrap()));
}

#[tokio::test]
async fn geo_status_fail_in_http_200_body_yields_rejected_with_message() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"status":"fail","message":"reserved range"}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let client = http_client().expect("client builds");
    let provider = IpApiCom::with_base_url(server.uri());

    let err = provider.fetch_geo(&client, None).await.expect_err("status: fail should error");
    match err {
        ProviderError::Rejected { message, .. } => {
            assert_eq!(message, "reserved range");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn geo_http_429_yields_rejected_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let client = http_client().expect("client builds");
    let provider = IpApiCom::with_base_url(server.uri());

    let err = provider.fetch_geo(&client, None).await.expect_err("429 should be rejected");
    match err {
        ProviderError::Rejected { provider, .. } => assert_eq!(provider, "ip-api.com"),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn geo_request_path_includes_ip_when_a_specific_ip_is_requested() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/json/203.0.113.9"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(IP_API_SUCCESS_BODY, "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = http_client().expect("client builds");
    let provider = IpApiCom::with_base_url(format!("{}/json", server.uri()));

    let requested_ip: IpAddr = "203.0.113.9".parse().unwrap();
    let geo = provider
        .fetch_geo(&client, Some(requested_ip))
        .await
        .expect("path-specific mock should match");
    assert_eq!(geo.ip, Some(requested_ip));
}

#[tokio::test]
async fn geo_request_path_omits_ip_when_no_specific_ip_is_requested() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(IP_API_SUCCESS_BODY, "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = http_client().expect("client builds");
    let provider = IpApiCom::with_base_url(format!("{}/json", server.uri()));

    provider
        .fetch_geo(&client, None)
        .await
        .expect("base-path mock should match when no ip is given");
}
