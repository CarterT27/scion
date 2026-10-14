//! Principals and tiers (SPEC 4.3.8, 7.1).

use crate::error::{ErrorCode, Result, ScionError};
use std::fmt;

/// An authenticated identity (SPEC 4.3.8). `group:` principals exist only in policy and
/// never authenticate, so they are not representable here.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Principal {
    /// A human logged in through GitHub; `id` is the stable numeric GitHub id carried in the
    /// token (not part of the id string).
    GitHub {
        /// GitHub login.
        login: String,
        /// Stable numeric GitHub user id, when known.
        id: Option<u64>,
    },
    /// An agent, CI job or importer authenticated with a configured secret.
    Service(String),
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes()
            .all(|b| b.is_ascii_graphic() && b != b':' && b != b'/')
}

impl Principal {
    /// Parse `github:<login>` or `service:<name>` (INVALID_ARGUMENT otherwise).
    pub fn parse(s: &str) -> Result<Self> {
        let invalid = || {
            ScionError::new(
                ErrorCode::InvalidArgument,
                format!("invalid principal {s:?}"),
            )
        };
        if let Some(login) = s.strip_prefix("github:") {
            if valid_name(login) {
                return Ok(Principal::GitHub {
                    login: login.to_owned(),
                    id: None,
                });
            }
        } else if let Some(name) = s.strip_prefix("service:") {
            if valid_name(name) {
                return Ok(Principal::Service(name.to_owned()));
            }
        }
        Err(invalid())
    }

    /// The principal id string: `github:<login>` or `service:<name>`.
    pub fn id_string(&self) -> String {
        match self {
            Principal::GitHub { login, .. } => format!("github:{login}"),
            Principal::Service(name) => format!("service:{name}"),
        }
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id_string())
    }
}

/// The serving tier reported in the `scion-tier` trailer (SPEC 7.1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tier {
    /// The authoritative server.
    Server,
    /// A caching daemon.
    Daemon,
}

impl Tier {
    /// `"server"` or `"daemon"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Server => "server",
            Tier::Daemon => "daemon",
        }
    }

    /// Inverse of [`Tier::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "server" => Some(Tier::Server),
            "daemon" => Some(Tier::Daemon),
            _ => None,
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display() {
        let p = Principal::parse("github:alice").unwrap();
        assert_eq!(
            p,
            Principal::GitHub {
                login: "alice".into(),
                id: None
            }
        );
        assert_eq!(p.to_string(), "github:alice");
        assert_eq!(p.id_string(), "github:alice");
        let with_id = Principal::GitHub {
            login: "alice".into(),
            id: Some(42),
        };
        assert_eq!(with_id.to_string(), "github:alice");
        let s = Principal::parse("service:ci").unwrap();
        assert_eq!(s, Principal::Service("ci".into()));
        assert_eq!(s.to_string(), "service:ci");
        assert_eq!(Principal::parse(&s.id_string()).unwrap(), s);
        for bad in [
            "",
            "alice",
            "github:",
            "service:",
            "group:eng",
            "github:a b",
            "github-id:12",
            "service:a:b",
        ] {
            assert_eq!(
                Principal::parse(bad).unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{bad}"
            );
        }
    }

    #[test]
    fn tiers() {
        assert_eq!(Tier::Server.as_str(), "server");
        assert_eq!(Tier::Daemon.to_string(), "daemon");
        assert_eq!(Tier::parse("daemon"), Some(Tier::Daemon));
        assert_eq!(Tier::parse("x"), None);
    }
}
