//! Ordered failover across a list of providers.

use std::net::IpAddr;

use reqwest::Client;

use super::{GeoInfo, GeoProvider, IpProvider, ProviderError};

/// Tries each provider in order, returning the first success. On total failure
/// it reports every underlying error, so a user seeing "all providers failed"
/// can tell a DNS outage from a rate limit.
///
/// `T` is an unsized trait object (`dyn IpProvider` / `dyn GeoProvider`), which
/// is why the bound is `?Sized` — one implementation serves both chains.
pub struct Chain<T: ?Sized> {
    providers: Vec<Box<T>>,
}

impl<T: ?Sized> Chain<T> {
    pub fn new(providers: Vec<Box<T>>) -> Self {
        Self { providers }
    }

    pub fn providers(&self) -> &[Box<T>] {
        &self.providers
    }

    pub fn len(&self) -> usize {
        self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

impl Chain<dyn IpProvider> {
    /// Returns the first successfully resolved external IP.
    pub async fn fetch_ip(&self, client: &Client) -> Result<IpAddr, ProviderError> {
        let mut errors = Vec::new();

        for provider in &self.providers {
            match provider.fetch_ip(client).await {
                Ok(ip) => {
                    tracing::debug!(provider = provider.name(), %ip, "resolved external ip");
                    return Ok(ip);
                }
                Err(err) => {
                    tracing::warn!(provider = provider.name(), %err, "ip provider failed");
                    errors.push((provider.name(), err));
                }
            }
        }

        Err(all_failed(errors))
    }
}

impl Chain<dyn GeoProvider> {
    /// Returns the first successful geolocation lookup.
    pub async fn fetch_geo(
        &self,
        client: &Client,
        ip: Option<IpAddr>,
    ) -> Result<GeoInfo, ProviderError> {
        let mut errors = Vec::new();

        for provider in &self.providers {
            match provider.fetch_geo(client, ip).await {
                Ok(info) => {
                    tracing::debug!(provider = provider.name(), "resolved geo info");
                    return Ok(info);
                }
                Err(err) => {
                    tracing::warn!(provider = provider.name(), %err, "geo provider failed");
                    errors.push((provider.name(), err));
                }
            }
        }

        Err(all_failed(errors))
    }
}

/// Builds the aggregate error once every provider has been exhausted.
pub(crate) fn all_failed(errors: Vec<(&'static str, ProviderError)>) -> ProviderError {
    ProviderError::AllFailed(
        errors
            .into_iter()
            .map(|(name, err)| format!("{name}: {err}"))
            .collect(),
    )
}
