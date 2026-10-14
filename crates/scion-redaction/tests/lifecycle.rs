//! SPEC 19.13 redaction lifecycle at library level (P22): `Storage::in_memory` + `ManualClock`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

use bytes::Bytes;
use scion_core::{
    ChangeId, ErrorCode, FaultAction, ManualClock, Mode, ObjectId, Principal, ScionError, points,
    subcode,
};
use scion_derived::{DeriveConfig, DeriveMode, DerivedEngine, Registry};
use scion_history::{
    AllowAll, CreateRequest, Created, History, HistoryConfig, MetaFields, NoEager,
};
use scion_object::{DeltaEntry, DeltaOp, Extensions, RepoPath, Timestamp};
use scion_redaction::{RedactOutcome, RedactionConfig, Redactor};
use scion_store::{
    Area, Consistency, DerivedKey, DerivedRow, MemHandles, MemOptions, ObjectRepo,
    ObjectRepoConfig, ObjectState, RedactionState, Storage,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const CONTENT_TY: &str = "test_content";
const PLAIN_TY: &str = "test_plain";
const HOUR: Duration = Duration::from_secs(3600);

struct Env {
    h: History,
    objects: ObjectRepo,
    mem: MemHandles,
    clock: Arc<ManualClock>,
    root: ObjectId,
    derived: Arc<DerivedEngine>,
    red: Arc<Redactor>,
}

async fn env_with(cfg: RedactionConfig) -> Env {
    let (storage, mem) = Storage::in_memory(MemOptions::default());
    let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let objects = ObjectRepo::new(storage, ObjectRepoConfig::default()).with_clock(clock.clone());
    let h = History::new(
        objects.clone(),
        HistoryConfig::default(),
        Arc::new(NoEager),
        clock.clone(),
    );
    let root = h.bootstrap().await.expect("bootstrap");
    let mut registry = Registry::empty();
    registry.register_descriptor(CONTENT_TY, 1, DeriveMode::Lazy, true);
    registry.register_descriptor(PLAIN_TY, 1, DeriveMode::Lazy, false);
    let derived = DerivedEngine::new(
        objects.clone(),
        registry,
        DeriveConfig::default(),
        clock.clone(),
    );
    let red = Redactor::new(objects.clone(), derived.clone(), clock.clone(), cfg);
    Env {
        h,
        objects,
        mem,
        clock,
        root,
        derived,
        red,
    }
}

async fn env() -> Env {
    env_with(RedactionConfig::default()).await
}

fn admin() -> Principal {
    Principal::GitHub {
        login: "admin".to_owned(),
        id: Some(1),
    }
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s.as_bytes()).unwrap()
}

async fn upload(e: &Env, content: &str) -> ObjectId {
    e.objects
        .put_blob(Bytes::from(content.as_bytes().to_vec()))
        .await
        .unwrap()
        .0
}

fn put(path: &str, id: ObjectId, content: &str) -> DeltaEntry {
    DeltaEntry {
        path: p(path),
        op: DeltaOp::Put {
            mode: Mode::File,
            id,
            size: content.len() as u64,
        },
    }
}

async fn create(e: &Env, parents: Vec<ObjectId>, delta: Vec<DeltaEntry>, msg: &str) -> Created {
    let req = CreateRequest {
        parents,
        delta,
        meta: MetaFields {
            author: b"Alice <alice@example.com>".to_vec(),
            author_time: Timestamp {
                seconds: 1_700_000_000,
                tz_offset_minutes: 0,
            },
            committer: None,
            committer_time: None,
            message: msg.as_bytes().to_vec(),
            extensions: Extensions::new(),
        },
        change_id: ChangeId([7; 16]),
        expected_root_tree: None,
        import_mode: false,
    };
    e.h.create_changeset(req, &admin(), &AllowAll)
        .await
        .unwrap()
}

/// Two changesets: c1 adds secret + readme, c2 changes readme only.
async fn sample(e: &Env) -> (ObjectId, Created, Created) {
    let secret = upload(e, "password=hunter2\n").await;
    let readme = upload(e, "hello\n").await;
    let c1 = create(
        e,
        vec![e.root],
        vec![
            put("readme", readme, "hello\n"),
            put("src/secret.txt", secret, "password=hunter2\n"),
        ],
        "add files",
    )
    .await;
    let readme2 = upload(e, "hello world\n").await;
    let c2 = create(
        e,
        vec![c1.changeset_id],
        vec![put("readme", readme2, "hello world\n")],
        "edit readme",
    )
    .await;
    (secret, c1, c2)
}

