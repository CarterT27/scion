//! SPEC 19.6 / 19.13 on the named edge-case histories of `scion_testkit::edge_cases()`
//! (file/dir flips, delete-and-readd, CRLF, NUL content, exec flips, symlink retargets,
//! unterminated last lines, empty files, a merge keeping the first parent's version, ...):
//! `log::log_path` (both modes), `blame::blame` and `versions::all_versions` equal the
//! reference model for every path at every commit (P24).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod history_support;

use history_support::*;
use scion_core::{ErrorCode, ObjectId};
use scion_derived::blame::{self, BlameRange};
use scion_derived::log::log_path;
use scion_derived::versions::all_versions;
use scion_object::RepoPath;
use scion_testkit::edge_cases;
use std::collections::{BTreeSet, HashMap};

#[tokio::test(flavor = "multi_thread")]
async fn log_blame_and_versions_match_refmodel_on_edge_cases() {
    let mut cases = 0;
    let mut flips = 0;
    for (name, repo) in edge_cases() {
        let s = stack();
        let ids = s.load(&repo).await;
        let model = model_of(&repo);
        let mut gens: HashMap<ObjectId, u64> = HashMap::new();
        for id in &ids {
            gens.insert(*id, s.h.changeset(id).await.unwrap().generation);
        }
        let mut paths: BTreeSet<Vec<u8>> = BTreeSet::new();
        for c in &repo.commits {
            paths.extend(all_paths(&c.tree));
        }
        // `deep_nesting_4096` has about 2000 directory levels: check every file and a sample
        // of the directories, so the case stays fast (every query is O(depth)).
        if paths.len() > 64 {
            let files: BTreeSet<Vec<u8>> = repo
                .commits
                .iter()
                .flat_map(|c| c.tree.keys().cloned())
                .collect();
            paths = paths
                .into_iter()
                .enumerate()
                .filter(|(i, path)| files.contains(path) || i % 256 == 0)
                .map(|(_, path)| path)
                .collect();
        }
        for c in 0..repo.commits.len() {
            for path in &paths {
                let Ok(rp) = RepoPath::parse(path) else {
                    continue;
                };
                let ctx = format!(
                    "{name}: commit {c}, path {:?}",
                    String::from_utf8_lossy(path)
                );
                for fp in [false, true] {
                    let got = log_path(&s.engine, &ids[c], &rp, fp, usize::MAX, None)
                        .await
                        .unwrap();
                    assert!(got.next_page.is_none(), "{ctx}");
                    let mut want: Vec<ObjectId> = model
                        .log_path(c, path, fp)
                        .into_iter()
                        .map(|i| ids[i])
                        .collect();
                    want.sort_by(|a, b| gens[b].cmp(&gens[a]).then(b.cmp(a)));
                    assert_eq!(got.changesets, want, "{ctx}, first_parent_only {fp}");
                }
                let got = all_versions(&s.engine, &ids[c], &rp).await.unwrap();
                let want: Vec<ObjectId> = model
                    .all_versions(c, path)
                    .iter()
                    .map(|content| ObjectId::hash_blob(content))
                    .collect();
                assert_eq!(got, want, "{ctx}: all_versions");
                let is_file = repo.commits[c].tree.contains_key(path);
                let got = blame::blame(&s.engine, &ids[c], &rp, None).await;
                if is_file {
                    let want: Vec<BlameRange> = model
                        .blame(c, path)
                        .into_iter()
                        .map(|(start, count, origin, ostart)| BlameRange {
                            start_line: start,
                            line_count: count,
                            origin_changeset: ids[origin],
                            origin_start_line: ostart,
                        })
                        .collect();
                    assert_eq!(got.unwrap(), want, "{ctx}: blame");
                } else {
                    assert!(is_code(&got, ErrorCode::NotFound), "{ctx}: blame {got:?}");
                }
            }
        }
        if name == "file_dir_flip" {
            flips += 1;
        }
        cases += 1;
    }
    assert!(cases >= 10, "edge cases exercised: {cases}");
    assert_eq!(flips, 1, "the file/dir flip case is covered");
}
