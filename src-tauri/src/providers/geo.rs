//! Geolocation via ip-api.com — the primary data source carried over from IPmonitor.

use std::net::IpAddr;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use super::{GeoInfo, GeoProvider, ProviderError};

const LABEL: &str = "ip-api.com";

/// ip-api.com's response shape. It answers HTTP 200 even for failures and
/// signals the real outcome in `status`, so that field drives error handling.
#[derive(Debug, Deserialize)]
struct IpApiResponse {
    status: String,
    message: Option<String>,
    query: Option<String>,
    country: Option<String>,
    #[serde(rename = "countryCode")]
    country_code: Option<String>,
    #[serde(rename = "regionName")]
    region_name: Option<String>,
    city: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    timezone: Option<String>,
    isp: Option<String>,
    org: Option<String>,
    /// Formatted like "AS15169 Google LLC".
    #[serde(rename = "as")]
    as_field: Option<String>,
}

pub struct IpApiCom {
    pub base_url: String,
}

impl Default for IpApiCom {
    fn default() -> Self {
        Self { base_url: "http://ip-api.com/json".to_string() }
    }
}

impl IpApiCom {
    /// Aim the provider at another base URL (wiremock in tests).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self { base_url: base_url.into() }
    }
}

#[async_trait]
impl GeoProvider for IpApiCom {
    fn name(&self) -> &'static str {
        LABEL
    }

    async fn fetch_geo(
        &self,
        client: &Client,
        ip: Option<IpAddr>,
    ) -> Result<GeoInfo, ProviderError> {
        // An empty path asks ip-api to describe the caller's own address.
        let url = match ip {
            Some(addr) => format!("{}/{}", self.base_url.trim_end_matches('/'), addr),
            None => self.base_url.trim_end_matches('/').to_string(),
        };

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(ProviderError::Rejected {
                provider: LABEL,
                message: format!("HTTP {status}"),
            });
        }

        let parsed: IpApiResponse = resp.json().await.map_err(|e| ProviderError::Parse {
            provider: LABEL,
            detail: e.to_string(),
        })?;

        if parsed.status != "success" {
            return Err(ProviderError::Rejected {
                provider: LABEL,
                message: parsed.message.unwrap_or_else(|| parsed.status.clone()),
            });
        }

        Ok(GeoInfo {
            // Prefer the address we asked about; fall back to the echoed query.
            ip: ip.or_else(|| parsed.query.as_deref().and_then(|q| q.parse().ok())),
            country: parsed.country,
            country_code: parsed.country_code,
            region: parsed.region_name,
            city: parsed.city,
            lat: parsed.lat,
            lon: parsed.lon,
            timezone: parsed.timezone,
            isp: parsed.isp,
            org: parsed.org,
            asn: parsed.as_field,
        })
    }
}
