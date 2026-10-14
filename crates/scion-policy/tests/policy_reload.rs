//! SPEC 19.12 policy reload (P05), SPEC 6.2 reload semantics and SPEC 16.1 class 9: valid
//! changes are applied atomically and change the epoch; equivalent files keep the epoch;
//! invalid changes keep the last good policy and surface `last_error`; invalid or missing files
//! at startup fail; readers never observe a torn policy.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_core::Principal;
use scion_policy::{Policy, PolicyError, PolicyHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn gh(login: &str) -> Principal {
    Principal::GitHub {
        login: login.into(),
        id: None,
    }
}

/// A policy where only `reader` can read `/` and only `writer` can write `/`.
fn policy_text(reader: &str, writer: &str) -> String {
    format!(
        "version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"github:{reader}\"]\nwrite = [\"github:{writer}\"]\n"
    )
}

fn write(path: &Path, text: &str) {
    // Write to a temp file and rename, as config managers do.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn setup(text: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("acl.toml");
    write(&path, text);
    (dir, path)
}

#[test]
fn startup_with_invalid_or_missing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("acl.toml");
    assert!(matches!(
        PolicyHandle::load(&missing),
        Err(PolicyError::Io(_))
    ));
    write(&missing, "version = 1\n");
    assert_eq!(
        PolicyHandle::load(&missing).err(),
        Some(PolicyError::MissingRootRule)
    );
    write(&missing, "not toml [");
    assert!(matches!(
        PolicyHandle::load(&missing),
        Err(PolicyError::Toml(_))
    ));
    std::fs::write(&missing, [0xff, 0xfe, 0x00]).unwrap();
    assert!(PolicyHandle::load(&missing).is_err());
}

#[test]
fn valid_change_is_applied_and_epoch_changes() {
    let (_dir, path) = setup(&policy_text("a", "a"));
    let h = PolicyHandle::load(&path).unwrap();
    let mut rx = h.subscribe();
    let e0 = h.epoch();
    assert_eq!(*rx.borrow(), e0);
    assert_eq!(e0, Policy::parse(&policy_text("a", "a")).unwrap().epoch());
    let before = h.current();
    assert!(before.can_read_bytes(&gh("a"), b"x"));

    // Unchanged file: no change.
    assert_eq!(h.reload_now(), Ok(false));
    assert!(!rx.has_changed().unwrap());

    write(&path, &policy_text("b", "b"));
    assert_eq!(h.reload_now(), Ok(true));
    let e1 = h.epoch();
    assert_ne!(e0, e1);
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), e1);
    let after = h.current();
    assert_eq!(after.epoch(), e1);
    assert!(after.can_read_bytes(&gh("b"), b"x"));
    assert!(!after.can_read_bytes(&gh("a"), b"x"));
    // A previously taken snapshot is unaffected (atomic swap of whole policies).
    assert!(before.can_read_bytes(&gh("a"), b"x"));
    assert_eq!(before.epoch(), e0);
    assert_eq!(h.last_error(), None);
}

#[test]
fn equivalent_reformatted_file_keeps_epoch() {
    let (_dir, path) = setup(&policy_text("alice", "bob"));
    let h = PolicyHandle::load(&path).unwrap();
    let e0 = h.epoch();
    let rx = h.subscribe();
    write(
        &path,
        "# reformatted\nversion=1\n\n[[paths]]\nwrite=[ \"github:BOB\" ]\nread=[\"github:Alice\",\"github:alice\"]\nprefix=\"/\"\n",
    );
    assert_eq!(h.reload_now(), Ok(false));
    assert_eq!(h.epoch(), e0);
    assert!(!rx.has_changed().unwrap());
    assert_eq!(h.last_error(), None);
}

