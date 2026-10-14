//! fsck (SPEC 4.5 oracle): passes on valid state and detects every `Violation` kind on
//! hand-corrupted state. Needs the scion-object codec (P01); skips with `SKIP:` otherwise.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

mod store_support;

use bytes::Bytes;
use scion_core::{ChangeId, Kind, Mode, ObjectId};
use scion_object::{
    Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, Name, RefName, RepoPath, Tree, TreeEntry,
};
use scion_store::{
    Area, AuditRow, ChangesetBundle, ChangesetRow, DerivedKey, DerivedRow, FsckOptions, MemHandles,
    ObjectPathRow, ObjectRepo, RedactTxn, RefCas, RefExpect, RefRow, Violation, fsck,
};
use std::collections::BTreeMap;
use store_support::{repo, skip_without_codec, t, test_meta};

struct Fx {
    repo: ObjectRepo,
    h: MemHandles,
    rows: BTreeMap<&'static str, ChangesetRow>,
    css: BTreeMap<&'static str, Changeset>,
    blobs: BTreeMap<&'static str, ObjectId>,
    trees: BTreeMap<&'static str, ObjectId>,
}

fn entry(name: &str, mode: Mode, id: ObjectId, size: u64) -> TreeEntry {
    TreeEntry {
        name: Name::new(name.as_bytes()).unwrap(),
        mode,
        id,
        size,
    }
}

fn put(path: &str, id: ObjectId, size: u64) -> DeltaEntry {
    DeltaEntry {
        path: RepoPath::parse(path.as_bytes()).unwrap(),
        op: DeltaOp::Put {
            mode: Mode::File,
            id,
            size,
        },
    }
}

fn del(path: &str) -> DeltaEntry {
    DeltaEntry {
        path: RepoPath::parse(path.as_bytes()).unwrap(),
        op: DeltaOp::Delete,
    }
}

fn history_key(id: &ObjectId) -> DerivedKey {
    DerivedKey {
        ty: "history".to_owned(),
        version: 1,
        key: Bytes::copy_from_slice(id.as_bytes()),
    }
}

fn opts(verify_bytes: bool) -> FsckOptions {
    FsckOptions {
        required_eager: vec![
            ("commit_graph".to_owned(), 1),
            ("path_index".to_owned(), 1),
            ("history".to_owned(), 1),
            ("git_mapping".to_owned(), 1),
        ],
        verify_bytes,
    }
}

/// Trees of `b` that differ from `a` at their directory path, with that path: the "new trees"
/// that SPEC 9.2 `path_index` indexes (the root tree sits at the empty path).
async fn new_trees(repo: &ObjectRepo, a: ObjectId, b: ObjectId) -> Vec<(RepoPath, ObjectId)> {
    let mut out = Vec::new();
    let mut stack = vec![(RepoPath::root(), Some(a), b)];
    while let Some((path, ta, tb)) = stack.pop() {
        if ta == Some(tb) {
            continue;
        }
        out.push((path.clone(), tb));
        let tree_b = repo.get_tree(&tb).await.unwrap();
        let tree_a = match ta {
            Some(x) => Some(repo.get_tree(&x).await.unwrap()),
            None => None,
        };
        for e in tree_b.entries.iter().filter(|e| e.mode == Mode::Tree) {
            let old = tree_a
                .as_ref()
                .and_then(|t| t.get(e.name.as_bytes()))
                .filter(|o| o.mode == Mode::Tree)
                .map(|o| o.id);
            stack.push((path.join(&e.name).unwrap(), old, e.id));
        }
    }
    out
}

impl Fx {
    async fn tree(&mut self, name: &'static str, entries: Vec<TreeEntry>) -> ObjectId {
        let enc = Tree {
            entries,
            extensions: Extensions::new(),
        }
        .encode();
        let (id, _) = self
            .repo
            .put_encoded(Kind::Tree, Bytes::from(enc))
            .await
            .unwrap();
        self.trees.insert(name, id);
        id
    }

    async fn blob(&mut self, name: &'static str, content: &[u8]) -> (ObjectId, u64) {
        let (id, _) = self
            .repo
            .put_blob(Bytes::copy_from_slice(content))
            .await
            .unwrap();
        self.blobs.insert(name, id);
        (id, content.len() as u64)
    }

