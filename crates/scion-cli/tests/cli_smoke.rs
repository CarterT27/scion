//! Smoke suite (P31): every command's human output on a scripted session against a
//! [`scion_conformance::TestCluster`], exit codes (PLAN 6.15), short ids for humans, the daemon
//! endpoint, and the real `scion` binary through `assert_cmd`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod cli_support;

use assert_cmd::Command;
use cli_support::{Harness, by_label, scion, scripted_session};
use scion_conformance::TestCluster;

/// `c1` followed by exactly 12 hex digits somewhere in `s`, and no full id.
fn has_short_id_only(s: &str) -> bool {
    let full = s
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| w.len() == 66 && w.starts_with("c1"));
    let short = s.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        w.len() == 14 && w.starts_with("c1") && w[2..].chars().all(|c| c.is_ascii_hexdigit())
    });
    short && !full
}

#[tokio::test(flavor = "multi_thread")]
async fn human_output_of_every_command() {
    let h = Harness::start(false).await;
    let steps = scripted_session(&h, false).await;
    if std::env::var("SCION_SHOW_OUTPUT").is_ok() {
        for s in &steps {
            println!(
                "== {} (exit {})\n{}{}",
                s.label,
                s.out.code,
                s.out.text(),
                s.out.stderr
            );
        }
    }
    let by = by_label(&steps);
    let out = |l: &str| by[l].out.text();
    let err = |l: &str| by[l].out.stderr.clone();

    assert!(
        out("login").contains("Logged in as service:ci"),
        "{}",
        out("login")
    );
    assert!(
        out("whoami").starts_with("github:alice\n"),
        "{}",
        out("whoami")
    );
    assert!(out("whoami").contains("groups: eng"), "{}", out("whoami"));
    assert!(
        out("checkout").contains("Checked out c1"),
        "{}",
        out("checkout")
    );
    assert!(out("checkout").contains("(main)"), "{}", out("checkout"));
    assert!(has_short_id_only(&out("checkout")), "{}", out("checkout"));
    let st = out("status-dirty");
    assert!(
        st.contains("A a.txt\n") && st.contains("A dir/b.txt\n"),
        "{st}"
    );
    assert!(out("diff").contains("A a.txt"), "{}", out("diff"));
    assert!(
        out("commit").starts_with("Committed c1"),
        "{}",
        out("commit")
    );
    assert!(has_short_id_only(&out("commit")), "{}", out("commit"));
    assert!(
        out("amend").starts_with("Amended as c1"),
        "{}",
        out("amend")
    );
    assert!(
        out("land").starts_with("Landed on main@"),
        "{}",
        out("land")
    );
    let log = out("log");
    assert!(log.contains("    add files (amended)\n"), "{log}");
    assert!(log.contains("Author: github:alice\n"), "{log}");
    assert!(has_short_id_only(&log), "{log}");
    let oneline = out("log-path");
    assert_eq!(oneline.lines().count(), 1, "{oneline}");
    assert!(oneline.ends_with(" add files (amended)\n"), "{oneline}");
    let show = out("show");
    assert!(
        show.contains("A a.txt\n") && show.contains("+one\n"),
        "{show}"
    );
    assert!(show.contains("Change-Id: "), "{show}");
    assert_eq!(by["cat"].out.stdout, b"one\ntwo\n");
    let blame = out("blame");
    assert!(
        blame.contains("    1) one\n") && blame.contains("    2) two\n"),
        "{blame}"
    );
    assert!(has_short_id_only(&blame), "{blame}");
    assert!(
        out("sparse-add").contains("Sparse profile: dir, a.txt"),
        "{}",
        out("sparse-add")
    );
    assert!(
        out("sparse-remove").contains("Sparse profile: dir\n"),
        "{}",
        out("sparse-remove")
    );
    assert!(
        out("goto-back").contains("Moved from c1"),
        "{}",
        out("goto-back")
    );
    assert!(
        out("goto-back").contains("2 removed"),
        "{}",
        out("goto-back")
    );
    assert!(
        out("goto-main").contains("2 written"),
        "{}",
        out("goto-main")
    );
    assert!(
        err("goto-dirty").contains("dirty_working_copy"),
        "{}",
        err("goto-dirty")
    );
    let rc = out("rebase-conflict");
    assert!(
        rc.contains("Rebase paused on conflicts:\nC a.txt\n"),
        "{rc}"
    );
    assert!(
        out("status-conflict").contains("C a.txt (unresolved conflict)"),
        "{}",
        out("status-conflict")
    );
    assert!(
        out("status-conflict").contains("pending operation: rebase"),
        "{}",
        out("status-conflict")
    );
    assert!(
        out("rebase-abort").starts_with("Rebase aborted; back at c1"),
        "{}",
        out("rebase-abort")
    );
    assert!(
        err("resolve-markers").contains("conflict_markers_remain"),
        "{}",
        err("resolve-markers")
    );
    assert!(
        out("resolve").contains("all conflicts resolved"),
        "{}",
        out("resolve")
    );
    assert!(
        out("commit-continue").starts_with("Committed c1"),
        "{}",
        out("commit-continue")
    );
    assert!(
        out("land-merged").starts_with("Landed on main@"),
        "{}",
        out("land-merged")
    );
    assert!(
        out("redact").contains("redacted blob b1"),
        "{}",
        out("redact")
    );
    assert!(out("redact").contains("quarantined"), "{}", out("redact"));
    assert!(
        err("cat-missing").starts_with("scion: error: NOT_FOUND"),
        "{}",
        err("cat-missing")
    );

    assert!(
        err("commit-no-message").contains("commit needs -m"),
        "{}",
        err("commit-no-message")
    );

    // The merged content landed, with the original message (SPEC 11.7: the continuing
    // commit keeps the original change id, author and message).
    let alice = h.env_as("alice");
    let top = scion(&alice, &["log", "--rev", "main", "--limit", "1"]).await;
    assert!(top.text().contains("    ours\n"), "{}", top.text());
    assert!(
        top.text().contains("Author: github:alice\n"),
        "{}",
        top.text()
    );
    let cat = scion(&alice, &["cat", "main:a.txt"]).await;
    assert_eq!(
        (cat.code, cat.stdout.as_slice()),
        (0, b"one\nMERGED\n".as_slice())
    );
    // Redacted content: ReadPath is REDACTED (exit 1).
    let red = scion(&alice, &["cat", "main:dir/b.txt"]).await;
    assert_eq!(red.code, 1);
    assert!(red.stderr.contains("REDACTED"), "{}", red.stderr);
    scion_conformance::invariants::check_16_2(&h.cluster)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_work_through_the_daemon_endpoint() {
    let h = Harness::start(true).await;
    let mut env = h.env_as("alice");
    env.0.insert("SCION_ENDPOINT".into(), h.daemon_endpoint());
    env.0.remove("SCION_CA_FILE");
    let who = scion(&env, &["--json", "whoami"]).await;
    assert_eq!(who.code, 0, "{}", who.stderr);
    assert!(
        who.json()["data"]["endpoint"]
            .as_str()
            .unwrap()
            .starts_with("unix://")
    );
    let wc = h.path("wc");
    let wcs = wc.to_str().unwrap();
    let co = scion(&env, &["checkout", "main", wcs]).await;
    assert_eq!(co.code, 0, "{}", co.stderr);
    std::fs::write(wc.join("f.txt"), b"via daemon\n").unwrap();
    let c = scion(&env, &["-C", wcs, "commit", "-m", "via daemon"]).await;
    assert_eq!(c.code, 0, "{}", c.stderr);
    let l = scion(&env, &["-C", wcs, "land", "--ref", "main"]).await;
    assert_eq!(l.code, 0, "{}", l.stderr);
    // The endpoint from SCION_ENDPOINT is pinned in .scion/config.toml at checkout.
    let mut no_ep = env.clone();
    no_ep.0.remove("SCION_ENDPOINT");
    let st = scion(&no_ep, &["-C", wcs, "--json", "status"]).await;
    assert_eq!(st.code, 0, "{}", st.stderr);
    assert_eq!(st.json()["data"]["clean"], true);
    let flag = scion(
        &no_ep,
        &[
            "--endpoint",
            &h.daemon_endpoint(),
            "log",
            "f.txt",
            "--oneline",
        ],
    )
    .await;
    assert_eq!(flag.code, 0, "{}", flag.stderr);
    assert!(flag.text().contains(" via daemon"), "{}", flag.text());
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_diff_work_offline_when_trees_are_cached() {
    // SPEC 11.5 (amendment W6-1): `status` makes no server call when every needed tree is
    // cached, so it must succeed with an unreachable endpoint after a checkout.
    let h = Harness::start(false).await;
    let env = h.env_as("alice");
    let wc = h.path("wc");
    let wcs = wc.to_str().unwrap();
    let co = scion(&env, &["checkout", "main", wcs]).await;
    assert_eq!(co.code, 0, "{}", co.stderr);
    std::fs::write(wc.join("offline.txt"), b"written offline\n").unwrap();
    let gone = h.path("no-server.sock");
    let mut offline = env.clone();
    offline.0.insert(
        "SCION_ENDPOINT".into(),
        format!("unix://{}", gone.display()),
    );
    let st = scion(&offline, &["-C", wcs, "--json", "status"]).await;
    assert_eq!(st.code, 0, "{}", st.stderr);
    let j = st.json();
    assert_eq!(j["data"]["clean"], false);
    assert_eq!(j["data"]["added"][0], "offline.txt");
    let d = scion(&offline, &["-C", wcs, "--json", "diff"]).await;
    assert_eq!(d.code, 0, "{}", d.stderr);
    assert_eq!(d.json()["data"]["changes"][0]["path"], "offline.txt");
    // A command that needs the server still reports the unreachable endpoint.
    let l = scion(&offline, &["-C", wcs, "log", "--oneline"]).await;
    assert_ne!(l.code, 0, "{}", l.text());
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_help_usage_and_whoami() {
    let h = Harness::start(false).await;
    let token = h.cluster.token_for(&TestCluster::gh("bob"));
    let home = h.home.path().to_path_buf();
    let endpoint = h.endpoint.clone();
    let ca = h.ca_file.clone();
    tokio::task::spawn_blocking(move || {
        let help = Command::cargo_bin("scion")
            .unwrap()
            .arg("--help")
            .output()
            .unwrap();
        assert_eq!(help.status.code(), Some(0));
        let text = String::from_utf8_lossy(&help.stdout);
        assert!(text.contains("Exit codes:"), "{text}");
        assert!(text.contains("3  conflicts"), "{text}");

        let bad = Command::cargo_bin("scion")
            .unwrap()
            .arg("nope")
            .output()
            .unwrap();
        assert_eq!(bad.status.code(), Some(2));

        let who = Command::cargo_bin("scion")
            .unwrap()
            .env_clear()
            .env("HOME", &home)
            .env("SCION_ENDPOINT", &endpoint)
            .env("SCION_CA_FILE", &ca)
            .env("SCION_TOKEN", &token)
            .args(["--json", "whoami"])
            .output()
            .unwrap();
        assert_eq!(
            who.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&who.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&who.stdout).unwrap();
        assert_eq!(v["schema"], "scion.cli.whoami.v1");
        assert_eq!(v["data"]["principal"], "github:bob");
        cli_support::validate(&v);

        let unauth = Command::cargo_bin("scion")
            .unwrap()
            .env_clear()
            .env("HOME", &home)
            .env("SCION_ENDPOINT", &endpoint)
            .env("SCION_CA_FILE", &ca)
            .env("SCION_TOKEN", "v4.public.garbage")
            .arg("whoami")
            .output()
            .unwrap();
        assert_eq!(unauth.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&unauth.stderr).contains("UNAUTHENTICATED"));
    })
    .await
    .unwrap();
}