fn assert_err(e: &ScionError, code: ErrorCode, sub: Option<&str>) {
    assert_eq!(e.code, code, "unexpected error {e:?}");
    if let Some(s) = sub {
        assert_eq!(e.subcode(), Some(s), "unexpected subcode in {e:?}");
    }
}

fn state(e: &Env, id: &ObjectId) -> ObjectState {
    e.mem
        .meta
        .all_objects()
        .into_iter()
        .find(|r| r.id == *id)
        .expect("row")
        .state
}

fn live_key(e: &Env, id: &ObjectId) -> String {
    e.objects.key(id, Area::Live)
}

fn q_key(e: &Env, id: &ObjectId) -> String {
    e.objects.key(id, Area::Quarantine)
}

fn has_key(e: &Env, key: &str) -> bool {
    e.mem.blobs.raw(key).is_some()
}

async fn redaction_state(e: &Env, id: &ObjectId) -> Option<RedactionState> {
    let (rows, _) = e.red.list(0).await.unwrap();
    rows.into_iter().find(|r| r.object == *id).map(|r| r.state)
}

/// Every identity-bearing id: changesets (id -> encoded), trees and refs (name -> target).
fn identity_snapshot(e: &Env) -> (BTreeMap<ObjectId, Bytes>, Vec<ObjectId>) {
    let cs = e
        .mem
        .meta
        .all_changesets()
        .into_iter()
        .map(|r| (r.id, r.encoded))
        .collect();
    let mut trees: Vec<ObjectId> = e
        .mem
        .meta
        .all_objects()
        .into_iter()
        .filter(|r| r.id.kind() == scion_core::Kind::Tree)
        .map(|r| r.id)
        .collect();
    trees.sort();
    (cs, trees)
}

#[tokio::test]
async fn redacting_a_blob_changes_no_tree_changeset_or_ref_id() {
    let e = env().await;
    let (secret, c1, c2) = sample(&e).await;
    let before = identity_snapshot(&e);
    let refs_before = e.objects.storage().meta.list_refs().await.unwrap();
    let rec_before = e.h.changeset(&c2.changeset_id).await.unwrap();

    e.red
        .redact(&[secret], "leaked credential", &admin(), None)
        .await
        .unwrap();

    assert_eq!(identity_snapshot(&e), before);
    assert_eq!(
        e.objects.storage().meta.list_refs().await.unwrap(),
        refs_before
    );
    let rec_after = e.h.changeset(&c2.changeset_id).await.unwrap();
    assert_eq!(rec_after.changeset, rec_before.changeset);
    assert_eq!(rec_after.changeset.root_tree, c2.root_tree);
    // Trees still list the redacted id and size (SPEC 13.4).
    let entry =
        e.h.lookup(&c1.root_tree, &p("src/secret.txt"))
            .await
            .unwrap()
            .expect("entry kept");
    assert_eq!(entry.id, secret);
    assert_eq!(entry.size, "password=hunter2\n".len() as u64);
    assert_err(
        &e.objects.get(&secret).await.unwrap_err(),
        ErrorCode::Redacted,
        None,
    );
    // fsck: quarantined bytes are not violations.
    let v = scion_store::fsck(
        &e.objects,
        &scion_store::FsckOptions {
            required_eager: vec![],
            verify_bytes: true,
        },
    )
    .await
    .unwrap();
    assert!(v.is_empty(), "{v:?}");
}

#[tokio::test]
async fn quarantined_then_purged_after_purge_after_elapses() {
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    let out = e
        .red
        .redact(&[secret], "leak", &admin(), Some(HOUR))
        .await
        .unwrap();
    assert_eq!(out.epoch, 1);
    assert_eq!(state(&e, &secret), ObjectState::Quarantined);
    assert_eq!(
        redaction_state(&e, &secret).await,
        Some(RedactionState::Quarantined)
    );
    // Bytes moved to the quarantine key.
    assert!(!has_key(&e, &live_key(&e, &secret)));
    assert!(has_key(&e, &q_key(&e, &secret)));

    assert_eq!(e.red.run_due_purges().await.unwrap(), 0);
    e.clock.advance(HOUR - Duration::from_secs(1));
    assert_eq!(e.red.run_due_purges().await.unwrap(), 0);
    e.clock.advance(Duration::from_secs(1));
    assert_eq!(e.red.run_due_purges().await.unwrap(), 1);
    assert_eq!(state(&e, &secret), ObjectState::Purged);
    assert_eq!(
        redaction_state(&e, &secret).await,
        Some(RedactionState::Purged)
    );
    assert!(!has_key(&e, &live_key(&e, &secret)));
    assert!(!has_key(&e, &q_key(&e, &secret)));
    // Purge does not bump the epoch; the object stays redacted.
    assert_eq!(e.red.list(0).await.unwrap().1, 1);
    assert_err(
        &e.objects.get(&secret).await.unwrap_err(),
        ErrorCode::Redacted,
        None,
    );
    assert_eq!(e.red.run_due_purges().await.unwrap(), 0);
}