    fn skip_for(&self, first_parent: Option<&ObjectId>) -> Vec<ObjectId> {
        let by_id: BTreeMap<ObjectId, &ChangesetRow> =
            self.rows.values().map(|r| (r.id, r)).collect();
        let mut skip = Vec::new();
        if let Some(p) = first_parent {
            skip.push(*p);
            loop {
                let k = skip.len();
                match by_id[skip.last().unwrap()].skip.get(k - 1) {
                    Some(n) => skip.push(*n),
                    None => break,
                }
            }
        }
        skip
    }

    /// Commit a changeset with path_index rows for its Puts and a history derived row.
    async fn commit(
        &mut self,
        name: &'static str,
        parents: Vec<&'static str>,
        root_tree: ObjectId,
        delta: Vec<DeltaEntry>,
    ) -> ObjectId {
        let meta = test_meta(name).encode();
        let (meta_id, _) = self
            .repo
            .put_encoded(Kind::Meta, Bytes::from(meta))
            .await
            .unwrap();
        let parent_ids: Vec<ObjectId> = parents.iter().map(|p| self.rows[p].id).collect();
        let cs = Changeset {
            parents: parent_ids.clone(),
            root_tree,
            meta: meta_id,
            change_id: ChangeId(
                *blake3::hash(name.as_bytes())
                    .as_bytes()
                    .first_chunk()
                    .unwrap(),
            ),
            delta,
            extensions: Extensions::new(),
        };
        let encoded = cs.encode();
        let id = ObjectId::hash_encoded(Kind::Changeset, &encoded);
        let generation = parents
            .iter()
            .map(|p| self.rows[p].generation)
            .max()
            .map_or(1, |g| g + 1);
        let row = ChangesetRow {
            id,
            encoded: Bytes::from(encoded),
            generation,
            skip: self.skip_for(parent_ids.first()),
            change_id: cs.change_id,
            created_by: "service:test".to_owned(),
            created_at: t(1),
        };
        let mut object_paths: Vec<ObjectPathRow> = cs
            .delta
            .iter()
            .filter_map(|d| match d.op {
                DeltaOp::Put { id: blob, .. } => Some(ObjectPathRow {
                    object: blob,
                    path: d.path.clone(),
                    first_changeset: id,
                    at: t(1),
                }),
                DeltaOp::Delete => None,
            })
            .collect();
        let t1 = parents
            .first()
            .map_or(scion_core::empty_tree_id(), |p| self.css[p].root_tree);
        for (path, tree) in new_trees(&self.repo, t1, root_tree).await {
            object_paths.push(ObjectPathRow {
                object: tree,
                path,
                first_changeset: id,
                at: t(1),
            });
        }
        self.repo
            .storage()
            .meta
            .commit_changeset(ChangesetBundle {
                row: row.clone(),
                parents: parent_ids,
                object_paths,
                derived: vec![DerivedRow {
                    key: history_key(&id),
                    value: Some(Bytes::from_static(b"node")),
                    storage_key: None,
                    source_object: None,
                    at: t(1),
                }],
            })
            .await
            .unwrap();
        self.rows.insert(name, row);
        self.css.insert(name, cs);
        id
    }

