//! SPEC 11.8 revision resolution for the Resolve RPC (P27).
//!
//! [`resolve_revision`] parses the revision with [`scion_object::parse_revision`] and resolves
//! the base (PLAN 2.14, implementation-defined 6.6):
//!
//! - a full id: must be an existing changeset;
//! - `c1<hex>` or a bare hex token of at least 8 digits: a changeset prefix search (zero matches
//!   is NOT_FOUND, more than one is INVALID_ARGUMENT/ambiguous_prefix with a Candidates
//!   payload, SPEC 4.2, 19.1); for a bare token an existing ref with that exact name wins;
//! - `<ref>` and `<ref>@<version>`: the ref (or its ref-log entry);
//! - `git:<sha>`: the `git_mapping` derived type (SPEC 14).
//!
//! The `~n` / `^k` suffixes are then applied left to right through the commit graph.

use scion_core::{ErrorCode, ErrorPayload, Kind, ObjectId, Result, ScionError, subcode};
use scion_derived::{DerivedEngine, git_mapping};
use scion_history::History;
use scion_object::{RefName, RevBase, RevSuffix, parse_revision};

/// The most candidates an ambiguous prefix reports (SPEC 19.1).
pub const MAX_PREFIX_CANDIDATES: usize = 16;

/// SPEC 11.8: the changeset a revision string denotes.
///
/// Errors: INVALID_ARGUMENT/invalid_revision for a malformed revision or a full id that is not
/// a changeset id; INVALID_ARGUMENT/ambiguous_prefix with [`ErrorPayload::Candidates`] for a
/// prefix matching several changesets; NOT_FOUND when the base (changeset, prefix, ref, ref
/// version or git commit) does not exist or a suffix walks past the available parents.
pub async fn resolve_revision(
    history: &History,
    derived: &DerivedEngine,
    revision: &str,
) -> Result<ObjectId> {
    let rev = parse_revision(revision)?;
    let mut cur = resolve_base(history, derived, &rev.base).await?;
    for s in &rev.suffixes {
        let next = match s {
            RevSuffix::Ancestor(n) => history.nth_first_parent(&cur, *n).await?,
            RevSuffix::Parent(k) => history.nth_parent(&cur, *k).await?,
        };
        cur = next.ok_or_else(|| {
            let what = match s {
                RevSuffix::Ancestor(n) => format!("~{n}"),
                RevSuffix::Parent(k) => format!("^{k}"),
            };
            ScionError::not_found(format!("{cur} has no ancestor {what}"))
        })?;
    }
    Ok(cur)
}

