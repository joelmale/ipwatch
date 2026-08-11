//! Local network facts: internal addresses, configured DNS resolvers, hostname.
//!
//! Purely local — no network I/O. Platform-specific pieces are `cfg`-gated.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NetInfo {
    /// Non-loopback addresses assigned to local interfaces.
    pub internal_ips: Vec<IpAddr>,
    /// Resolvers the system is configured to query. A VPN that is working
    /// normally replaces these with its own; leftover ISP resolvers here are
    /// the first hint of a DNS leak.
    pub dns_servers: Vec<IpAddr>,
    pub hostname: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum NetInfoError {
    #[error("could not enumerate local interfaces: {0}")]
    Interfaces(String),

    #[error("could not read DNS configuration: {0}")]
    Dns(String),
}

/// Collects everything the details window shows about the local machine.
pub fn collect() -> Result<NetInfo, NetInfoError> {
    let internal_ips = collect_internal_ips()?;

    let dns_servers = collect_dns_servers().unwrap_or_else(|err| {
        tracing::warn!("could not read DNS configuration: {err}");
        Vec::new()
    });

    let hostname = match gethostname::gethostname().into_string() {
        Ok(name) => Some(name),
        Err(os_str) => {
            tracing::warn!("hostname was not valid UTF-8, using lossy conversion");
            Some(os_str.to_string_lossy().into_owned())
        }
    };

    Ok(NetInfo {
        internal_ips,
        dns_servers,
        hostname,
    })
}

/// Enumerates non-loopback addresses assigned to local interfaces.
fn collect_internal_ips() -> Result<Vec<IpAddr>, NetInfoError> {
    let netifas = local_ip_address::list_afinet_netifas()
        .map_err(|err| NetInfoError::Interfaces(err.to_string()))?;

    Ok(netifas
        .into_iter()
        .map(|(_name, addr)| addr)
        .filter(|addr| !addr.is_loopback())
        .collect())
}

/// Collects the system's configured DNS resolvers. A failure here is
/// deliberately kept out of `NetInfoError` — see module docs on `collect`.
#[cfg(windows)]
fn collect_dns_servers() -> Result<Vec<IpAddr>, String> {
    let adapters = ipconfig::get_adapters().map_err(|err| err.to_string())?;

    let mut servers = Vec::new();
    for adapter in adapters {
        if adapter.oper_status() != ipconfig::OperStatus::IfOperStatusUp {
            continue;
        }
        for dns in adapter.dns_servers() {
            if !servers.contains(dns) {
                servers.push(*dns);
            }
        }
    }

    Ok(servers)
}

#[cfg(unix)]
fn collect_dns_servers() -> Result<Vec<IpAddr>, String> {
    match std::fs::read_to_string("/etc/resolv.conf") {
        Ok(contents) => Ok(parse_resolv_conf(&contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err.to_string()),
    }
}

/// Parses `nameserver <addr>` lines out of a `resolv.conf`-formatted string.
///
/// Deliberately *not* `cfg(unix)`-gated: only the file-reading half of DNS
/// collection is platform-specific. Keeping this pure function available on
/// every target lets its unit tests run on Windows CI too, even though the
/// function is otherwise only called from the unix `collect_dns_servers`.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_resolv_conf(contents: &str) -> Vec<IpAddr> {
    let mut servers = Vec::new();

    for raw_line in contents.lines() {
        let line = raw_line.trim();

        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        let mut parts = line.split_whitespace();
        let directive = match parts.next() {
            Some(d) => d,
            None => continue,
        };

        if directive != "nameserver" {
            continue;
        }

        if let Some(addr_str) = parts.next() {
            if let Ok(addr) = addr_str.parse::<IpAddr>() {
                servers.push(addr);
            }
        }
    }

    servers
}

#[cfg(test)]
mod tests {
    use super::*;

    mod resolv_conf_parsing {
        use super::*;

        #[test]
        fn parses_multiple_nameservers_in_order() {
            let input = "nameserver 1.1.1.1\nnameserver 8.8.8.8\n";
            assert_eq!(
                parse_resolv_conf(input),
                vec![
                    "1.1.1.1".parse::<IpAddr>().unwrap(),
                    "8.8.8.8".parse::<IpAddr>().unwrap(),
                ]
            );
        }

        #[test]
        fn ignores_comments_blank_lines_and_other_directives() {
            let input = "\
# a comment
; also a comment

search example.com
domain example.com
options timeout:2
nameserver 9.9.9.9
";
            assert_eq!(
                parse_resolv_conf(input),
                vec!["9.9.9.9".parse::<IpAddr>().unwrap()]
            );
        }

        #[test]
        fn tolerates_extra_and_tab_whitespace() {
            let input = "  nameserver\t\t127.0.0.53  \n\tnameserver   10.0.0.1\t\n";
            assert_eq!(
                parse_resolv_conf(input),
                vec![
                    "127.0.0.53".parse::<IpAddr>().unwrap(),
                    "10.0.0.1".parse::<IpAddr>().unwrap(),
                ]
            );
        }

        #[test]
        fn parses_ipv6_nameserver() {
            let input = "nameserver 2001:4860:4860::8888\n";
            assert_eq!(
                parse_resolv_conf(input),
                vec!["2001:4860:4860::8888".parse::<IpAddr>().unwrap()]
            );
        }

        #[test]
        fn skips_malformed_nameserver_line() {
            let input = "nameserver not-an-address\nnameserver 4.4.4.4\n";
            assert_eq!(
                parse_resolv_conf(input),
                vec!["4.4.4.4".parse::<IpAddr>().unwrap()]
            );
        }

        #[test]
        fn empty_input_yields_empty_vec() {
            assert_eq!(parse_resolv_conf(""), Vec::<IpAddr>::new());
        }
    }

    #[test]
    fn collect_smoke_test() {
        let info = collect().expect("collect() should succeed");
        assert!(info.hostname.is_some(), "expected a hostname to be present");
    }
}
