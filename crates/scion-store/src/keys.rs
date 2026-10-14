//! SPEC 5.1 object storage key layout.

use crate::types::{Area, RepoId};
use scion_core::{Kind, ObjectId};

/// History node kind (SPEC 9.2), which uses the bare `x` letter in keys.
const HISTORY_NODE_KIND: u8 = 0x10;

/// `<prefix>/{r|q}/<repo_id>/<kind_letter>/<hex[0:2]>/<hex>` (SPEC 5.1). Derived kinds use `x`
/// (kind 0x10) or `x<2-hex kind>` (`docs/contracts/storage-semantics.md` section 7).
pub fn object_key(prefix: &str, repo: RepoId, id: &ObjectId, area: Area) -> String {
    let area = match area {
        Area::Live => 'r',
        Area::Quarantine => 'q',
    };
    let kind = match id.kind() {
        Kind::Derived(k) if k != HISTORY_NODE_KIND => format!("x{k:02x}"),
        k => k.letter().to_string(),
    };
    let hex = id.hex();
    let fan = hex.get(..2).unwrap_or_default();
    format!("{prefix}/{area}/{}/{kind}/{fan}/{hex}", repo.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let id = ObjectId::new(Kind::Blob, [0xab; 32]);
        let hex = "ab".repeat(32);
        assert_eq!(
            object_key("scion", RepoId(42), &id, Area::Live),
            format!("scion/r/42/b/ab/{hex}")
        );
        assert_eq!(
            object_key("p/x", RepoId(7), &id, Area::Quarantine),
            format!("p/x/q/7/b/ab/{hex}")
        );
        let t = ObjectId::new(Kind::Tree, [0x01; 32]);
        assert!(object_key("s", RepoId(1), &t, Area::Live).starts_with("s/r/1/t/01/"));
        let m = ObjectId::new(Kind::Meta, [0x01; 32]);
        assert!(object_key("s", RepoId(1), &m, Area::Live).starts_with("s/r/1/m/01/"));
        let c = ObjectId::new(Kind::Changeset, [0x01; 32]);
        assert!(object_key("s", RepoId(1), &c, Area::Live).starts_with("s/r/1/c/01/"));
        let h = ObjectId::new(Kind::Derived(0x10), [0x01; 32]);
        assert!(object_key("s", RepoId(1), &h, Area::Live).starts_with("s/r/1/x/01/"));
        let d = ObjectId::new(Kind::Derived(0x2a), [0x01; 32]);
        assert!(object_key("s", RepoId(1), &d, Area::Live).starts_with("s/r/1/x2a/01/"));
    }
}