async fn resolve_base(
    history: &History,
    derived: &DerivedEngine,
    base: &RevBase,
) -> Result<ObjectId> {
    match base {
        RevBase::Id(id) => {
            if id.kind() != Kind::Changeset {
                return Err(ScionError::invalid(
                    subcode::INVALID_REVISION,
                    format!("{id} is not a changeset id"),
                ));
            }
            history.changeset(id).await.map(|r| r.id)
        }
        RevBase::IdPrefix(_) => match base.hex_prefix() {
            Some(hex) => by_prefix(history, hex).await,
            None => Err(ScionError::invalid(
                subcode::INVALID_REVISION,
                "malformed changeset prefix",
            )),
        },
        RevBase::Name(name) => {
            // An existing ref with exactly this name wins (implementation-defined 6.6).
            if let Ok(r) = RefName::parse(name) {
                match history.resolve_ref(&r, None).await {
                    Ok(row) => return Ok(row.target),
                    Err(e) if e.code == ErrorCode::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            match base.hex_prefix() {
                Some(hex) => by_prefix(history, hex).await,
                None => Err(ScionError::not_found(format!("no ref named {name:?}"))),
            }
        }
        RevBase::RefAt(name, version) => history
            .resolve_ref(name, Some(*version))
            .await
            .map(|r| r.target),
        RevBase::Git(sha) => git_mapping::changeset_for_git(derived, sha)
            .await?
            .ok_or_else(|| ScionError::not_found(format!("no changeset imported from git:{sha}"))),
    }
}

/// SPEC 4.2 unique-prefix lookup.
async fn by_prefix(history: &History, hex: &str) -> Result<ObjectId> {
    let found = history
        .meta()
        .changesets_by_prefix(hex, MAX_PREFIX_CANDIDATES)
        .await?;
    match found.as_slice() {
        [] => Err(ScionError::not_found(format!(
            "no changeset id starts with {hex}"
        ))),
        [one] => Ok(*one),
        many => Err(ScionError::invalid(
            subcode::AMBIGUOUS_PREFIX,
            format!(
                "changeset prefix {hex} is ambiguous ({} or more candidates)",
                many.len()
            ),
        )
        .with_payload(ErrorPayload::Candidates(many.to_vec()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::{ChangeId, Principal, SystemClock};
    use scion_derived::{DeriveConfig, Registry};
    use scion_history::{HistoryConfig, LandConfig, MetaFields, MetaSource, NoEager};
    use scion_object::{DeltaEntry, DeltaOp, RepoPath, Timestamp};
    use scion_store::{MemOptions, ObjectRepo, ObjectRepoConfig, Storage};
    use std::sync::Arc;
    use std::time::Duration;

    async fn setup() -> (History, Arc<DerivedEngine>) {
        let (storage, _) = Storage::in_memory(MemOptions::default());
        let objects = ObjectRepo::new(
            storage,
            ObjectRepoConfig {
                prefix: "t".into(),
                inline_threshold_bytes: 65536,
                zstd_level: 3,
                max_blob_bytes: 1 << 20,
                decoded_cache_entries: 64,
            },
        );
        let clock = Arc::new(SystemClock);
        let cfg = HistoryConfig {
            limits: Default::default(),
            land: LandConfig {
                max_attempts: 4,
                backoff_base: Duration::from_millis(1),
                backoff_max: Duration::from_millis(2),
                rng_seed: Some(1),
            },
            bootstrap_principal: Principal::Service("scion".into()),
        };
        let h = History::new(objects.clone(), cfg, Arc::new(NoEager), clock.clone());
        let d = DerivedEngine::new(
            objects,
            Registry::empty(),
            DeriveConfig {
                lease_ttl: Duration::from_secs(1),
                lazy_workers: 1,
                holder_id: "t".into(),
            },
            clock,
        );
        h.bootstrap().await.unwrap();
        (h, d)
    }

    async fn child(h: &History, parent: ObjectId, name: &str) -> ObjectId {
        let (blob, _) = h
            .objects()
            .put_blob(bytes::Bytes::from(name.as_bytes().to_vec()))
            .await
            .unwrap();
        let meta = MetaFields {
            author: b"t <t@t>".to_vec(),
            author_time: Timestamp {
                seconds: 1,
                tz_offset_minutes: 0,
            },
            committer: None,
            committer_time: None,
            message: name.as_bytes().to_vec(),
            extensions: Default::default(),
        };
        h.create_internal(
            vec![parent],
            vec![DeltaEntry {
                path: RepoPath::parse(name.as_bytes()).unwrap(),
                op: DeltaOp::Put {
                    mode: scion_core::Mode::File,
                    id: blob,
                    size: name.len() as u64,
                },
            }],
            MetaSource::New(meta),
            ChangeId::random(),
            &Principal::Service("t".into()),
        )
        .await
        .unwrap()
        .changeset_id
    }

    #[tokio::test]
    async fn bases_and_suffixes() {
        let (h, d) = setup().await;
        let root = h.resolve_ref(&RefName::main(), None).await.unwrap().target;
        let c1 = child(&h, root, "a").await;
        let c2 = child(&h, c1, "b").await;
        assert_eq!(resolve_revision(&h, &d, "main").await.unwrap(), root);
        assert_eq!(resolve_revision(&h, &d, "main@1").await.unwrap(), root);
        assert_eq!(resolve_revision(&h, &d, &c2.to_text()).await.unwrap(), c2);
        let full = c2.to_text();
        assert_eq!(resolve_revision(&h, &d, &full[..12]).await.unwrap(), c2);
        assert_eq!(resolve_revision(&h, &d, &c2.hex()[..10]).await.unwrap(), c2);
        assert_eq!(
            resolve_revision(&h, &d, &format!("{full}~2"))
                .await
                .unwrap(),
            root
        );
        assert_eq!(
            resolve_revision(&h, &d, &format!("{full}^~"))
                .await
                .unwrap(),
            root
        );
        let e = resolve_revision(&h, &d, &format!("{full}~3"))
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        let e = resolve_revision(&h, &d, "nope").await.unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        let e = resolve_revision(&h, &d, "main@7").await.unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        let e = resolve_revision(&h, &d, &format!("git:{}", "a".repeat(40)))
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        let e = resolve_revision(&h, &d, "bad..ref").await.unwrap_err();
        assert_eq!(e.subcode(), Some(subcode::INVALID_REVISION));
        let tree = h.changeset(&c2).await.unwrap().changeset.root_tree;
        let e = resolve_revision(&h, &d, &tree.to_text()).await.unwrap_err();
        assert_eq!(e.subcode(), Some(subcode::INVALID_REVISION));
        // A missing full id.
        let missing = ObjectId::new(Kind::Changeset, [7u8; 32]);
        let e = resolve_revision(&h, &d, &missing.to_text())
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn exact_ref_name_beats_hex_prefix() {
        let (h, d) = setup().await;
        let root = h.resolve_ref(&RefName::main(), None).await.unwrap().target;
        let c1 = child(&h, root, "a").await;
        let token = c1.hex()[..8].to_owned();
        assert_eq!(resolve_revision(&h, &d, &token).await.unwrap(), c1);
        let name = RefName::parse(&token).unwrap();
        h.create_ref(&name, &root, &Principal::Service("t".into()), "test")
            .await
            .unwrap();
        assert_eq!(resolve_revision(&h, &d, &token).await.unwrap(), root);
        // The c1-prefix form is never a ref name.
        assert_eq!(
            resolve_revision(&h, &d, &format!("c1{token}"))
                .await
                .unwrap(),
            c1
        );
    }
}