#[test]
fn invalid_change_keeps_last_good() {
    let (_dir, path) = setup(&policy_text("a", "a"));
    let h = PolicyHandle::load(&path).unwrap();
    let e0 = h.epoch();
    let rx = h.subscribe();

    let invalid: &[(&str, fn(&PolicyError) -> bool)] = &[
        ("version = 1\n", |e| *e == PolicyError::MissingRootRule),
        ("version = 9\n[[paths]]\nprefix = \"/\"\n", |e| {
            *e == PolicyError::UnsupportedVersion(9)
        }),
        ("version = 1\n[[paths]]\nprefix = \"/\"\nbogus = 1\n", |e| {
            *e == PolicyError::UnknownKey("paths[0].bogus".into())
        }),
        ("this is [not toml", |e| matches!(e, PolicyError::Toml(_))),
        (
            "version = 1\n[groups]\na = [\"group:a\"]\n[[paths]]\nprefix = \"/\"\n",
            |e| matches!(e, PolicyError::GroupCycle(_)),
        ),
    ];
    for (text, check) in invalid {
        write(&path, text);
        let err = h.reload_now().unwrap_err();
        assert!(check(&err), "{text}: {err:?}");
        assert_eq!(h.last_error(), Some(err));
        assert_eq!(h.epoch(), e0);
        assert!(h.current().can_read_bytes(&gh("a"), b"x"));
        assert!(!rx.has_changed().unwrap());
    }

    // A missing file during reload (e.g. mid-replacement) also keeps the last good policy.
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(h.reload_now(), Err(PolicyError::Io(_))));
    assert!(matches!(h.last_error(), Some(PolicyError::Io(_))));
    assert_eq!(h.epoch(), e0);

    // Restoring the same valid content clears the error without an epoch change.
    write(&path, &policy_text("a", "a"));
    assert_eq!(h.reload_now(), Ok(false));
    assert_eq!(h.last_error(), None);
    assert_eq!(h.epoch(), e0);

    // A later valid change is applied normally.
    write(&path, &policy_text("c", "c"));
    assert_eq!(h.reload_now(), Ok(true));
    assert_ne!(h.epoch(), e0);
    assert!(rx.has_changed().unwrap());
    assert_eq!(h.last_error(), None);
}

#[test]
fn from_policy_handle_is_fixed() {
    let h = PolicyHandle::from_policy(Policy::allow_all_for_tests());
    let e = h.epoch();
    assert_eq!(e, Policy::allow_all_for_tests().epoch());
    assert_eq!(h.reload_now(), Ok(false));
    assert_eq!(h.last_error(), None);
    assert_eq!(*h.subscribe().borrow(), e);
    assert_eq!(h.current().epoch(), e);
}

/// Readers never observe a torn policy: every snapshot is exactly policy A or policy B, its
/// epoch matches its content, and `epoch()` is always one of the two.
#[test]
fn readers_never_observe_torn_policy() {
    let text_a = policy_text("alpha", "alpha");
    let text_b = policy_text("beta", "beta");
    let ea = Policy::parse(&text_a).unwrap().epoch();
    let eb = Policy::parse(&text_b).unwrap().epoch();
    assert_ne!(ea, eb);
    let (_dir, path) = setup(&text_a);
    let h = PolicyHandle::load(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let alpha = gh("alpha");
    let beta = gh("beta");

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let h = Arc::clone(&h);
            let stop = Arc::clone(&stop);
            let (alpha, beta) = (alpha.clone(), beta.clone());
            std::thread::spawn(move || {
                let mut seen = [0u64; 2];
                while !stop.load(Ordering::Relaxed) {
                    let snap = h.current();
                    let a_read = snap.can_read_bytes(&alpha, b"f");
                    let a_write = snap.can_write_bytes(&alpha, b"f");
                    let b_read = snap.can_read_bytes(&beta, b"f");
                    let b_write = snap.can_write_bytes(&beta, b"f");
                    if snap.epoch() == ea {
                        assert!(a_read && a_write && !b_read && !b_write);
                        seen[0] += 1;
                    } else {
                        assert_eq!(snap.epoch(), eb);
                        assert!(b_read && b_write && !a_read && !a_write);
                        seen[1] += 1;
                    }
                    let e = h.epoch();
                    assert!(e == ea || e == eb);
                }
                seen
            })
        })
        .collect();

    let rounds = if scion_core::test_support::full_mode() {
        2000
    } else {
        200
    };
    for i in 0..rounds {
        let (text, want) = if i % 2 == 0 {
            (&text_b, eb)
        } else {
            (&text_a, ea)
        };
        write(&path, text);
        assert_eq!(h.reload_now(), Ok(true));
        assert_eq!(h.epoch(), want);
        if i % 10 == 0 {
            // Interleave invalid files: they must never become visible.
            write(&path, "version = 1\n");
            assert!(h.reload_now().is_err());
            assert_eq!(h.epoch(), want);
        }
    }
    stop.store(true, Ordering::Relaxed);
    let mut total = [0u64; 2];
    for r in readers {
        let s = r.join().unwrap();
        total[0] += s[0];
        total[1] += s[1];
    }
    assert!(total[0] + total[1] > 0);
}

