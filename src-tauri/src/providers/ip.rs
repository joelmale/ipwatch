//! Plaintext external-IP endpoints. Each returns a bare address and nothing else.

use std::net::IpAddr;

use async_trait::async_trait;
use reqwest::Client;

use super::{IpProvider, ProviderError};

/// Shared logic: GET a URL whose entire body is an IP address.
async fn fetch_plaintext_ip(
    client: &Client,
    url: &str,
    provider: &'static str,
) -> Result<IpAddr, ProviderError> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| ProviderError::Http(e.to_string()))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(ProviderError::Rejected {
            provider,
            message: format!("HTTP {status}"),
        });
    }

    let body = resp
        .text()
        .await
        .map_err(|e| ProviderError::Http(e.to_string()))?;

    // These endpoints append a trailing newline; some pad with whitespace.
    body.trim()
        .parse::<IpAddr>()
        .map_err(|e| ProviderError::Parse {
            provider,
            detail: format!("{e}: {:?}", body.trim()),
        })
}

macro_rules! plaintext_provider {
    ($name:ident, $label:literal, $default_url:literal) => {
        pub struct $name {
            pub url: String,
        }

        impl Default for $name {
            fn default() -> Self {
                Self {
                    url: $default_url.to_string(),
                }
            }
        }

        impl $name {
            /// Point the provider at another base URL — used by tests to aim it
            /// at a wiremock server instead of the real endpoint.
            pub fn with_url(url: impl Into<String>) -> Self {
                Self { url: url.into() }
            }
        }

        #[async_trait]
        impl IpProvider for $name {
            fn name(&self) -> &'static str {
                $label
            }

            async fn fetch_ip(&self, client: &Client) -> Result<IpAddr, ProviderError> {
                fetch_plaintext_ip(client, &self.url, $label).await
            }
        }
    };
}

plaintext_provider!(Ipify, "ipify", "https://api.ipify.org");
plaintext_provider!(IcanHazIp, "icanhazip", "https://icanhazip.com");
plaintext_provider!(AwsCheckIp, "aws-checkip", "https://checkip.amazonaws.com");
