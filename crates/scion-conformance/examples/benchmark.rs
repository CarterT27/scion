//! Synthetic Git/Scion comparison. Run in release mode; setup and verification are untimed.
use anyhow::{Context, Result, ensure};
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use scion_conformance::{Backend, ClusterOpts, TestCluster, Via};
use scion_core::ObjectId;
use scion_object::RepoPath;
use scion_proto::{ScionClient, v1};
use scion_wc::{WcOptions, WorkingCopy};
use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .arg("-c")
        .arg("user.name=Benchmark")
        .arg("-c")
        .arg("user.email=benchmark@example.com")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    ensure!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

fn fixture(files_per_dir: usize) -> Vec<(String, Vec<u8>)> {
    let mut rng = ChaCha8Rng::seed_from_u64(1);
    (0..10)
        .flat_map(|d| (0..files_per_dir).map(move |f| (d, f)))
        .map(|(d, f)| {
            let mut content = vec![0; 4096];
            rng.fill_bytes(&mut content);
            (format!("dir{d:04}/file{f:05}.bin"), content)
        })
        .collect()
}

fn write_fixture(root: &Path, files: &[(String, Vec<u8>)]) -> Result<()> {
    for (path, bytes) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().context("file parent")?)?;
        fs::write(path, bytes)?;
    }
    Ok(())
}

fn verify(root: &Path, files: &[(String, Vec<u8>)], sparse: bool) -> Result<()> {
    for (path, bytes) in files {
        if sparse && !path.starts_with("dir0000/") {
            ensure!(!root.join(path).exists(), "out-of-scope file: {path}");
        } else {
            ensure!(
                fs::read(root.join(path))? == *bytes,
                "content mismatch: {path}"
            );
        }
    }
    Ok(())
}

// Directory ingestion through the production write RPCs; this is not the Git-history importer.
async fn import(
    client: &ScionClient,
    root: &Path,
    files: &[(String, Vec<u8>)],
) -> Result<ObjectId> {
    let contents: Vec<Vec<u8>> = files
        .iter()
        .map(|(path, _)| fs::read(root.join(path)))
        .collect::<std::io::Result<_>>()?;
    for batch in contents.chunks(256) {
        let blobs: Vec<&[u8]> = batch.iter().map(Vec::as_slice).collect();
        TestCluster::upload(client, &blobs).await;
    }
    let puts: Vec<(&str, &[u8])> = files
        .iter()
        .zip(&contents)
        .map(|((path, _), content)| (path.as_str(), content.as_slice()))
        .collect();
    let mut request = TestCluster::create_request(&ObjectId::hash_blob(b""), &puts, &[], 1);
    request.parents.clear();
    request.import_mode = true;
    let response = client.write().create_changeset(request).await?.into_inner();
    client
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: "benchmark".into(),
            target: response.changeset_id.clone(),
            reason: "benchmark import".into(),
        })
        .await?;
    Ok(ObjectId::from_bytes(&response.changeset_id)?)
}