#[tokio::test]
async fn default_quarantine_applies_when_purge_after_is_none() {
    let cfg = RedactionConfig {
        default_quarantine: Duration::from_secs(600),
        ..RedactionConfig::default()
    };
    let e = env_with(cfg).await;
    let (secret, _, _) = sample(&e).await;
    let t0 = e.clock_now();
    e.red
        .redact(&[secret], "leak", &admin(), None)
        .await
        .unwrap();
    let (rows, _) = e.red.list(0).await.unwrap();
    assert_eq!(rows[0].purge_after, t0 + Duration::from_secs(600));
    assert_eq!(rows[0].created_at, t0);
    e.clock.advance(Duration::from_secs(600));
    assert_eq!(e.red.run_due_purges().await.unwrap(), 1);
}

impl Env {
    fn clock_now(&self) -> SystemTime {
        use scion_core::Clock;
        self.clock.now()
    }
}

#[tokio::test]
async fn unredact_from_quarantine_restores_bytes_and_bumps_epoch() {
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    let original = e.objects.get(&secret).await.unwrap();
    let frame_before = e.mem.blobs.raw(&live_key(&e, &secret)).unwrap();
    let out = e
        .red
        .redact(&[secret], "oops", &admin(), None)
        .await
        .unwrap();
    assert_err(
        &e.objects.get(&secret).await.unwrap_err(),
        ErrorCode::Redacted,
        None,
    );

    let epoch = e.red.unredact(&[secret], &admin()).await.unwrap();
    assert_eq!(epoch, out.epoch + 1);
    assert_eq!(e.red.list(0).await.unwrap(), (vec![], epoch));
    assert_eq!(state(&e, &secret), ObjectState::Live);
    assert_eq!(e.objects.get(&secret).await.unwrap(), original);
    assert_eq!(
        e.mem.blobs.raw(&live_key(&e, &secret)).unwrap(),
        frame_before
    );
    assert!(!has_key(&e, &q_key(&e, &secret)));
    // Unredacting again: no longer quarantined.
    assert_err(
        &e.red.unredact(&[secret], &admin()).await.unwrap_err(),
        ErrorCode::InvalidArgument,
        Some(subcode::NOT_QUARANTINED),
    );
}

#[tokio::test]
async fn unredact_after_purge_fails_not_quarantined() {
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    e.red
        .redact(&[secret], "leak", &admin(), None)
        .await
        .unwrap();
    e.red
        .purge_now(&[secret], &admin(), "confirmed")
        .await
        .unwrap();
    let epoch = e.red.list(0).await.unwrap().1;
    assert_err(
        &e.red.unredact(&[secret], &admin()).await.unwrap_err(),
        ErrorCode::InvalidArgument,
        Some(subcode::NOT_QUARANTINED),
    );
    assert_eq!(e.red.list(0).await.unwrap().1, epoch);
    assert_eq!(state(&e, &secret), ObjectState::Purged);
    // A live object is not quarantined either; no effects on a mixed request.
    let other = upload(&e, "other\n").await;
    e.red.redact(&[other], "x", &admin(), None).await.unwrap();
    let live = upload(&e, "live\n").await;
    assert_err(
        &e.red.unredact(&[other, live], &admin()).await.unwrap_err(),
        ErrorCode::InvalidArgument,
        Some(subcode::NOT_QUARANTINED),
    );
    assert_eq!(state(&e, &other), ObjectState::Quarantined);
    assert!(has_key(&e, &q_key(&e, &other)));
}

fn dkey(ty: &str, key: &[u8]) -> DerivedKey {
    DerivedKey {
        ty: ty.to_owned(),
        version: 1,
        key: Bytes::copy_from_slice(key),
    }
}

fn drow(ty: &str, key: &[u8], source: Option<ObjectId>, at: SystemTime) -> DerivedRow {
    DerivedRow {
        key: dkey(ty, key),
        value: Some(Bytes::from_static(b"value")),
        storage_key: None,
        source_object: source,
        at,
    }
}

