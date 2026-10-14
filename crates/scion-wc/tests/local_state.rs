//! State, index, lock and credentials (SPEC 6.4, 11.1, 11.5, 17, 19.10, 19.11) for P11.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code (PLAN 5.1)

mod local_support;

use local_support::{rp, rpb};
use scion_core::{ChangeId, ErrorCode, Kind, Mode, ObjectId};
use scion_object::RefName;
use scion_wc::credentials::{self, Credentials};
use scion_wc::index::{INDEX_FILE, IndexEntry, StatCache, StatTuple};
use scion_wc::lock;
use scion_wc::state::{self, PendingOperation, STATE_FILE, WcState};
use std::os::unix::fs::PermissionsExt;

fn entry(n: u8, redacted: bool) -> IndexEntry {
    IndexEntry {
        stat: StatTuple {
            size: u64::from(n) * 1000,
            mtime_ns: 1_700_000_000_123_456_789 + i128::from(n),
            ctime_ns: -5 - i128::from(n),
            inode: u64::MAX - u64::from(n),
            mode: 0o100644 | (u32::from(n % 2) * 0o111),
        },
        id: ObjectId::new(Kind::Blob, [n; 32]),
        mode: if n % 2 == 1 {
            Mode::Executable
        } else {
            Mode::File
        },
        redacted,
    }
}

#[test]
fn index_round_trip_and_atomic_save() {
    require_p01!();
    let dir = tempfile::tempdir().unwrap();
    let mut idx = StatCache::default();
    idx.entries.insert(rp("a"), entry(1, false));
    idx.entries.insert(rp("dir/b c"), entry(2, true));
    idx.entries.insert(rpb(b"\xff\xfe/x"), entry(3, false));
    idx.save(dir.path()).unwrap();
    let w1 = idx.written_at_ns;
    assert!(w1 > 0);
    let back = StatCache::load(dir.path()).unwrap();
    assert_eq!(back.entries, idx.entries);
    assert_eq!(back.written_at_ns, w1);

    // The index's written_at comes from the filesystem clock: a file written afterwards is
    // racily clean or newer, never older.
    std::fs::write(dir.path().join("after"), b"x").unwrap();
    let md = std::fs::metadata(dir.path().join("after")).unwrap();
    let after_ns = {
        use std::os::unix::fs::MetadataExt;
        i128::from(md.mtime()) * 1_000_000_000 + i128::from(md.mtime_nsec())
    };
    assert!(after_ns >= w1);

    // Simulated crash mid-save: a half-written temporary file next to the index is ignored.
    let old = std::fs::read(dir.path().join(INDEX_FILE)).unwrap();
    std::fs::write(dir.path().join(".index.tmp-crash"), &old[..old.len() / 2]).unwrap();
    let back = StatCache::load(dir.path()).unwrap();
    assert_eq!(back.entries, idx.entries);

    // A newer save replaces it wholesale.
    let mut idx2 = back.clone();
    idx2.entries.remove(&rp("a"));
    idx2.save(dir.path()).unwrap();
    assert!(idx2.written_at_ns >= w1);
    let back2 = StatCache::load(dir.path()).unwrap();
    assert_eq!(back2.entries.len(), 2);
    assert!(!back2.entries.contains_key(&rp("a")));
}

#[test]
fn state_round_trip_with_paths_and_ref() {
    require_p01!();
    let dir = tempfile::tempdir().unwrap();
    let base = ObjectId::new(Kind::Changeset, [4; 32]);
    let s = WcState {
        endpoint: Some("https://scion.example:443".into()),
        base,
        tracking_ref: Some(RefName::parse("main").unwrap()),
        sparse: vec![rp("src"), rp("docs/guide"), rpb(b"bin\xff")],
        inaccessible: vec![rp("src/secret")],
        pending: Some(PendingOperation::SparseChange {
            old: vec![rp("src")],
            new: vec![rp("src"), rp("docs/guide")],
        }),
    };
    state::save(dir.path(), &s).unwrap();
    assert_eq!(state::load(dir.path()).unwrap(), s);
    let bytes = std::fs::read(dir.path().join(STATE_FILE)).unwrap();
    state::save(dir.path(), &state::load(dir.path()).unwrap()).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join(STATE_FILE)).unwrap(),
        bytes,
        "deterministic"
    );

    for pending in [
        PendingOperation::Checkout { target: base },
        PendingOperation::Goto {
            from: base,
            to: base,
        },
        PendingOperation::Commit {
            parent: base,
            change_id: ChangeId([0xab; 16]),
            amend: false,
        },
    ] {
        let s2 = WcState {
            pending: Some(pending),
            ..s.clone()
        };
        state::save(dir.path(), &s2).unwrap();
        assert_eq!(state::load(dir.path()).unwrap(), s2);
    }
}

#[test]
fn state_without_paths_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let s = WcState {
        endpoint: None,
        base: ObjectId::new(Kind::Changeset, [1; 32]),
        tracking_ref: None,
        sparse: vec![],
        inaccessible: vec![],
        pending: None,
    };
    state::save(dir.path(), &s).unwrap();
    assert_eq!(state::load(dir.path()).unwrap(), s);
}

#[test]
fn lock_excludes_a_second_holder() {
    let dir = tempfile::tempdir().unwrap();
    let held = lock::acquire(dir.path()).unwrap();
    for _ in 0..3 {
        assert_eq!(
            lock::acquire(dir.path()).unwrap_err().code,
            ErrorCode::Unavailable
        );
    }
    // Another thread is excluded as well.
    let d = dir.path().to_path_buf();
    let other = std::thread::spawn(move || lock::acquire(&d).is_err())
        .join()
        .unwrap();
    assert!(other);
    drop(held);
    let _again = lock::acquire(dir.path()).unwrap();
}

#[test]
fn credentials_file_is_0600_and_strict_on_read() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join(".config/scion/credentials");
    let c = Credentials {
        token: "v4.public.token".into(),
        github_token: Some("gho_xyz".into()),
    };
    credentials::write(&path, &c).unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    assert_eq!(credentials::read(&path).unwrap(), c);

    for bad in [0o640, 0o604] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(bad)).unwrap();
        let err = credentials::read(&path).unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        assert!(err.message.contains("readable by group or others"));
    }
    // Rewriting restores 0600.
    credentials::write(&path, &c).unwrap();
    assert_eq!(mode(&path), 0o600);
    assert_eq!(credentials::read(&path).unwrap(), c);
}
