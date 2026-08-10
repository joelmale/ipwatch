//! External IP and geolocation lookup with an ordered failover chain.
//!
//! Mirrors IPmonitor's `APIManager` primary + backup design: each concern has a
//! list of providers tried in order, and the first success wins. HTML scrapers
//! from the original are deliberately dropped — redundant plaintext APIs are
//! more robust than parsing pages that change shape.

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

mod chain;
mod geo;
mod ip;
#[cfg(test)]
mod tests;

pub use chain::Chain;
pub use geo::IpApiCom;
pub use ip::{AwsCheckIp, IcanHazIp, Ipify};

/// Per-request ceiling so one hung endpoint cannot stall the whole chain.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Builds the HTTP client every provider shares.
pub fn http_client() -> Result<Client, ProviderError> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("ipwatch/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| ProviderError::Http(e.to_string()))
}

/// Geolocation and network-ownership facts about an external address.
///
/// Every field beyond `ip` is optional: the free tiers of these APIs omit
/// fields unpredictably, and a partial answer is still useful for VPN checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GeoInfo {
    pub ip: Option<IpAddr>,
    pub country: Option<String>,
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub timezone: Option<String>,
    pub isp: Option<String>,
    pub org: Option<String>,
    pub asn: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("http error: {0}")]
    Http(String),

    #[error("could not parse response from {provider}: {detail}")]
    Parse { provider: &'static str, detail: String },

    /// The endpoint answered, but told us it would not serve the request
    /// (rate limit, `status: "fail"`, etc.). Distinct from a transport error.
    #[error("{provider} rejected the request: {message}")]
    Rejected { provider: &'static str, message: String },

    #[error("all providers failed: {0:?}")]
    AllFailed(Vec<String>),
}

/// Resolves the external IP address as seen from the public internet.
#[async_trait]
pub trait IpProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch_ip(&self, client: &Client) -> Result<IpAddr, ProviderError>;
}

/// Resolves geolocation / ISP detail, optionally for a specific address.
///
/// Passing `None` asks the endpoint to describe the caller's own address,
/// which lets a geo provider double as a last-resort IP provider.
#[async_trait]
pub trait GeoProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch_geo(
        &self,
        client: &Client,
        ip: Option<IpAddr>,
    ) -> Result<GeoInfo, ProviderError>;
}

/// The default ordered chain for external-IP lookup.
pub fn default_ip_chain() -> Chain<dyn IpProvider> {
    Chain::new(vec![
        Box::new(Ipify::default()),
        Box::new(IcanHazIp::default()),
        Box::new(AwsCheckIp::default()),
    ])
}

/// The default ordered chain for geolocation lookup.
pub fn default_geo_chain() -> Chain<dyn GeoProvider> {
    Chain::new(vec![Box::new(IpApiCom::default())])
}