#[tokio::test]
async fn purge_deletes_bytes_inline_data_and_content_bearing_derived_records() {
    let e = env().await;
    let (secret, c1, _) = sample(&e).await;
    let other = upload(&e, "unrelated\n").await;
    let now = e.clock_now();
    let meta = e.objects.storage().meta.clone();
    meta.put_derived(vec![
        drow(CONTENT_TY, b"secret", Some(secret), now),
        drow(PLAIN_TY, b"secret-plain", Some(secret), now),
        drow(CONTENT_TY, b"other", Some(other), now),
    ])
    .await
    .unwrap();

    // Simulate a crash mid-move so both the live and the quarantine keys exist.
    e.objects
        .storage()
        .faults
        .arm(points::QUARANTINE_AFTER_COPY, FaultAction::Crash, Some(1));
    e.red
        .redact(&[secret], "leak", &admin(), None)
        .await
        .unwrap();
    e.objects.storage().faults.reset();
    assert!(has_key(&e, &live_key(&e, &secret)));
    assert!(has_key(&e, &q_key(&e, &secret)));

    // An inline Meta: its bytes live in the row.
    let meta_id = c1.meta_id;
    let row = |id: ObjectId| {
        e.mem
            .meta
            .all_objects()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap()
    };
    assert!(row(meta_id).inline_data.is_some());
    e.red
        .redact(&[meta_id], "pii in message", &admin(), None)
        .await
        .unwrap();
    assert!(row(meta_id).inline_data.is_some(), "kept until purge");

    e.red
        .purge_now(&[secret, meta_id], &admin(), "confirmed leak")
        .await
        .unwrap();

    assert!(!has_key(&e, &live_key(&e, &secret)));
    assert!(!has_key(&e, &q_key(&e, &secret)));
    assert_eq!(row(secret).state, ObjectState::Purged);
    assert_eq!(row(meta_id).state, ObjectState::Purged);
    assert_eq!(row(meta_id).inline_data, None);
    let c = Consistency::Strong;
    assert_eq!(
        meta.get_derived(&dkey(CONTENT_TY, b"secret"), c)
            .await
            .unwrap(),
        None
    );
    assert!(
        meta.get_derived(&dkey(PLAIN_TY, b"secret-plain"), c)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        meta.get_derived(&dkey(CONTENT_TY, b"other"), c)
            .await
            .unwrap()
            .is_some()
    );
    // The unrelated blob is untouched.
    assert_eq!(
        e.objects.get(&other).await.unwrap(),
        Bytes::from_static(b"unrelated\n")
    );
    // Purging again is a no-op.
    e.red.purge_now(&[secret], &admin(), "again").await.unwrap();
}

#[tokio::test]
async fn purge_now_requires_quarantine() {
    let e = env().await;
    let live = upload(&e, "live\n").await;
    assert_err(
        &e.red.purge_now(&[live], &admin(), "r").await.unwrap_err(),
        ErrorCode::InvalidArgument,
        Some(subcode::NOT_QUARANTINED),
    );
    assert_eq!(state(&e, &live), ObjectState::Live);
    assert!(has_key(&e, &live_key(&e, &live)));
    let unknown = ObjectId::hash_blob(b"never uploaded");
    assert_err(
        &e.red
            .purge_now(&[unknown], &admin(), "r")
            .await
            .unwrap_err(),
        ErrorCode::NotFound,
        None,
    );
}

#[tokio::test]
async fn a_meta_object_can_be_redacted() {
    let e = env().await;
    let (_, c1, c2) = sample(&e).await;
    let before = identity_snapshot(&e);
    e.objects.get_meta(&c1.meta_id).await.unwrap(); // warm the decoded LRU
    let out = e
        .red
        .redact(&[c1.meta_id], "pii", &admin(), None)
        .await
        .unwrap();
    assert_eq!(out.epoch, 1);
    assert_eq!(state(&e, &c1.meta_id), ObjectState::Quarantined);
    assert_err(
        &e.objects.get_meta(&c1.meta_id).await.unwrap_err(),
        ErrorCode::Redacted,
        None,
    );
    assert_eq!(identity_snapshot(&e), before);
    let rec = e.h.changeset(&c1.changeset_id).await.unwrap();
    assert_eq!(rec.changeset.meta, c1.meta_id);
    assert!(e.objects.get_meta(&c2.meta_id).await.is_ok());
    // Unredact restores the inline Meta.
    e.red.unredact(&[c1.meta_id], &admin()).await.unwrap();
    assert_eq!(
        e.objects.get_meta(&c1.meta_id).await.unwrap().message,
        b"add files"
    );
}

