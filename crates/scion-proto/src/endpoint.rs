//! Endpoint parsing and TLS options (SPEC 6.4, 7.1; P08).

use scion_core::{ErrorCode, Result, ScionError};
use std::path::PathBuf;

/// Where a client connects: a daemon Unix socket or an HTTPS server (SPEC 6.4).
#[derive(Clone, Debug)]
pub enum Endpoint {
    /// `unix:///abs/path`.
    Unix(PathBuf),
    /// `https://host:port`.
    Https(String),
}

fn bad(s: &str, why: &str) -> ScionError {
    ScionError::new(
        ErrorCode::InvalidArgument,
        format!("invalid endpoint {s:?}: {why} (expected unix:///abs/path or https://host:port)"),
    )
}

impl Endpoint {
    /// Parse `unix:///abs/path` or `https://host:port` (INVALID_ARGUMENT otherwise).
    ///
    /// The HTTPS form keeps its canonical text `https://host[:port]` (a single trailing `/` is
    /// dropped); a path, query, userinfo or empty host is rejected. Plain `http://` is rejected
    /// because TLS is REQUIRED between clients and servers (SPEC 17).
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(path) = s.strip_prefix("unix://") {
            if !path.starts_with('/') {
                return Err(bad(s, "unix socket path must be absolute"));
            }
            if path.len() < 2 || path.contains('\0') {
                return Err(bad(s, "empty or invalid unix socket path"));
            }
            return Ok(Endpoint::Unix(PathBuf::from(path)));
        }
        if let Some(rest) = s.strip_prefix("https://") {
            let authority = rest.strip_suffix('/').unwrap_or(rest);
            if authority.is_empty() {
                return Err(bad(s, "missing host"));
            }
            if authority.contains(['/', '?', '#', '@']) || authority.contains(char::is_whitespace) {
                return Err(bad(s, "only host and port are allowed"));
            }
            let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
                let (h, after) = rest
                    .split_once(']')
                    .ok_or_else(|| bad(s, "unterminated IPv6 literal"))?;
                if h.parse::<std::net::Ipv6Addr>().is_err() {
                    return Err(bad(s, "malformed IPv6 literal"));
                }
                match after {
                    "" => (h, None),
                    _ => match after.strip_prefix(':') {
                        Some(p) => (h, Some(p)),
                        None => return Err(bad(s, "malformed host or port")),
                    },
                }
            } else {
                match authority.split_once(':') {
                    Some((h, p)) => (h, Some(p)),
                    None => (authority, None),
                }
            };
            let host_ok = !host.is_empty()
                && (authority.starts_with('[')
                    || host
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_')));
            if !host_ok {
                return Err(bad(s, "missing or malformed host"));
            }
            if let Some(p) = port {
                match p.parse::<u16>() {
                    Ok(n) if n != 0 && p.bytes().all(|b| b.is_ascii_digit()) => {}
                    _ => return Err(bad(s, "port must be 1-65535")),
                }
            }
            return Ok(Endpoint::Https(format!("https://{authority}")));
        }
        Err(bad(s, "unsupported scheme"))
    }
}

impl std::fmt::Display for Endpoint {
    /// The text form accepted by [`Endpoint::parse`] (SPEC 6.4).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Unix(p) => write!(f, "unix://{}", p.display()),
            Endpoint::Https(u) => f.write_str(u),
        }
    }
}

/// TLS client options; without a CA the native roots are used (SPEC 17).
#[derive(Clone, Debug, Default)]
pub struct TlsOptions {
    /// PEM-encoded CA certificate(s) to trust.
    pub ca_pem: Option<Vec<u8>>,
    /// Server name override for certificate verification.
    pub domain: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid() {
        match Endpoint::parse("unix:///tmp/scion.sock").unwrap() {
            Endpoint::Unix(p) => assert_eq!(p, PathBuf::from("/tmp/scion.sock")),
            e => panic!("{e:?}"),
        }
        for (s, want) in [
            ("https://localhost:8443", "https://localhost:8443"),
            (
                "https://scion.example.com:443/",
                "https://scion.example.com:443",
            ),
            ("https://127.0.0.1:1", "https://127.0.0.1:1"),
            ("https://[::1]:9000", "https://[::1]:9000"),
            ("https://example.com", "https://example.com"),
        ] {
            match Endpoint::parse(s).unwrap() {
                Endpoint::Https(u) => assert_eq!(u, want),
                e => panic!("{e:?}"),
            }
            assert_eq!(Endpoint::parse(s).unwrap().to_string(), want);
        }
    }

    #[test]
    fn rejects_invalid() {
        for s in [
            "",
            "localhost:443",
            "http://localhost:80",
            "unix://relative/path",
            "unix://",
            "unix:///",
            "https://",
            "https://:443",
            "https://host:",
            "https://host:0",
            "https://host:99999",
            "https://host:44a",
            "https://host/path",
            "https://user@host:1",
            "https://host:1?q",
            "grpc://host:1",
        ] {
            let e = Endpoint::parse(s).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{s}");
        }
    }
}