/// Concurrent reloads are serialized: the published epoch always matches the stored policy.
#[test]
fn concurrent_reloads_publish_consistent_epochs() {
    let (_dir, path) = setup(&policy_text("x", "x"));
    let h = PolicyHandle::load(&path).unwrap();
    let rx = h.subscribe();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let h = Arc::clone(&h);
            std::thread::spawn(move || {
                for _ in 0..50 {
                    let _ = h.reload_now();
                }
            })
        })
        .collect();
    for i in 0..50 {
        let who = if i % 2 == 0 { "y" } else { "x" };
        write(&path, &policy_text(who, who));
    }
    for t in threads {
        t.join().unwrap();
    }
    let _ = h.reload_now();
    assert_eq!(*rx.borrow(), h.epoch());
}

async fn wait_for(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_applies_changes_and_keeps_last_good() {
    let (_dir, path) = setup(&policy_text("a", "a"));
    let h = PolicyHandle::load(&path).unwrap();
    let e0 = h.epoch();
    let mut rx = h.subscribe();
    let task = h.spawn_watcher();

    write(&path, &policy_text("b", "b"));
    // notify or, at worst, the 1 s polling fallback picks it up.
    tokio::time::timeout(Duration::from_secs(10), rx.changed())
        .await
        .expect("watcher did not publish an epoch change")
        .unwrap();
    let e1 = *rx.borrow_and_update();
    assert_ne!(e1, e0);
    assert_eq!(h.epoch(), e1);
    assert!(h.current().can_read_bytes(&gh("b"), b"x"));

    // An invalid file keeps the last good policy and surfaces the error.
    write(&path, "version = 1\n");
    assert!(
        wait_for(Duration::from_secs(10), || h.last_error()
            == Some(PolicyError::MissingRootRule))
        .await
    );
    assert_eq!(h.epoch(), e1);

    // Fixing it clears the error and applies the new policy.
    write(&path, &policy_text("c", "c"));
    assert!(wait_for(Duration::from_secs(10), || h.last_error().is_none()).await);
    tokio::time::timeout(Duration::from_secs(10), rx.changed())
        .await
        .expect("no epoch change after fix")
        .unwrap();
    assert!(h.current().can_read_bytes(&gh("c"), b"x"));

    task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_exits_when_handle_dropped() {
    let (_dir, path) = setup(&policy_text("a", "a"));
    let h = PolicyHandle::load(&path).unwrap();
    let task = h.spawn_watcher();
    drop(h);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("watcher task did not exit")
        .unwrap();
    // A fixed handle has nothing to watch.
    let fixed = PolicyHandle::from_policy(Policy::allow_all_for_tests());
    tokio::time::timeout(Duration::from_secs(5), fixed.spawn_watcher())
        .await
        .unwrap()
        .unwrap();
}
