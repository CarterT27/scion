//! SPEC 4.3.2 Tree.

use crate::canonical::Canonical;
use crate::enc::{Decoder, Encoder};
use crate::extensions::Extensions;
use crate::path::Name;
use scion_core::{Kind, Mode, ObjectId, Result, ScionError, subcode};

/// SPEC 4.3.2 Tree `format_version`.
const TREE_FORMAT_VERSION: u64 = 1;

/// One tree entry (SPEC 4.3.2): Blob id for modes 1-3, Tree id with size 0 for mode 4.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// Entry name.
    pub name: Name,
    /// Entry mode.
    pub mode: Mode,
    /// Entry object id.
    pub id: ObjectId,
    /// Blob length (modes 1-3) or 0 (mode 4).
    pub size: u64,
}

impl TreeEntry {
    /// SPEC 4.3.2 mode/kind agreement: modes 1-3 need a Blob id (invalid_kind), mode 4 a
    /// Tree id (invalid_kind) with size 0 (invalid_mode).
    pub fn validate(&self) -> Result<()> {
        let want = if self.mode.is_file_like() {
            Kind::Blob
        } else {
            Kind::Tree
        };
        if self.id.kind() != want {
            return Err(ScionError::invalid(
                subcode::INVALID_KIND,
                format!(
                    "tree entry of mode {} needs a {:?} id, got {:?}",
                    self.mode as u8,
                    want,
                    self.id.kind()
                ),
            ));
        }
        if self.mode == Mode::Tree && self.size != 0 {
            return Err(ScionError::invalid(
                subcode::INVALID_MODE,
                format!("tree entry of mode 4 must have size 0, got {}", self.size),
            ));
        }
        Ok(())
    }
}

/// SPEC 4.3.2 Tree: `format_version = 1`, entries strictly ascending by name, extensions.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Tree {
    /// Entries, strictly ascending by bytewise name.
    pub entries: Vec<TreeEntry>,
    /// Extensions (final field).
    pub extensions: Extensions,
}

/// Strictly ascending bytewise order check (unsorted / duplicate).
pub(crate) fn check_order(prev: &[u8], next: &[u8], what: &str) -> Result<()> {
    match prev.cmp(next) {
        std::cmp::Ordering::Less => Ok(()),
        std::cmp::Ordering::Equal => Err(ScionError::invalid(
            subcode::DUPLICATE,
            format!("duplicate {what}"),
        )),
        std::cmp::Ordering::Greater => Err(ScionError::invalid(
            subcode::UNSORTED,
            format!("{what}s not strictly ascending"),
        )),
    }
}

impl Canonical for Tree {
    const KIND: Kind = Kind::Tree;

    fn encode_into(&self, e: &mut Encoder) {
        e.uvarint(TREE_FORMAT_VERSION);
        e.uvarint(self.entries.len() as u64);
        for entry in &self.entries {
            e.bytes(entry.name.as_bytes());
            e.u8(entry.mode as u8);
            e.id(&entry.id);
            e.uvarint(entry.size);
        }
        self.extensions.encode_into(e);
    }

    /// Validates order, uniqueness, names, mode/kind agreement and size 0 for mode 4.
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self> {
        d.format_version(TREE_FORMAT_VERSION)?;
        let n = d.list_len()?;
        let mut entries: Vec<TreeEntry> = Vec::with_capacity(n);
        for _ in 0..n {
            let name = Name::new(d.bytes()?)?;
            let mode = Mode::from_u8(d.u8()?)?;
            let id = d.id()?;
            let size = d.uvarint()?;
            if let Some(prev) = entries.last() {
                check_order(prev.name.as_bytes(), name.as_bytes(), "tree entry name")?;
            }
            let entry = TreeEntry {
                name,
                mode,
                id,
                size,
            };
            entry.validate()?;
            entries.push(entry);
        }
        let extensions = Extensions::decode_with(d, &[])?;
        Ok(Tree {
            entries,
            extensions,
        })
    }
}

impl Tree {
    /// The empty tree (valid only as a root; SPEC 4.3.2).
    pub fn empty() -> Self {
        Tree::default()
    }

    /// The entry named `name`, if present (binary search; entries must be sorted).
    pub fn get(&self, name: &[u8]) -> Option<&TreeEntry> {
        self.entries
            .binary_search_by(|e| e.name.as_bytes().cmp(name))
            .ok()
            .and_then(|i| self.entries.get(i))
    }

    /// Every rule [`Canonical::decode`] enforces, for a constructed value: entries strictly
    /// ascending and unique, mode/kind agreement, size 0 for mode 4, no odd extension tag
    /// (SPEC 4.1, 4.3.2).
    pub fn validate(&self) -> Result<()> {
        for pair in self.entries.windows(2) {
            if let [a, b] = pair {
                check_order(a.name.as_bytes(), b.name.as_bytes(), "tree entry name")?;
            }
        }
        for entry in &self.entries {
            entry.validate()?;
        }
        if let Some((tag, _)) = self.extensions.iter().find(|(t, _)| t % 2 == 1) {
            return Err(ScionError::invalid(
                subcode::UNKNOWN_CRITICAL_EXTENSION,
                format!("unknown critical extension tag {tag}"),
            ));
        }
        Ok(())
    }

    /// A non-root tree must have at least one entry (INVALID_ARGUMENT/empty_tree).
    pub fn validate_non_root(&self) -> Result<()> {
        if self.entries.is_empty() {
            return Err(ScionError::invalid(
                subcode::EMPTY_TREE,
                "a non-root tree must have at least one entry",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::empty_tree_id;

    fn entry(name: &str, mode: Mode, id: ObjectId, size: u64) -> TreeEntry {
        TreeEntry {
            name: Name::new(name.as_bytes()).unwrap(),
            mode,
            id,
            size,
        }
    }

    #[test]
    fn empty_tree_matches_core() {
        let t = Tree::empty();
        assert_eq!(t.encode(), [0x01, 0x00, 0x00]);
        assert_eq!(t.id(), empty_tree_id());
        assert_eq!(Tree::decode(&[1, 0, 0]).unwrap(), t);
        assert_eq!(
            t.validate_non_root().unwrap_err().subcode(),
            Some(subcode::EMPTY_TREE)
        );
    }

    #[test]
    fn get_and_validate() {
        let blob = ObjectId::hash_blob(b"x");
        let t = Tree {
            entries: vec![
                entry("a", Mode::File, blob, 1),
                entry("b", Mode::Tree, empty_tree_id(), 0),
                entry("c", Mode::Symlink, blob, 1),
            ],
            extensions: Extensions::new(),
        };
        t.validate().unwrap();
        t.validate_non_root().unwrap();
        assert_eq!(t.get(b"b").unwrap().mode, Mode::Tree);
        assert!(t.get(b"d").is_none());
        let back = Tree::decode(&t.encode()).unwrap();
        assert_eq!(back, t);

        let mut bad = t.clone();
        bad.entries.swap(0, 1);
        assert_eq!(
            bad.validate().unwrap_err().subcode(),
            Some(subcode::UNSORTED)
        );
        let mut bad = t.clone();
        bad.entries[1].size = 3;
        assert_eq!(
            bad.validate().unwrap_err().subcode(),
            Some(subcode::INVALID_MODE)
        );
        let mut bad = t;
        bad.entries[0].id = empty_tree_id();
        assert_eq!(
            bad.validate().unwrap_err().subcode(),
            Some(subcode::INVALID_KIND)
        );
    }
}