fn median(samples: &mut [Duration]) -> f64 {
    samples.sort_unstable();
    let middle = samples.len() / 2;
    let seconds = if samples.len().is_multiple_of(2) {
        (samples[middle - 1].as_secs_f64() + samples[middle].as_secs_f64()) / 2.0
    } else {
        samples[middle].as_secs_f64()
    };
    seconds * 1000.0
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ensure!(
        args.len() <= 2,
        "usage: benchmark [files-per-directory=100] [runs=5]"
    );
    let files_per_dir = args.first().map(|v| v.parse()).transpose()?.unwrap_or(100);
    let runs = args.get(1).map(|v| v.parse()).transpose()?.unwrap_or(5);
    ensure!(
        files_per_dir > 0 && runs > 0,
        "sizes and runs must be positive"
    );
    let files = fixture(files_per_dir);
    let mut samples: Vec<Vec<Duration>> = vec![Vec::new(); 8];
    for run in 0..runs {
        let scratch = tempfile::tempdir()?;
        let root = scratch.path();
        let source = root.join("source");
        write_fixture(&source, &files)?;
        let cluster =
            TestCluster::start(ClusterOpts::new(Backend::Memory).with_daemon(false)).await;
        let client = cluster.client(Via::Server, &TestCluster::seeder()).await;

        let start = Instant::now();
        git(&source, &["init", "-q", "-b", "main"])?;
        git(&source, &["add", "-A"])?;
        git(&source, &["commit", "-q", "-m", "initial"])?;
        samples[0].push(start.elapsed());
        // Enable actual partial-clone filtering over the local Git transport.
        git(&source, &["config", "uploadpack.allowFilter", "true"])?;
        let start = Instant::now();
        let base = import(&client, &source, &files).await?;
        samples[1].push(start.elapsed());

        let start = Instant::now();
        git(root, &["clone", "-q", "--no-local", "source", "git-full"])?;
        samples[2].push(start.elapsed());
        let start = Instant::now();
        let mut wc = WorkingCopy::checkout(
            client.clone(),
            &root.join("scion-full"),
            &base.to_text(),
            Vec::new(),
            WcOptions::default(),
        )
        .await?;
        samples[3].push(start.elapsed());
        verify(&root.join("git-full"), &files, false)?;
        verify(&root.join("scion-full"), &files, false)?;

        let start = Instant::now();
        git(
            root,
            &[
                "clone",
                "-q",
                "--no-local",
                "--filter=blob:none",
                "--sparse",
                "source",
                "git-sparse",
            ],
        )?;
        git(
            &root.join("git-sparse"),
            &["sparse-checkout", "set", "dir0000"],
        )?;
        samples[4].push(start.elapsed());
        let start = Instant::now();
        let sparse = WorkingCopy::checkout(
            client.clone(),
            &root.join("scion-sparse"),
            &base.to_text(),
            vec![RepoPath::parse(b"dir0000")?],
            WcOptions::default(),
        )
        .await?;
        samples[5].push(start.elapsed());
        verify(&root.join("git-sparse"), &files, true)?;
        verify(&root.join("scion-sparse"), &files, true)?;
        ensure!(
            sparse.fetched_objects().len() == files_per_dir + 2,
            "unexpected sparse object count"
        );

        let mut changed = files[0].1.clone();
        changed[0] ^= 1;
        fs::write(root.join("git-full").join(&files[0].0), &changed)?;
        fs::write(root.join("scion-full").join(&files[0].0), &changed)?;
        let start = Instant::now();
        git(
            &root.join("git-full"),
            &["commit", "-q", "-a", "-m", "one file"],
        )?;
        samples[6].push(start.elapsed());
        let start = Instant::now();
        let report = wc
            .commit_with_report(b"one file", b"Benchmark <benchmark@example.com>", None)
            .await?;
        samples[7].push(start.elapsed());
        ensure!(
            report.delta.len() == 1,
            "commit must change exactly one file"
        );
        ensure!(
            wc.status().await?.is_clean(),
            "Scion commit left dirty files"
        );
        eprintln!(
            "run {}: {:?} ms (git/scion: import, full, sparse, commit)",
            run + 1,
            samples
                .iter()
                .map(|v| v[run].as_secs_f64() * 1000.0)
                .collect::<Vec<_>>()
        );
    }
    println!(
        "{} files, 4096 bytes/file, {runs} runs; medians:",
        files.len()
    );
    println!("| Scenario | Git (ms) | Scion (ms) |\n| --- | ---: | ---: |");
    for (name, index) in [
        ("import", 0),
        ("commit-one-file", 6),
        ("full-clone", 2),
        ("sparse-clone", 4),
    ] {
        println!(
            "| {name} | {:.1} | {:.1} |",
            median(&mut samples[index]),
            median(&mut samples[index + 1])
        );
    }
    Ok(())
}
