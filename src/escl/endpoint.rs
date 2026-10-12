//! Network location of an eSCL service.

use std::fmt;
use std::net::IpAddr;

/// The resource path most scanners use (the mDNS `rs` TXT key).
pub const DEFAULT_ROOT: &str = "eSCL";

/// Where an eSCL service can be reached, e.g. `http://192.0.2.10:80/eSCL`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    /// Host name or IP address literal (without brackets).
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// Resource path without leading or trailing slashes, usually `eSCL`.
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointParseError {
    #[error("the scanner address is empty")]
    Empty,
    #[error("unsupported URL scheme '{0}', expected http or https")]
    Scheme(String),
    #[error("invalid port '{0}'")]
    Port(String),
    #[error("invalid host '{0}'")]
    Host(String),
}

impl Endpoint {
    pub fn new(host: impl Into<String>, port: u16, tls: bool, root: &str) -> Self {
        let root = root.trim_matches('/');
        Endpoint {
            host: host.into(),
            port,
            tls,
            root: if root.is_empty() { DEFAULT_ROOT } else { root }.to_owned(),
        }
    }

    /// Parses user input such as `192.0.2.10`, `printer.local:8080`,
    /// `[2001:db8::1]` or `https://printer.local/eSCL`.
    pub fn parse(input: &str) -> Result<Self, EndpointParseError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(EndpointParseError::Empty);
        }
        let (tls, rest) = match input.split_once("://") {
            Some((scheme, rest)) => match scheme.to_ascii_lowercase().as_str() {
                "http" => (false, rest),
                "https" => (true, rest),
                other => return Err(EndpointParseError::Scheme(other.to_owned())),
            },
            None => (false, input),
        };
        let (authority, root) = rest.split_once('/').unwrap_or((rest, ""));
        let default_port = if tls { 443 } else { 80 };

        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (host, after) = bracketed
                .split_once(']')
                .ok_or_else(|| EndpointParseError::Host(authority.to_owned()))?;
            let port = match after.strip_prefix(':') {
                Some(port) => parse_port(port)?,
                None if after.is_empty() => default_port,
                None => return Err(EndpointParseError::Host(authority.to_owned())),
            };
            (host, port)
        } else if authority.parse::<IpAddr>().is_ok() {
            // A bare IPv6 address: its colons are not a port separator.
            (authority, default_port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host, parse_port(port)?),
                None => (authority, default_port),
            }
        };
        if host.is_empty() || host.contains(|c: char| c.is_whitespace() || c == '@') {
            return Err(EndpointParseError::Host(host.to_owned()));
        }
        Ok(Endpoint::new(host, port, tls, root))
    }

    /// `scheme://host:port`, with IPv6 literals bracketed.
    pub fn origin(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        if self.host.contains(':') {
            format!("{scheme}://[{}]:{}", self.host, self.port)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }

    /// The URL that eSCL resource names are appended to.
    pub fn base_url(&self) -> String {
        format!("{}/{}", self.origin(), self.root)
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base_url())
    }
}

fn parse_port(port: &str) -> Result<u16, EndpointParseError> {
    port.parse()
        .map_err(|_| EndpointParseError::Port(port.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Endpoint {
        Endpoint::parse(input).unwrap()
    }

    #[test]
    fn parses_bare_hosts() {
        assert_eq!(
            parse("192.0.2.10"),
            Endpoint::new("192.0.2.10", 80, false, "eSCL")
        );
        assert_eq!(
            parse(" scanner.local:8080 "),
            Endpoint::new("scanner.local", 8080, false, "eSCL")
        );
        assert_eq!(
            parse("2001:db8::1"),
            Endpoint::new("2001:db8::1", 80, false, "eSCL")
        );
        assert_eq!(
            parse("[2001:db8::1]:8080"),
            Endpoint::new("2001:db8::1", 8080, false, "eSCL")
        );
    }

    #[test]
    fn parses_urls() {
        assert_eq!(
            parse("https://scanner.local"),
            Endpoint::new("scanner.local", 443, true, "eSCL")
        );
        assert_eq!(
            parse("HTTP://192.0.2.10:81/custom/"),
            Endpoint::new("192.0.2.10", 81, false, "custom")
        );
        assert_eq!(
            parse("https://[2001:db8::1]/eSCL"),
            Endpoint::new("2001:db8::1", 443, true, "eSCL")
        );
    }

    #[test]
    fn rejects_invalid_input() {
        assert_eq!(Endpoint::parse("  "), Err(EndpointParseError::Empty));
        assert!(matches!(
            Endpoint::parse("ftp://x"),
            Err(EndpointParseError::Scheme(_))
        ));
        assert!(matches!(
            Endpoint::parse("host:99999"),
            Err(EndpointParseError::Port(_))
        ));
        assert!(matches!(
            Endpoint::parse("[::1"),
            Err(EndpointParseError::Host(_))
        ));
        assert!(matches!(
            Endpoint::parse("http://:80"),
            Err(EndpointParseError::Host(_))
        ));
    }

    #[test]
    fn formats_urls() {
        assert_eq!(parse("192.0.2.10").base_url(), "http://192.0.2.10:80/eSCL");
        assert_eq!(
            parse("https://[2001:db8::1]").origin(),
            "https://[2001:db8::1]:443"
        );
    }
}
