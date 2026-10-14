//! SPEC 19.10 "`--json` output validates against the published schema for every command"
//! (P31): the committed `schemas/v1/*.schema.json` equal the generated ones, and an end-to-end
//! session against a [`scion_conformance::TestCluster`] produces, for every command, output
//! that validates against its published schema (jsonschema crate).
//!
//! Regenerate the committed schemas with `SCION_REGEN_SCHEMAS=1 cargo test -p scion-cli
//! --test cli_json committed_schemas`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod cli_support;

use cli_support::{Harness, schemas_dir, scripted_session, validate};
use scion_cli::output::{
    ERROR_SCHEMA, command_names, schema_file_name, schema_id, schema_text, write_schemas,
};
use std::collections::BTreeSet;

#[test]
fn committed_schemas_match_generated() {
    let dir = schemas_dir();
    if std::env::var("SCION_REGEN_SCHEMAS").is_ok_and(|v| v == "1") {
        write_schemas(&dir).unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    let written = write_schemas(tmp.path()).unwrap();
    let mut expected: BTreeSet<String> = command_names()
        .iter()
        .chain([&ERROR_SCHEMA])
        .map(|c| schema_file_name(c))
        .collect();
    assert_eq!(written.len(), expected.len());
    let committed: BTreeSet<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        committed, expected,
        "schemas/v1 must contain exactly one schema per command plus error.schema.json"
    );
    for cmd in command_names().iter().chain([&ERROR_SCHEMA]) {
        let name = schema_file_name(cmd);
        let generated = std::fs::read_to_string(tmp.path().join(&name)).unwrap();
        assert_eq!(generated, schema_text(cmd).unwrap());
        let on_disk = std::fs::read_to_string(dir.join(&name)).unwrap();
        assert!(
            on_disk == generated,
            "schemas/v1/{name} is stale; regenerate with SCION_REGEN_SCHEMAS=1 cargo test -p scion-cli --test cli_json committed_schemas"
        );
        let v: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(v["properties"]["schema"]["const"], schema_id(cmd));
        expected.remove(&name);
    }
    assert!(expected.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn every_command_json_validates_against_its_published_schema() {
    let h = Harness::start(false).await;
    let steps = scripted_session(&h, true).await;
    let mut covered = BTreeSet::new();
    for s in &steps {
        let v = s.out.json();
        let cmd = validate(&v);
        assert_eq!(cmd, s.cmd, "step {}: envelope {}", s.label, v["schema"]);
        if cmd == "error" {
            assert_eq!(v["data"]["exit_code"], s.out.code, "step {}", s.label);
        }
        // Full ids in --json.
        for key in ["base", "changeset", "head", "target", "id"] {
            if let Some(id) = v["data"][key].as_str()
                && id.starts_with("c1")
            {
                assert_eq!(
                    id.len(),
                    66,
                    "step {}: {key} is not a full id: {id}",
                    s.label
                );
            }
        }
        covered.insert(cmd);
    }
    for cmd in command_names() {
        assert!(covered.contains(*cmd), "no --json run of {cmd}");
    }
    assert!(covered.contains("error"));

    let by = cli_support::by_label(&steps);
    let conflict = by["rebase-conflict"].out.json();
    assert_eq!(conflict["data"]["conflicts"], serde_json::json!(["a.txt"]));
    let dirty = by["goto-dirty"].out.json();
    assert_eq!(dirty["data"]["subcode"], "dirty_working_copy");
    let markers = by["resolve-markers"].out.json();
    assert_eq!(markers["data"]["subcode"], "conflict_markers_remain");
    let no_msg = by["commit-no-message"].out.json();
    assert_eq!(no_msg["data"]["code"], "USAGE");
    assert_eq!(no_msg["data"]["exit_code"], 2);
    let missing = by["cat-missing"].out.json();
    assert_eq!(missing["data"]["code"], "NOT_FOUND");
    let cat = by["cat"].out.json();
    assert_eq!(cat["data"]["content"], "one\ntwo\n");
    assert_eq!(cat["data"]["encoding"], "utf8");
    let login = by["login"].out.json();
    assert_eq!(login["data"]["principal"], "service:ci");
    let whoami = by["whoami"].out.json();
    assert_eq!(whoami["data"]["principal"], "github:alice");
}
