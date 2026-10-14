//! Serde helpers for scion-object types that carry no serde impls (P00).
//!
//! Paths are serialized losslessly: a UTF-8 path as a plain string, any other path as a map
//! `{ hex = "<lowercase hex>" }`.

use scion_object::{RefName, RepoPath};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PathRepr {
    Text(String),
    Hex { hex: String },
}

fn to_repr(p: &RepoPath) -> PathRepr {
    match std::str::from_utf8(p.as_bytes()) {
        Ok(s) => PathRepr::Text(s.to_owned()),
        Err(_) => PathRepr::Hex {
            hex: hex::encode(p.as_bytes()),
        },
    }
}

fn from_repr<E: serde::de::Error>(r: PathRepr) -> Result<RepoPath, E> {
    let bytes = match r {
        PathRepr::Text(s) => s.into_bytes(),
        PathRepr::Hex { hex } => hex::decode(hex).map_err(E::custom)?,
    };
    RepoPath::parse(&bytes).map_err(E::custom)
}

/// `#[serde(with = "crate::serde_util::path")]`.
#[allow(dead_code)]
pub(crate) mod path {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(p: &RepoPath, s: S) -> Result<S::Ok, S::Error> {
        to_repr(p).serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<RepoPath, D::Error> {
        from_repr(PathRepr::deserialize(d)?)
    }
}

/// `#[serde(with = "crate::serde_util::paths")]` for `Vec<RepoPath>`.
#[allow(dead_code)]
pub(crate) mod paths {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(p: &[RepoPath], s: S) -> Result<S::Ok, S::Error> {
        p.iter().map(to_repr).collect::<Vec<_>>().serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<RepoPath>, D::Error> {
        Vec::<PathRepr>::deserialize(d)?
            .into_iter()
            .map(from_repr)
            .collect()
    }
}

/// `#[serde(with = "crate::serde_util::opt_ref")]` for `Option<RefName>`.
#[allow(dead_code)]
pub(crate) mod opt_ref {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(r: &Option<RefName>, s: S) -> Result<S::Ok, S::Error> {
        r.as_ref().map(|r| r.as_str().to_owned()).serialize(s)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<RefName>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|s| RefName::parse(&s).map_err(D::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use crate::state::{PendingOperation, WcState};
    use scion_core::empty_tree_id;

    #[test]
    fn wc_state_toml_round_trip_without_optional_fields() {
        let s = WcState {
            endpoint: None,
            base: empty_tree_id(),
            tracking_ref: None,
            sparse: Vec::new(),
            inaccessible: Vec::new(),
            pending: Some(PendingOperation::Checkout {
                target: empty_tree_id(),
            }),
        };
        let text = toml::to_string(&s).unwrap();
        let back: WcState = toml::from_str(&text).unwrap();
        assert_eq!(back, s);

        let r = WcState {
            pending: Some(PendingOperation::Rebase {
                original: empty_tree_id(),
                onto: empty_tree_id(),
                remaining_stack: vec![empty_tree_id()],
                done: Vec::new(),
                conflicts: vec![b"a/b".to_vec(), vec![0xff, 0x00]],
                resolved: Vec::new(),
                current: None,
            }),
            ..s
        };
        let text = toml::to_string(&r).unwrap();
        let back: WcState = toml::from_str(&text).unwrap();
        assert_eq!(back, r);
    }
}