    async fn build() -> Fx {
        let (repo, h) = repo();
        let mut fx = Fx {
            repo,
            h,
            rows: BTreeMap::new(),
            css: BTreeMap::new(),
            blobs: BTreeMap::new(),
            trees: BTreeMap::new(),
        };
        let empty = fx.tree("empty", vec![]).await;
        assert_eq!(empty, scion_core::empty_tree_id());
        fx.commit("root", vec![], empty, vec![]).await;

        let (hello, hs) = fx.blob("hello", b"hello").await;
        let (world, ws) = fx.blob("world", &[b'w'; 70_000]).await;
        let dir = fx
            .tree("dir", vec![entry("b.txt", Mode::File, world, ws)])
            .await;
        let root1 = fx
            .tree(
                "root1",
                vec![
                    entry("a.txt", Mode::File, hello, hs),
                    entry("dir", Mode::Tree, dir, 0),
                ],
            )
            .await;
        fx.commit(
            "c1",
            vec!["root"],
            root1,
            vec![put("a.txt", hello, hs), put("dir/b.txt", world, ws)],
        )
        .await;

        let (hello2, h2s) = fx.blob("hello2", b"hello, again").await;
        let root2 = fx
            .tree("root2", vec![entry("a.txt", Mode::File, hello2, h2s)])
            .await;
        fx.commit(
            "c2",
            vec!["c1"],
            root2,
            vec![put("a.txt", hello2, h2s), del("dir/b.txt")],
        )
        .await;

        // Path order differs from name order: "a-b" < "a.txt" < "a/c" bytewise.
        let (ab, abs) = fx.blob("ab", b"dash").await;
        let (ac, acs) = fx.blob("ac", b"slash").await;
        let adir = fx
            .tree("adir", vec![entry("c", Mode::Executable, ac, acs)])
            .await;
        let root3 = fx
            .tree(
                "root3",
                vec![
                    entry("a", Mode::Tree, adir, 0),
                    entry("a-b", Mode::File, ab, abs),
                    entry("a.txt", Mode::File, hello2, h2s),
                ],
            )
            .await;
        let mut ac_put = put("a/c", ac, acs);
        ac_put.op = DeltaOp::Put {
            mode: Mode::Executable,
            id: ac,
            size: acs,
        };
        fx.commit("c3", vec!["c2"], root3, vec![put("a-b", ab, abs), ac_put])
            .await;
        // A merge of c3 and c1 keeping c3's tree: delta is relative to the first parent only.
        fx.commit("m", vec!["c3", "c1"], root3, vec![]).await;

        let m = fx.rows["m"].id;
        fx.repo
            .storage()
            .meta
            .cas_ref(RefCas {
                name: RefName::main(),
                expect: RefExpect::Absent,
                new_target: Some(m),
                principal: "service:test".to_owned(),
                at: t(2),
                landed: vec![],
                audit: None,
            })
            .await
            .unwrap();
        fx
    }

    async fn check(&self, verify: bool) -> Vec<Violation> {
        fsck(&self.repo, &opts(verify)).await.unwrap()
    }

    fn id(&self, cs: &str) -> ObjectId {
        self.rows[cs].id
    }
}

#[tokio::test]
async fn valid_state_has_no_violations() {
    if skip_without_codec("valid_state_has_no_violations") {
        return;
    }
    let fx = Fx::build().await;
    assert_eq!(fx.check(true).await, vec![]);
    assert_eq!(fx.check(false).await, vec![]);
    assert_eq!(fx.rows["m"].generation, 5);
    assert_eq!(
        fx.rows["m"].skip,
        vec![fx.id("c3"), fx.id("c2"), fx.id("root")]
    );
}

