//! One-shot live probe against the real endpoints.
//!
//! The unit tests are fully mocked, which proves the parsers handle the shapes
//! we *expect*. This proves the shapes the upstream APIs actually send today —
//! the failure mode mocks structurally cannot catch.
//!
//! Run it deliberately; it makes real network requests:
//!
//! ```sh
//! cargo run --manifest-path src-tauri/Cargo.toml --example probe
//! ```

use ipwatch_lib::netinfo;
use ipwatch_lib::providers::{default_geo_chain, default_ip_chain, http_client};

#[tokio::main]
async fn main() {
    println!("=== local network ===");
    match netinfo::collect() {
        Ok(info) => {
            println!("hostname:     {:?}", info.hostname);
            println!("internal ips: {:?}", info.internal_ips);
            println!("dns servers:  {:?}", info.dns_servers);
            if info.dns_servers.is_empty() {
                println!("  (none reported — check this against `ipconfig /all`)");
            }
        }
        Err(err) => println!("netinfo failed: {err}"),
    }

    let client = match http_client() {
        Ok(client) => client,
        Err(err) => {
            println!("could not build http client: {err}");
            return;
        }
    };

    println!();
    println!("=== external ip (failover chain) ===");
    let ip = match default_ip_chain().fetch_ip(&client).await {
        Ok(ip) => {
            println!("external ip:  {ip}");
            Some(ip)
        }
        Err(err) => {
            println!("all ip providers failed: {err}");
            None
        }
    };

    println!();
    println!("=== geolocation ===");
    match default_geo_chain().fetch_geo(&client, ip).await {
        Ok(geo) => {
            println!("country:      {:?} ({:?})", geo.country, geo.country_code);
            println!("region/city:  {:?} / {:?}", geo.region, geo.city);
            println!("lat/lon:      {:?} / {:?}", geo.lat, geo.lon);
            println!("timezone:     {:?}", geo.timezone);
            println!("isp:          {:?}", geo.isp);
            println!("org:          {:?}", geo.org);
            println!("asn:          {:?}", geo.asn);

            // A None here means the upstream field was absent or renamed —
            // exactly what the mocked tests cannot detect.
            for (label, missing) in [
                ("country_code", geo.country_code.is_none()),
                ("isp", geo.isp.is_none()),
                ("asn", geo.asn.is_none()),
                ("timezone", geo.timezone.is_none()),
            ] {
                if missing {
                    println!("  WARNING: {label} came back empty — upstream shape may have changed");
                }
            }
        }
        Err(err) => println!("all geo providers failed: {err}"),
    }

    println!();
    println!("Sanity-check the country and ISP above against your expected VPN state.");
}