#[tokio::test]
async fn trees_and_changesets_are_not_redactable_and_unknown_ids_are_not_found() {
    let e = env().await;
    let (secret, c1, _) = sample(&e).await;
    for id in [c1.root_tree, c1.changeset_id, e.root] {
        assert_err(
            &e.red.redact(&[id], "r", &admin(), None).await.unwrap_err(),
            ErrorCode::InvalidArgument,
            Some(subcode::NOT_REDACTABLE),
        );
    }
    // A mixed request has no effect.
    assert_err(
        &e.red
            .redact(&[secret, c1.root_tree], "r", &admin(), None)
            .await
            .unwrap_err(),
        ErrorCode::InvalidArgument,
        Some(subcode::NOT_REDACTABLE),
    );
    let unknown = ObjectId::hash_blob(b"never uploaded");
    assert_err(
        &e.red
            .redact(&[secret, unknown], "r", &admin(), None)
            .await
            .unwrap_err(),
        ErrorCode::NotFound,
        None,
    );
    assert_err(
        &e.red.redact(&[], "r", &admin(), None).await.unwrap_err(),
        ErrorCode::InvalidArgument,
        Some(subcode::MISSING_FIELD),
    );
    assert_eq!(e.red.list(0).await.unwrap(), (vec![], 0));
    assert_eq!(state(&e, &secret), ObjectState::Live);
    assert!(
        e.objects
            .storage()
            .meta
            .audit_log(100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn every_action_writes_an_audit_row_with_actor_and_reason() {
    let e = env().await;
    let (secret, c1, _) = sample(&e).await;
    let b2 = upload(&e, "second secret\n").await;
    let b3 = upload(&e, "third secret\n").await;
    let meta = e.objects.storage().meta.clone();

    e.red
        .redact(&[secret, b2], "leaked key", &admin(), Some(HOUR))
        .await
        .unwrap();
    e.red
        .unredact_with_reason(&[b2], &admin(), "false positive")
        .await
        .unwrap();
    e.red
        .redact(&[c1.meta_id], "pii", &admin(), None)
        .await
        .unwrap();
    let ops = Principal::Service("ops".to_owned());
    e.red
        .purge_now(&[c1.meta_id], &ops, "legal request")
        .await
        .unwrap();
    e.red.redact(&[b3], "dup", &admin(), None).await.unwrap();
    e.red.unredact(&[b3], &admin()).await.unwrap();
    e.clock.advance(HOUR);
    assert_eq!(e.red.run_due_purges().await.unwrap(), 1);

    let log = meta.audit_log(100).await.unwrap();
    let rows: Vec<(String, String, String, String)> = log
        .iter()
        .map(|r| {
            (
                r.action.clone(),
                r.actor.clone(),
                r.target.clone(),
                r.reason.clone(),
            )
        })
        .collect();
    let a = admin().id_string();
    let mut first_two = [secret, b2];
    first_two.sort();
    let expected = vec![
        ("redact", a.as_str(), first_two[0].to_text(), "leaked key"),
        ("redact", a.as_str(), first_two[1].to_text(), "leaked key"),
        ("unredact", a.as_str(), b2.to_text(), "false positive"),
        ("redact", a.as_str(), c1.meta_id.to_text(), "pii"),
        (
            "purge",
            "service:ops",
            c1.meta_id.to_text(),
            "legal request",
        ),
        ("redact", a.as_str(), b3.to_text(), "dup"),
        ("unredact", a.as_str(), b3.to_text(), "unredact"),
        (
            "purge",
            "service:scion",
            secret.to_text(),
            "scheduled purge: quarantine period elapsed",
        ),
    ];
    let expected: Vec<(String, String, String, String)> = expected
        .into_iter()
        .map(|(x, y, z, w)| (x.to_owned(), y.to_owned(), z, w.to_owned()))
        .collect();
    assert_eq!(rows, expected);
    for r in &log {
        assert!(!r.actor.is_empty() && !r.reason.is_empty(), "{r:?}");
    }
}

#[tokio::test]
async fn crash_after_redact_commit_is_completed_by_move_pending_bytes() {
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    let faults = e.objects.storage().faults.clone();
    faults.arm(points::REDACT_AFTER_COMMIT, FaultAction::Crash, Some(1));
    let err = e
        .red
        .redact(&[secret], "leak", &admin(), None)
        .await
        .unwrap_err();
    assert_err(&err, ErrorCode::Unavailable, Some(subcode::INJECTED_CRASH));
    faults.reset();

    // Committed: quarantined and served to no one, but the bytes have not moved yet.
    assert_eq!(state(&e, &secret), ObjectState::Quarantined);
    assert_eq!(e.red.list(0).await.unwrap().1, 1);
    assert!(has_key(&e, &live_key(&e, &secret)));
    assert!(!has_key(&e, &q_key(&e, &secret)));
    assert_err(
        &e.objects.get(&secret).await.unwrap_err(),
        ErrorCode::Redacted,
        None,
    );

    assert_eq!(e.red.move_pending_bytes().await.unwrap(), 1);
    assert!(!has_key(&e, &live_key(&e, &secret)));
    assert!(has_key(&e, &q_key(&e, &secret)));
    // Idempotent.
    assert_eq!(e.red.move_pending_bytes().await.unwrap(), 0);
    // And the restored bytes are intact.
    e.red.unredact(&[secret], &admin()).await.unwrap();
    assert_eq!(
        e.objects.get(&secret).await.unwrap(),
        Bytes::from_static(b"password=hunter2\n")
    );
}

#[tokio::test]
async fn crash_after_quarantine_copy_is_resumed_by_move_pending_bytes() {
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    let faults = e.objects.storage().faults.clone();
    faults.arm(points::QUARANTINE_AFTER_COPY, FaultAction::Crash, Some(1));
    // The transaction committed; the deferred move does not fail the call.
    let out = e
        .red
        .redact(&[secret], "leak", &admin(), None)
        .await
        .unwrap();
    assert_eq!(out.epoch, 1);
    faults.reset();
    assert!(has_key(&e, &live_key(&e, &secret)));
    assert!(has_key(&e, &q_key(&e, &secret)));
    assert_eq!(e.red.move_pending_bytes().await.unwrap(), 1);
    assert!(!has_key(&e, &live_key(&e, &secret)));
    assert!(has_key(&e, &q_key(&e, &secret)));
    assert_eq!(e.red.move_pending_bytes().await.unwrap(), 0);
}

#[tokio::test]
async fn blob_store_outage_during_redact_is_finished_by_the_mover() {
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    e.mem.blobs.set_down(true);
    e.red
        .redact(&[secret], "leak", &admin(), None)
        .await
        .unwrap();
    assert_eq!(state(&e, &secret), ObjectState::Quarantined);
    assert!(e.red.move_pending_bytes().await.is_err());
    e.mem.blobs.set_down(false);
    assert_eq!(e.red.move_pending_bytes().await.unwrap(), 1);
    assert!(has_key(&e, &q_key(&e, &secret)));
    assert!(!has_key(&e, &live_key(&e, &secret)));
}

#[tokio::test]
async fn crash_after_purge_bytes_is_completed_by_a_rerun() {
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    e.red
        .redact(&[secret], "leak", &admin(), Some(HOUR))
        .await
        .unwrap();
    let faults = e.objects.storage().faults.clone();
    faults.arm(points::PURGE_AFTER_BYTES, FaultAction::Crash, Some(1));
    assert_err(
        &e.red.purge_now(&[secret], &admin(), "r").await.unwrap_err(),
        ErrorCode::Unavailable,
        Some(subcode::INJECTED_CRASH),
    );
    faults.reset();
    // Bytes are gone but the state is still quarantined: purge is irreversible, and a re-run
    // (here the scheduler) completes it.
    assert_eq!(state(&e, &secret), ObjectState::Quarantined);
    assert!(!has_key(&e, &q_key(&e, &secret)));
    e.clock.advance(HOUR);
    assert_eq!(e.red.run_due_purges().await.unwrap(), 1);
    assert_eq!(state(&e, &secret), ObjectState::Purged);
}

#[tokio::test]
async fn effective_by_is_commit_time_plus_propagation_bound() {
    let cfg = RedactionConfig {
        propagation_bound: Duration::from_millis(2500),
        ..RedactionConfig::default()
    };
    let e = env_with(cfg).await;
    let (secret, _, _) = sample(&e).await;
    e.clock.advance(Duration::from_secs(42));
    let t = e.clock_now();
    let out = e
        .red
        .redact(&[secret], "leak", &admin(), None)
        .await
        .unwrap();
    assert_eq!(
        out,
        RedactOutcome {
            epoch: 1,
            effective_by: t + Duration::from_millis(2500),
        }
    );
    let (rows, epoch) = e.red.list(0).await.unwrap();
    assert_eq!(epoch, 1);
    assert_eq!(rows[0].created_at, t);
    assert_eq!(rows[0].epoch, 1);
    assert_eq!(rows[0].actor, admin().id_string());
    assert_eq!(rows[0].reason, "leak");
}

#[tokio::test]
async fn list_returns_rows_newer_than_since_epoch() {
    let e = env().await;
    let a = upload(&e, "a\n").await;
    let b = upload(&e, "b\n").await;
    e.red.redact(&[a], "r1", &admin(), None).await.unwrap();
    e.red.redact(&[b], "r2", &admin(), None).await.unwrap();
    let (rows, epoch) = e.red.list(1).await.unwrap();
    assert_eq!(epoch, 2);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].object, b);
    assert_eq!(e.red.list(0).await.unwrap().0.len(), 2);
    // Re-redacting refreshes the row with the new epoch.
    let out = e.red.redact(&[a, a], "r3", &admin(), None).await.unwrap();
    assert_eq!(out.epoch, 3);
    let (rows, _) = e.red.list(2).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].object, rows[0].reason.as_str()), (a, "r3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn background_task_moves_pending_bytes_and_runs_due_purges() {
    let cfg = RedactionConfig {
        purge_interval: Duration::from_millis(10),
        ..RedactionConfig::default()
    };
    let e = env_with(cfg).await;
    let (secret, _, _) = sample(&e).await;
    let later = upload(&e, "later\n").await;
    let faults = e.objects.storage().faults.clone();
    faults.arm(points::REDACT_AFTER_COMMIT, FaultAction::Crash, Some(1));
    assert!(
        e.red
            .redact(&[later], "x", &admin(), Some(HOUR))
            .await
            .is_err()
    );
    faults.reset();
    e.red
        .redact(&[secret], "leak", &admin(), Some(Duration::ZERO))
        .await
        .unwrap();

    let handle = e.red.spawn_background();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let done = state(&e, &secret) == ObjectState::Purged
            && !has_key(&e, &live_key(&e, &later))
            && has_key(&e, &q_key(&e, &later));
        if done {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background work did not finish"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(state(&e, &later), ObjectState::Quarantined);
    handle.abort();
}

#[tokio::test]
async fn failed_unredact_transaction_moves_bytes_back_to_quarantine() {
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    let out = e
        .red
        .redact(&[secret], "leak", &admin(), None)
        .await
        .unwrap();
    let faults = e.objects.storage().faults.clone();
    faults.arm(
        "meta.unredact.before",
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    assert_err(
        &e.red.unredact(&[secret], &admin()).await.unwrap_err(),
        ErrorCode::Unavailable,
        Some(subcode::INJECTED),
    );
    assert_eq!(state(&e, &secret), ObjectState::Quarantined);
    assert_eq!(e.red.list(0).await.unwrap().1, out.epoch);
    assert!(!has_key(&e, &live_key(&e, &secret)));
    assert!(has_key(&e, &q_key(&e, &secret)));
    // A retry succeeds.
    e.red.unredact(&[secret], &admin()).await.unwrap();
    assert_eq!(
        e.objects.get(&secret).await.unwrap(),
        Bytes::from_static(b"password=hunter2\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn background_task_stops_when_the_redactor_is_dropped() {
    let e = env_with(RedactionConfig {
        purge_interval: Duration::ZERO,
        ..RedactionConfig::default()
    })
    .await;
    let handle = e.red.spawn_background();
    drop(e);
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("task ends")
        .expect("no panic");
}

#[tokio::test]
async fn unredact_after_an_interrupted_purge_fails_and_the_purge_completes() {
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    let out = e
        .red
        .redact(&[secret], "leak", &admin(), Some(HOUR))
        .await
        .unwrap();
    let faults = e.objects.storage().faults.clone();
    faults.arm(points::PURGE_AFTER_BYTES, FaultAction::Crash, Some(1));
    assert!(e.red.purge_now(&[secret], &admin(), "r").await.is_err());
    faults.reset();
    assert_eq!(state(&e, &secret), ObjectState::Quarantined);
    let audit_before = e.objects.storage().meta.audit_log(100).await.unwrap();

    // Purge is irreversible: the bytes are gone, so the object must not become live again.
    assert_err(
        &e.red.unredact(&[secret], &admin()).await.unwrap_err(),
        ErrorCode::InvalidArgument,
        Some(subcode::NOT_QUARANTINED),
    );
    assert_eq!(state(&e, &secret), ObjectState::Quarantined);
    assert_eq!(e.red.list(0).await.unwrap().1, out.epoch);
    assert_eq!(
        e.objects.storage().meta.audit_log(100).await.unwrap(),
        audit_before
    );
    assert!(!has_key(&e, &live_key(&e, &secret)));
    assert!(!has_key(&e, &q_key(&e, &secret)));

    e.red.purge_now(&[secret], &admin(), "r").await.unwrap();
    assert_eq!(state(&e, &secret), ObjectState::Purged);
}

#[tokio::test]
async fn unredact_of_bytes_never_moved_keeps_them_live() {
    // The redact transaction committed but the move never ran (crash at redact.after_commit):
    // unredact finds the bytes still at the live key and restores the object.
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    let faults = e.objects.storage().faults.clone();
    faults.arm(points::REDACT_AFTER_COMMIT, FaultAction::Crash, Some(1));
    assert!(
        e.red
            .redact(&[secret], "leak", &admin(), None)
            .await
            .is_err()
    );
    faults.reset();
    e.red.unredact(&[secret], &admin()).await.unwrap();
    assert_eq!(state(&e, &secret), ObjectState::Live);
    assert_eq!(
        e.objects.get(&secret).await.unwrap(),
        Bytes::from_static(b"password=hunter2\n")
    );
    assert_eq!(e.red.move_pending_bytes().await.unwrap(), 0);
    assert!(has_key(&e, &live_key(&e, &secret)));
}

#[tokio::test]
async fn steady_state_mover_runs_do_not_probe_object_storage() {
    let e = env().await;
    let (secret, c1, _) = sample(&e).await;
    let b2 = upload(&e, "second\n").await;
    let faults = e.objects.storage().faults.clone();
    faults.arm(points::REDACT_AFTER_COMMIT, FaultAction::Crash, Some(1));
    assert!(
        e.red
            .redact(&[secret, b2, c1.meta_id], "leak", &admin(), None)
            .await
            .is_err()
    );
    faults.reset();
    assert_eq!(e.red.move_pending_bytes().await.unwrap(), 2);

    let exists = scion_core::points::blob_before("exists");
    faults.reset_hits();
    assert_eq!(e.red.move_pending_bytes().await.unwrap(), 0);
    assert_eq!(faults.hits(&exists), 0, "confirmed moves are not re-probed");

    // Unredact + re-redact (crashing before the move again) makes the object pending anew.
    e.red.unredact(&[b2], &admin()).await.unwrap();
    faults.arm(points::REDACT_AFTER_COMMIT, FaultAction::Crash, Some(1));
    assert!(e.red.redact(&[b2], "again", &admin(), None).await.is_err());
    faults.reset();
    assert!(has_key(&e, &live_key(&e, &b2)));
    faults.reset_hits();
    assert_eq!(e.red.move_pending_bytes().await.unwrap(), 1);
    assert_eq!(faults.hits(&exists), 1);
    assert!(!has_key(&e, &live_key(&e, &b2)));
    assert!(has_key(&e, &q_key(&e, &b2)));
}

#[tokio::test]
async fn one_failing_scheduled_purge_does_not_block_the_others() {
    let e = env().await;
    let a = upload(&e, "a secret\n").await;
    let b = upload(&e, "b secret\n").await;
    e.red
        .redact(&[a, b], "leak", &admin(), Some(Duration::ZERO))
        .await
        .unwrap();
    let mut ids = [a, b];
    ids.sort();
    let faults = e.objects.storage().faults.clone();
    // The first purge's first blob delete fails.
    faults.arm(
        &scion_core::points::blob_before("delete"),
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    assert_err(
        &e.red.run_due_purges().await.unwrap_err(),
        ErrorCode::Unavailable,
        Some(subcode::INJECTED),
    );
    assert_eq!(state(&e, &ids[0]), ObjectState::Quarantined);
    assert_eq!(state(&e, &ids[1]), ObjectState::Purged);
    // The next run retries the failed one.
    assert_eq!(e.red.run_due_purges().await.unwrap(), 1);
    assert_eq!(state(&e, &ids[0]), ObjectState::Purged);
    assert!(!has_key(&e, &q_key(&e, &ids[0])));
}

#[tokio::test]
async fn re_derivation_reads_redacted_and_purged_content_as_empty() {
    // SPEC 13.5 / 19.13: non-content-bearing values stay, and later re-derivation is
    // deterministic because quarantined and purged content both read as empty.
    let e = env().await;
    let (secret, _, _) = sample(&e).await;
    let ctx = e.derived.ctx();
    assert_eq!(
        ctx.blob_or_empty(&secret).await.unwrap(),
        Bytes::from_static(b"password=hunter2\n")
    );
    e.red
        .redact(&[secret], "leak", &admin(), None)
        .await
        .unwrap();
    let quarantined = ctx.blob_or_empty(&secret).await.unwrap();
    assert!(quarantined.is_empty());
    e.red.purge_now(&[secret], &admin(), "r").await.unwrap();
    assert_eq!(ctx.blob_or_empty(&secret).await.unwrap(), quarantined);
}