#[tokio::test]
async fn missing_objects() {
    if skip_without_codec("missing_objects") {
        return;
    }
    let fx = Fx::build().await;
    fx.h.meta.remove_object_row_raw(&fx.trees["root1"]);
    let v = fx.check(false).await;
    assert!(
        v.contains(&Violation::MissingObject {
            changeset: fx.id("c1"),
            object: fx.trees["root1"],
            role: "root_tree",
        }),
        "{v:?}"
    );

    let fx = Fx::build().await;
    let meta = fx.css["c2"].meta;
    fx.h.meta.remove_object_row_raw(&meta);
    assert_eq!(
        fx.check(false).await,
        vec![Violation::MissingObject {
            changeset: fx.id("c2"),
            object: meta,
            role: "meta",
        }]
    );

    let fx = Fx::build().await;
    fx.h.meta.remove_object_row_raw(&fx.blobs["ab"]);
    let v = fx.check(false).await;
    assert_eq!(
        v,
        vec![Violation::MissingObject {
            changeset: fx.id("c3"),
            object: fx.blobs["ab"],
            role: "delta_blob",
        }]
    );
    let v = fx.check(true).await;
    assert!(
        v.contains(&Violation::MissingObject {
            changeset: fx.id("c3"),
            object: fx.blobs["ab"],
            role: "tree_entry",
        }),
        "verify_bytes walks every tree entry: {v:?}"
    );

    // A subtree missing under an existing root tree.
    let fx = Fx::build().await;
    fx.h.meta.remove_object_row_raw(&fx.trees["adir"]);
    let v = fx.check(false).await;
    assert!(
        v.contains(&Violation::MissingObject {
            changeset: fx.id("c3"),
            object: fx.trees["adir"],
            role: "tree",
        }),
        "{v:?}"
    );

    // A changeset whose parent row is missing.
    let fx = Fx::build().await;
    let ghost = ObjectId::hash_encoded(Kind::Changeset, b"ghost");
    let cs = Changeset {
        parents: vec![ghost],
        root_tree: fx.trees["root2"],
        meta: fx.css["c2"].meta,
        change_id: ChangeId([9; 16]),
        delta: vec![],
        extensions: Extensions::new(),
    };
    let enc = cs.encode();
    let mut row = fx.rows["c2"].clone();
    row.id = ObjectId::hash_encoded(Kind::Changeset, &enc);
    row.encoded = Bytes::from(enc);
    fx.h.meta.put_changeset_row_raw(row.clone());
    let v = fx.check(false).await;
    assert!(
        v.contains(&Violation::MissingObject {
            changeset: row.id,
            object: ghost,
            role: "parent",
        }),
        "{v:?}"
    );
}

#[tokio::test]
async fn digest_mismatches() {
    if skip_without_codec("digest_mismatches") {
        return;
    }
    let fx = Fx::build().await;
    let world = fx.blobs["world"];
    fx.h.blobs.corrupt(&fx.repo.key(&world, Area::Live));
    assert_eq!(
        fx.check(false).await,
        vec![],
        "bytes are only read with verify_bytes"
    );
    assert_eq!(
        fx.check(true).await,
        vec![Violation::DigestMismatch { id: world }]
    );

    let fx = Fx::build().await;
    let mut row = fx.rows["c1"].clone();
    row.encoded = fx.rows["c2"].encoded.clone();
    fx.h.meta.put_changeset_row_raw(row);
    let v = fx.check(false).await;
    assert!(
        v.contains(&Violation::DigestMismatch { id: fx.id("c1") }),
        "{v:?}"
    );

    // A corrupt tree (object storage) is caught by the delta walk even without verify_bytes.
    let fx = Fx::build().await;
    let dir = fx.trees["dir"];
    let mut row =
        fx.h.meta
            .all_objects()
            .into_iter()
            .find(|r| r.id == dir)
            .unwrap();
    row.inline_data = Some(Bytes::from_static(b"\x01\x00\x01"));
    fx.h.meta.put_object_row_raw(row);
    let v = fx.check(false).await;
    assert!(v.contains(&Violation::DigestMismatch { id: dir }), "{v:?}");
}

#[tokio::test]
async fn bad_encoding() {
    if skip_without_codec("bad_encoding") {
        return;
    }
    let fx = Fx::build().await;
    let garbage = b"\x01\x05not a changeset";
    let mut row = fx.rows["c2"].clone();
    row.id = ObjectId::hash_encoded(Kind::Changeset, garbage);
    row.encoded = Bytes::from_static(garbage);
    fx.h.meta.put_changeset_row_raw(row.clone());
    assert_eq!(
        fx.check(false).await,
        vec![Violation::BadEncoding { id: row.id }]
    );
}

#[tokio::test]
async fn delta_mismatch() {
    if skip_without_codec("delta_mismatch") {
        return;
    }
    let mut fx = Fx::build().await;
    // Claims to add "x" but keeps the parent's tree.
    let (x, xs) = fx.blob("x", b"x").await;
    let root3 = fx.trees["root3"];
    fx.commit("bad", vec!["c3"], root3, vec![put("x", x, xs)])
        .await;
    assert_eq!(
        fx.check(false).await,
        vec![Violation::DeltaMismatch {
            changeset: fx.id("bad")
        }]
    );
    // Omitting an entry is also a mismatch.
    let mut fx = Fx::build().await;
    let (hello, hs) = (fx.blobs["hello"], 5);
    let root1 = fx.trees["root1"];
    fx.commit("bad2", vec!["root"], root1, vec![put("a.txt", hello, hs)])
        .await;
    assert_eq!(
        fx.check(false).await,
        vec![Violation::DeltaMismatch {
            changeset: fx.id("bad2")
        }]
    );
}

