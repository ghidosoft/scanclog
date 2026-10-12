//! Scanner discovery over mDNS / DNS-SD (`_uscan._tcp` and `_uscans._tcp`).

use super::endpoint::{DEFAULT_ROOT, Endpoint};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use std::net::IpAddr;
use std::time::{Duration, Instant};

const SERVICE_HTTP: &str = "_uscan._tcp.local.";
const SERVICE_HTTPS: &str = "_uscans._tcp.local.";

/// A scanner found on the network, possibly reachable through several endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannerInfo {
    /// Human-readable name, e.g. "Example Inkjet MFP".
    pub name: String,
    /// Stable identifier from the TXT record, used to merge announcements.
    pub uuid: Option<String>,
    /// Endpoints in order of preference: plain HTTP first.
    pub endpoints: Vec<Endpoint>,
}

impl ScannerInfo {
    /// A scanner entered by hand rather than discovered.
    pub fn manual(endpoint: Endpoint) -> Self {
        ScannerInfo {
            name: endpoint.host.clone(),
            uuid: None,
            endpoints: vec![endpoint],
        }
    }

    fn merge(&mut self, other: ScannerInfo) {
        for endpoint in other.endpoints {
            if !self.endpoints.contains(&endpoint) {
                self.endpoints.push(endpoint);
            }
        }
        // Plain HTTP avoids certificate problems with self-signed devices.
        self.endpoints.sort_by_key(|e| e.tls);
    }

    fn same_device(&self, other: &ScannerInfo) -> bool {
        match (&self.uuid, &other.uuid) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => self.name == other.name,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("mDNS discovery failed: {0}")]
pub struct DiscoveryError(#[from] mdns_sd::Error);

/// Browses the local network for eSCL scanners for `timeout`.
///
/// `on_update` is called with the full, merged list each time it changes, so
/// a UI can show scanners as soon as they answer.
pub fn discover(
    timeout: Duration,
    mut on_update: impl FnMut(&[ScannerInfo]),
) -> Result<Vec<ScannerInfo>, DiscoveryError> {
    log::debug!("starting mDNS discovery");
    let daemon = ServiceDaemon::new()?;
    let receivers = [daemon.browse(SERVICE_HTTP)?, daemon.browse(SERVICE_HTTPS)?];
    let deadline = Instant::now() + timeout;
    let mut scanners: Vec<ScannerInfo> = Vec::new();

    'outer: loop {
        for receiver in &receivers {
            let now = Instant::now();
            if now >= deadline {
                break 'outer;
            }
            // Short slices keep both receivers responsive.
            let slice = (deadline - now).min(Duration::from_millis(50));
            let Ok(ServiceEvent::ServiceResolved(service)) = receiver.recv_timeout(slice) else {
                continue;
            };
            log::debug!(
                "resolved {} at {}:{}",
                service.fullname,
                service.host,
                service.port
            );
            let Some(found) = scanner_from_service(&service) else {
                continue;
            };
            match scanners.iter_mut().find(|s| s.same_device(&found)) {
                Some(existing) => {
                    let before = existing.clone();
                    existing.merge(found);
                    if *existing == before {
                        continue;
                    }
                }
                None => scanners.push(found),
            }
            on_update(&scanners);
        }
    }

    log::debug!("mDNS discovery found {} scanner(s)", scanners.len());
    // Shutdown failures only mean the daemon thread is already gone.
    let _ = daemon.shutdown();
    Ok(scanners)
}

fn scanner_from_service(service: &ResolvedService) -> Option<ScannerInfo> {
    let tls = service.ty_domain == SERVICE_HTTPS;
    let root = service.get_property_val_str("rs").unwrap_or(DEFAULT_ROOT);
    let host = preferred_host(service)?;
    let instance = service
        .fullname
        .strip_suffix(&format!(".{}", service.ty_domain))
        .unwrap_or(&service.fullname);
    let name = service
        .get_property_val_str("ty")
        .filter(|ty| !ty.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| unescape_instance_name(instance));
    Some(ScannerInfo {
        name,
        uuid: service.get_property_val_str("UUID").map(str::to_owned),
        endpoints: vec![Endpoint::new(host, service.port, tls, root)],
    })
}

/// Picks an address that works in a URL without extra configuration:
/// IPv4 first, then routable IPv6, then the mDNS host name.
fn preferred_host(service: &ResolvedService) -> Option<String> {
    let addresses: Vec<IpAddr> = service.addresses.iter().map(|a| a.to_ip_addr()).collect();
    let mut v4: Vec<_> = addresses.iter().filter(|a| a.is_ipv4()).collect();
    v4.sort();
    if let Some(addr) = v4.first() {
        return Some(addr.to_string());
    }
    // Link-local IPv6 would need a zone index, which HTTP URLs handle poorly.
    let mut v6: Vec<_> = addresses
        .iter()
        .filter(|a| matches!(a, IpAddr::V6(v6) if !v6.is_unicast_link_local()))
        .collect();
    v6.sort();
    if let Some(addr) = v6.first() {
        return Some(addr.to_string());
    }
    let host = service.host.trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_owned())
}

/// DNS-SD instance names escape spaces and punctuation as `\032` or `\.`.
fn unescape_instance_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = std::iter::from_fn(|| chars.next_if(char::is_ascii_digit))
            .take(3)
            .collect();
        match digits.parse::<u8>() {
            Ok(code) if digits.len() == 3 => out.push(char::from(code)),
            _ => {
                out.push_str(&digits);
                if digits.is_empty() {
                    out.extend(chars.next());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_instance_names() {
        assert_eq!(
            unescape_instance_name(r"Example\032Inkjet\032MFP"),
            "Example Inkjet MFP"
        );
        assert_eq!(unescape_instance_name(r"a\.b\\c"), r"a.b\c");
        assert_eq!(unescape_instance_name("plain"), "plain");
    }

    #[test]
    fn merges_endpoints_preferring_http() {
        let https = Endpoint::new("192.0.2.10", 443, true, "eSCL");
        let http = Endpoint::new("192.0.2.10", 80, false, "eSCL");
        let mut a = ScannerInfo {
            name: "x".into(),
            uuid: Some("ABC".into()),
            endpoints: vec![https.clone()],
        };
        let b = ScannerInfo {
            name: "y".into(),
            uuid: Some("abc".into()),
            endpoints: vec![http.clone()],
        };
        assert!(a.same_device(&b));
        a.merge(b);
        assert_eq!(a.endpoints, vec![http, https]);
    }
}
