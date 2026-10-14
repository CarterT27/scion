//! SPEC 4.1 `Extensions`: strictly ascending `(tag, value)` pairs; odd tags are critical.

use crate::enc::{Decoder, Encoder};
use scion_core::{Result, ScionError, subcode};

/// SPEC 4.3.3 Meta extension tag 2 (ignorable): the 20-byte git commit id from import.
pub const META_EXT_GIT_COMMIT: u64 = 2;
/// Implementation-defined Meta extension tag 4 (ignorable): skipped gitlinks,
/// `list<(path bytes, 20-byte sha)>` (PLAN 6.4).
pub const META_EXT_SKIPPED_GITLINKS: u64 = 4;

/// Extension list with strictly ascending tags. Decoding rejects odd tags not listed as
/// known; unknown even tags are preserved byte for byte (SPEC 4.1).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Extensions(Vec<(u64, Vec<u8>)>);

impl Extensions {
    /// An empty extension list.
    pub fn new() -> Self {
        Extensions(Vec::new())
    }

    /// The value for `tag`, if present.
    pub fn get(&self, tag: u64) -> Option<&[u8]> {
        self.0
            .binary_search_by_key(&tag, |(t, _)| *t)
            .ok()
            .and_then(|i| self.0.get(i))
            .map(|(_, v)| v.as_slice())
    }

    /// Insert or replace `tag`, keeping tags strictly ascending.
    ///
    /// v1 defines no critical (odd) extension, so an odd tag is rejected with
    /// INVALID_ARGUMENT/unknown_critical_extension: no v1 decoder could read the result
    /// (SPEC 4.1).
    pub fn insert(&mut self, tag: u64, value: Vec<u8>) -> Result<()> {
        if tag % 2 == 1 {
            return Err(ScionError::invalid(
                subcode::UNKNOWN_CRITICAL_EXTENSION,
                format!("extension tag {tag} is critical and unknown in v1"),
            ));
        }
        match self.0.binary_search_by_key(&tag, |(t, _)| *t) {
            Ok(i) => {
                if let Some(slot) = self.0.get_mut(i) {
                    slot.1 = value;
                }
            }
            Err(i) => self.0.insert(i, (tag, value)),
        }
        Ok(())
    }

    /// Remove `tag`, returning its value if it was present.
    pub fn remove(&mut self, tag: u64) -> Option<Vec<u8>> {
        let i = self.0.binary_search_by_key(&tag, |(t, _)| *t).ok()?;
        Some(self.0.remove(i).1)
    }

    /// Number of extensions.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when there are no extensions.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate `(tag, value)` in ascending tag order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.0.iter().map(|(t, v)| (*t, v.as_slice()))
    }

    /// Append the SPEC 4.1 encoding `list<(tag: uvarint, value: bytes)>`.
    pub fn encode_into(&self, e: &mut Encoder) {
        e.uvarint(self.0.len() as u64);
        for (tag, value) in &self.0 {
            e.uvarint(*tag);
            e.bytes(value);
        }
    }

    /// Decode an extension list, rejecting unsorted/duplicate tags and odd tags not in
    /// `known_odd` (unknown_critical_extension).
    pub fn decode_with(d: &mut Decoder<'_>, known_odd: &[u64]) -> Result<Self> {
        let n = d.list_len()?;
        let mut out: Vec<(u64, Vec<u8>)> = Vec::with_capacity(n.min(crate::enc::PREALLOC_CAP));
        for _ in 0..n {
            let tag = d.uvarint()?;
            let value = d.bytes()?;
            if let Some((prev, _)) = out.last() {
                if tag == *prev {
                    return Err(ScionError::invalid(
                        subcode::DUPLICATE,
                        format!("duplicate extension tag {tag}"),
                    ));
                }
                if tag < *prev {
                    return Err(ScionError::invalid(
                        subcode::UNSORTED,
                        format!("extension tag {tag} after {prev}"),
                    ));
                }
            }
            if tag % 2 == 1 && !known_odd.contains(&tag) {
                return Err(ScionError::invalid(
                    subcode::UNKNOWN_CRITICAL_EXTENSION,
                    format!("unknown critical extension tag {tag}"),
                ));
            }
            out.push((tag, value.to_vec()));
        }
        Ok(Extensions(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(pairs: &[(u64, &[u8])]) -> Vec<u8> {
        let mut e = Encoder::new();
        e.uvarint(pairs.len() as u64);
        for (t, v) in pairs {
            e.uvarint(*t);
            e.bytes(v);
        }
        e.finish()
    }

    fn sub(b: &[u8], known: &[u64]) -> String {
        let mut d = Decoder::new(b);
        Extensions::decode_with(&mut d, known)
            .unwrap_err()
            .subcode()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn insert_get_order() {
        let mut x = Extensions::new();
        x.insert(8, b"c".to_vec()).unwrap();
        x.insert(2, b"a".to_vec()).unwrap();
        x.insert(4, b"b".to_vec()).unwrap();
        x.insert(4, b"B".to_vec()).unwrap();
        assert_eq!(
            x.iter().collect::<Vec<_>>(),
            vec![(2, &b"a"[..]), (4, &b"B"[..]), (8, &b"c"[..])]
        );
        assert_eq!(x.get(4), Some(&b"B"[..]));
        assert_eq!(x.get(6), None);
        assert_eq!(
            x.insert(3, vec![]).unwrap_err().subcode(),
            Some(subcode::UNKNOWN_CRITICAL_EXTENSION)
        );
        assert_eq!(x.remove(8), Some(b"c".to_vec()));
        assert_eq!(x.len(), 2);
        let mut e = Encoder::new();
        x.encode_into(&mut e);
        let b = e.finish();
        assert_eq!(b, enc(&[(2, b"a"), (4, b"B")]));
        let mut d = Decoder::new(&b);
        assert_eq!(Extensions::decode_with(&mut d, &[]).unwrap(), x);
        d.finish().unwrap();
    }

    #[test]
    fn decode_rejections() {
        assert_eq!(sub(&enc(&[(4, b""), (2, b"")]), &[]), subcode::UNSORTED);
        assert_eq!(sub(&enc(&[(2, b""), (2, b"")]), &[]), subcode::DUPLICATE);
        assert_eq!(
            sub(&enc(&[(1, b"x")]), &[]),
            subcode::UNKNOWN_CRITICAL_EXTENSION
        );
        let known = enc(&[(1, b"x")]);
        let mut d = Decoder::new(&known);
        let x = Extensions::decode_with(&mut d, &[1]).unwrap();
        assert_eq!(x.get(1), Some(&b"x"[..]));
    }
}