#[tokio::test]
async fn missing_eager() {
    if skip_without_codec("missing_eager") {
        return;
    }
    let fx = Fx::build().await;
    fx.h.meta.remove_derived_raw(&history_key(&fx.id("c2")));
    assert_eq!(
        fx.check(false).await,
        vec![Violation::MissingEager {
            changeset: fx.id("c2"),
            ty: "history".to_owned()
        }]
    );
    let none = FsckOptions {
        required_eager: vec![],
        verify_bytes: false,
    };
    assert_eq!(fsck(&fx.repo, &none).await.unwrap(), vec![]);

    // path_index: a new tree (here `dir` of c1, and the root tree of c3) without its row.
    let fx = Fx::build().await;
    let dir_path = RepoPath::parse(b"dir").unwrap();
    assert!(
        fx.h.meta
            .remove_object_path_raw(&fx.trees["dir"], &dir_path)
    );
    assert_eq!(
        fx.check(false).await,
        vec![Violation::MissingEager {
            changeset: fx.id("c1"),
            ty: "path_index".to_owned()
        }]
    );
    let fx = Fx::build().await;
    assert!(
        fx.h.meta
            .remove_object_path_raw(&fx.trees["root3"], &RepoPath::root())
    );
    assert_eq!(
        fx.check(false).await,
        vec![Violation::MissingEager {
            changeset: fx.id("c3"),
            ty: "path_index".to_owned()
        }]
    );

    // path_index: a Put whose (blob, path) has no object_paths row.
    let mut fx = Fx::build().await;
    let (nb2, ns2) = fx.blob("new2", b"never indexed").await;
    let root_b = fx
        .tree(
            "rootb",
            vec![
                entry("a.txt", Mode::File, fx.blobs["hello2"], 12),
                entry("n", Mode::File, nb2, ns2),
            ],
        )
        .await;
    let cs2 = Changeset {
        parents: vec![fx.id("c2")],
        root_tree: root_b,
        meta: fx.css["c2"].meta,
        change_id: ChangeId([44; 16]),
        delta: vec![put("n", nb2, ns2)],
        extensions: Extensions::new(),
    };
    let enc2 = cs2.encode();
    let id2 = ObjectId::hash_encoded(Kind::Changeset, &enc2);
    let row2 = ChangesetRow {
        id: id2,
        encoded: Bytes::from(enc2),
        generation: 4,
        skip: vec![fx.id("c2"), fx.id("c1")],
        change_id: cs2.change_id,
        created_by: "service:test".to_owned(),
        created_at: t(1),
    };
    fx.repo
        .storage()
        .meta
        .commit_changeset(ChangesetBundle {
            row: row2,
            parents: vec![fx.id("c2")],
            object_paths: vec![],
            derived: vec![DerivedRow {
                key: history_key(&id2),
                value: Some(Bytes::from_static(b"n")),
                storage_key: None,
                source_object: None,
                at: t(1),
            }],
        })
        .await
        .unwrap();
    assert_eq!(
        fx.check(false).await,
        vec![Violation::MissingEager {
            changeset: id2,
            ty: "path_index".to_owned()
        }]
    );
}

#[tokio::test]
async fn bad_graph() {
    if skip_without_codec("bad_graph") {
        return;
    }
    let fx = Fx::build().await;
    let mut row = fx.rows["c2"].clone();
    row.generation = 7;
    fx.h.meta.put_changeset_row_raw(row);
    // The child's (correct) generation no longer matches its parent's either.
    let mut v = fx.check(false).await;
    v.sort_by_key(|x| format!("{x:?}"));
    let mut want = vec![
        Violation::BadGraph {
            changeset: fx.id("c2"),
        },
        Violation::BadGraph {
            changeset: fx.id("c3"),
        },
    ];
    want.sort_by_key(|x| format!("{x:?}"));
    assert_eq!(v, want);
    let fx = Fx::build().await;
    let mut row = fx.rows["c3"].clone();
    row.skip.pop();
    fx.h.meta.put_changeset_row_raw(row);
    assert_eq!(
        fx.check(false).await,
        vec![Violation::BadGraph {
            changeset: fx.id("c3")
        }]
    );
}

