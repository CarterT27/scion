//! SPEC 9.2 history node, derived kind `0x10`.
//!
//! Encoding (SPEC 4.1 primitives, P01):
//!
//! ```text
//! HistoryNode {
//!   format_version: uvarint = 1
//!   kind: u8                         // 1 File, 2 Dir
//!   content: ObjectId                // kind Blob (File) or Tree (Dir)
//!   changeset: ObjectId              // kind Changeset
//!   parents: list<ObjectId>          // kind Derived(0x10); unique; parent order
//!   children: list<(name: bytes, node: ObjectId)>  // strictly ascending by name; empty for File
//!   extensions: Extensions
//! }
//! ```

use crate::canonical::Canonical;
use crate::enc::{Decoder, Encoder};
use crate::extensions::Extensions;
use crate::path::Name;
use crate::tree::check_order;
use scion_core::{Kind, ObjectId, Result, ScionError, subcode};
use std::collections::BTreeSet;

/// SPEC 9.2 HistoryNode `format_version`.
const HISTORY_NODE_FORMAT_VERSION: u64 = 1;
/// The derived kind byte of history nodes (SPEC 9.2).
const HISTORY_NODE_KIND: Kind = Kind::Derived(0x10);

/// SPEC 9.2 node kind: 1 File, 2 Dir.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A file node.
    File = 1,
    /// A directory node.
    Dir = 2,
}

impl NodeKind {
    /// Parse the kind byte (INVALID_ARGUMENT/invalid_kind otherwise).
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            1 => Ok(NodeKind::File),
            2 => Ok(NodeKind::Dir),
            _ => Err(ScionError::invalid(
                subcode::INVALID_KIND,
                format!("invalid history node kind {v}"),
            )),
        }
    }

    /// The object kind of `content` for this node kind: Blob (File) or Tree (Dir).
    pub fn content_kind(self) -> Kind {
        match self {
            NodeKind::File => Kind::Blob,
            NodeKind::Dir => Kind::Tree,
        }
    }
}

/// SPEC 9.2 `HistoryNode { kind, content, changeset, parents, children (Dir), extensions }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryNode {
    /// File or Dir.
    pub kind: NodeKind,
    /// Blob id (File) or Tree id (Dir).
    pub content: ObjectId,
    /// The changeset that introduced this content at this path.
    pub changeset: ObjectId,
    /// Nodes for this path in each parent where it exists (deduplicated, parent order).
    pub parents: Vec<ObjectId>,
    /// Dir only: `(name, node id)` ascending by name; empty for File.
    pub children: Vec<(Name, ObjectId)>,
    /// Extensions (final field).
    pub extensions: Extensions,
}

fn expect_kind(id: &ObjectId, kind: Kind, what: &str) -> Result<()> {
    if id.kind() != kind {
        return Err(ScionError::invalid(
            subcode::INVALID_KIND,
            format!(
                "history node {what} must be a {kind:?} id, got {:?}",
                id.kind()
            ),
        ));
    }
    Ok(())
}

impl Canonical for HistoryNode {
    const KIND: Kind = HISTORY_NODE_KIND;

    fn encode_into(&self, e: &mut Encoder) {
        e.uvarint(HISTORY_NODE_FORMAT_VERSION);
        e.u8(self.kind as u8);
        e.id(&self.content);
        e.id(&self.changeset);
        e.uvarint(self.parents.len() as u64);
        for p in &self.parents {
            e.id(p);
        }
        e.uvarint(self.children.len() as u64);
        for (name, id) in &self.children {
            e.bytes(name.as_bytes());
            e.id(id);
        }
        self.extensions.encode_into(e);
    }

    /// Validates the kind byte, id kinds, parent uniqueness (duplicate), children strictly
    /// ascending (unsorted / duplicate), valid names, and no children on a File node
    /// (invalid_kind).
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self> {
        d.format_version(HISTORY_NODE_FORMAT_VERSION)?;
        let kind = NodeKind::from_u8(d.u8()?)?;
        let content = d.id()?;
        expect_kind(&content, kind.content_kind(), "content")?;
        let changeset = d.id()?;
        expect_kind(&changeset, Kind::Changeset, "changeset")?;
        let n = d.list_len()?;
        let mut parents = Vec::with_capacity(n.min(crate::enc::PREALLOC_CAP));
        let mut seen = BTreeSet::new();
        for _ in 0..n {
            let p = d.id()?;
            expect_kind(&p, HISTORY_NODE_KIND, "parent")?;
            if !seen.insert(p) {
                return Err(ScionError::invalid(
                    subcode::DUPLICATE,
                    format!("duplicate history node parent {p}"),
                ));
            }
            parents.push(p);
        }
        let n = d.list_len()?;
        if n > 0 && kind == NodeKind::File {
            return Err(ScionError::invalid(
                subcode::INVALID_KIND,
                "a File history node has no children",
            ));
        }
        let mut children: Vec<(Name, ObjectId)> =
            Vec::with_capacity(n.min(crate::enc::PREALLOC_CAP));
        for _ in 0..n {
            let name = Name::new(d.bytes()?)?;
            let id = d.id()?;
            expect_kind(&id, HISTORY_NODE_KIND, "child")?;
            if let Some((prev, _)) = children.last() {
                check_order(prev.as_bytes(), name.as_bytes(), "history node child name")?;
            }
            children.push((name, id));
        }
        let extensions = Extensions::decode_with(d, &[])?;
        Ok(HistoryNode {
            kind,
            content,
            changeset,
            parents,
            children,
            extensions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::empty_tree_id;

    fn node(n: u8) -> ObjectId {
        ObjectId::new(HISTORY_NODE_KIND, [n; 32])
    }

    #[test]
    fn round_trip_and_rules() {
        let cs = ObjectId::new(Kind::Changeset, [1; 32]);
        let dir = HistoryNode {
            kind: NodeKind::Dir,
            content: empty_tree_id(),
            changeset: cs,
            parents: vec![node(2), node(3)],
            children: vec![
                (Name::new(b"a").unwrap(), node(4)),
                (Name::new(b"b").unwrap(), node(5)),
            ],
            extensions: Extensions::new(),
        };
        let b = dir.encode();
        assert_eq!(HistoryNode::decode(&b).unwrap(), dir);
        assert_eq!(dir.id().kind(), Kind::Derived(0x10));
        assert!(dir.id().to_text().starts_with("x1"));

        let mut file = dir.clone();
        file.kind = NodeKind::File;
        file.content = ObjectId::hash_blob(b"x");
        let e = HistoryNode::decode(&file.encode()).unwrap_err();
        assert_eq!(e.subcode(), Some(subcode::INVALID_KIND));
        file.children.clear();
        assert_eq!(HistoryNode::decode(&file.encode()).unwrap(), file);

        let mut swapped = dir.clone();
        swapped.children.swap(0, 1);
        assert_eq!(
            HistoryNode::decode(&swapped.encode())
                .unwrap_err()
                .subcode(),
            Some(subcode::UNSORTED)
        );
        let mut dup = dir.clone();
        dup.parents = vec![node(2), node(2)];
        assert_eq!(
            HistoryNode::decode(&dup.encode()).unwrap_err().subcode(),
            Some(subcode::DUPLICATE)
        );
        let mut wrong = dir;
        wrong.content = ObjectId::hash_blob(b"x");
        assert_eq!(
            HistoryNode::decode(&wrong.encode()).unwrap_err().subcode(),
            Some(subcode::INVALID_KIND)
        );
        assert_eq!(
            NodeKind::from_u8(3).unwrap_err().subcode(),
            Some(subcode::INVALID_KIND)
        );
    }
}