#[tokio::test]
async fn ref_target_missing() {
    if skip_without_codec("ref_target_missing") {
        return;
    }
    let fx = Fx::build().await;
    fx.h.meta.put_ref_row_raw(RefRow {
        name: RefName::parse("release/1").unwrap(),
        target: ObjectId::hash_encoded(Kind::Changeset, b"nowhere"),
        version: 1,
        updated_at: t(3),
    });
    assert_eq!(
        fx.check(false).await,
        vec![Violation::RefTargetMissing {
            name: "release/1".to_owned()
        }]
    );
}

#[tokio::test]
async fn quarantined_and_purged_bytes_are_not_violations() {
    if skip_without_codec("quarantined_and_purged_bytes_are_not_violations") {
        return;
    }
    let fx = Fx::build().await;
    let world = fx.blobs["world"];
    let meta = fx.css["c1"].meta;
    let audit = AuditRow {
        at: t(5),
        actor: "github:admin".to_owned(),
        action: "purge".to_owned(),
        target: world.to_text(),
        reason: "r".to_owned(),
        detail: String::new(),
    };
    fx.repo
        .storage()
        .meta
        .redact(RedactTxn {
            ids: vec![world, meta],
            reason: "leak".to_owned(),
            actor: "github:admin".to_owned(),
            at: t(4),
            purge_after: t(5),
        })
        .await
        .unwrap();
    fx.repo.quarantine_bytes(&world).await.unwrap();
    assert_eq!(fx.check(true).await, vec![]);
    fx.repo.purge_bytes(&world).await.unwrap();
    fx.repo
        .storage()
        .meta
        .mark_purged(&[world, meta], audit)
        .await
        .unwrap();
    assert_eq!(fx.check(true).await, vec![]);
}

#[tokio::test]
async fn unreachable_live_objects_are_verified() {
    // SPEC 4.5 invariant 1 covers every stored object, not only those a changeset reaches.
    let (repo, h) = store_support::repo();
    let verify = FsckOptions {
        required_eager: vec![],
        verify_bytes: true,
    };
    let (good, _) = repo
        .put_blob(Bytes::from_static(b"unreferenced"))
        .await
        .unwrap();
    let (bad, _) = repo
        .put_blob(Bytes::from_static(b"unreferenced, then corrupted"))
        .await
        .unwrap();
    // Orphan bytes (no row) are harmless.
    let orphan = ObjectId::hash_blob(b"orphan");
    h.blobs.insert_raw(
        &repo.key(&orphan, Area::Live),
        Bytes::from_static(b"not even zstd"),
    );
    // Foreign keys are ignored.
    h.blobs
        .insert_raw("scion/elsewhere", Bytes::from_static(b"x"));
    assert_eq!(fsck(&repo, &verify).await.unwrap(), vec![]);

    h.blobs.corrupt(&repo.key(&bad, Area::Live));
    assert_eq!(
        fsck(&repo, &verify).await.unwrap(),
        vec![Violation::DigestMismatch { id: bad }]
    );
    // Without verify_bytes nothing is read.
    let quick = FsckOptions {
        required_eager: vec![],
        verify_bytes: false,
    };
    assert_eq!(fsck(&repo, &quick).await.unwrap(), vec![]);

    // A redacted object whose bytes have not moved yet is not a violation.
    repo.storage()
        .meta
        .redact(RedactTxn {
            ids: vec![bad],
            reason: "r".to_owned(),
            actor: "github:admin".to_owned(),
            at: t(1),
            purge_after: t(2),
        })
        .await
        .unwrap();
    assert_eq!(fsck(&repo, &verify).await.unwrap(), vec![]);
    assert_eq!(
        repo.get(&good).await.unwrap(),
        Bytes::from_static(b"unreferenced")
    );
}
