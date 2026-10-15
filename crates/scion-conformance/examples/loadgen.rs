//! Load generator for a running Scion server: admin ref setup, one-shot history queries, and an
//! open-loop landing soak (SPEC 19.19). Run in release mode against a disposable server.
//!
//! ```text
//! loadgen [--endpoint URL] [--ca-file PEM] [--token-file FILE] <command>
//!   ref-reset <name> <changeset>        delete <name> if present, then create it at <changeset>
//!   ref-delete <name>
//!   query blame <changeset> <path>
//!   query diff <a> <b>
//!   query merge-base <a> <b>
//!   soak --ref <name> --base <changeset> --rate <landings/s> --duration <s> [--files N]
//!        [--dir PATH] [--max-in-flight N] [--window S] [--max-attempts N] [--seed N]
//!        [--arrivals poisson|uniform] [--metrics-url URL] [--output FILE]
//! ```
//!
//! Changesets are hex ids. The endpoint, CA and token fall back to `SCION_ENDPOINT`,
//! `SCION_CA_FILE` and `SCION_TOKEN`. Every command prints one JSON object on stdout.
//!
//! The soak resets `--ref` to `--base`, lands one changeset adding `--files` files under
//! `--dir`, then schedules landings at `--rate` for `--duration` seconds whether or not earlier
//! ones finished (open loop, so a slow server shows up as latency and backlog instead of a
//! lower request rate). Each landing reads the ref tip, uploads one blob, creates a one-file
//! changeset replacing a random seeded file and lands it, retrying on MERGE_CONFLICT as an
//! agent would. Latency is measured from the scheduled arrival. Afterwards it checks that the
//! ref advanced by exactly the number of successful landings, that every landing got a distinct
//! version, and that every landed result is on the ref's first-parent history.

use anyhow::{Context, Result, bail, ensure};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use scion_core::{ErrorCode, Kind, ObjectId};
use scion_proto::convert::DELTA_OP_PUT;
use scion_proto::{Endpoint, ScionClient, TlsOptions, error_from_status, v1};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;

const USAGE: &str = "usage: loadgen [--endpoint URL] [--ca-file PEM] [--token-file FILE] \
                     <ref-reset|ref-delete|query|soak> ... (see the module docs)";

#[tokio::main]
async fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let endpoint = take_opt(&mut args, "--endpoint")?
        .or_else(|| std::env::var("SCION_ENDPOINT").ok())
        .context("--endpoint or SCION_ENDPOINT is required")?;
    let ca = take_opt(&mut args, "--ca-file")?.or_else(|| std::env::var("SCION_CA_FILE").ok());
    let token = match take_opt(&mut args, "--token-file")? {
        Some(f) => Some(std::fs::read_to_string(&f).with_context(|| format!("read {f}"))?),
        None => std::env::var("SCION_TOKEN").ok(),
    }
    .map(|t| t.trim().to_owned());
    let tls = match ca {
        Some(f) => Some(TlsOptions {
            ca_pem: Some(std::fs::read(&f).with_context(|| format!("read {f}"))?),
            domain: None,
        }),
        None => None,
    };
    let client = ScionClient::connect(&Endpoint::parse(&endpoint)?, tls, token).await?;
    let out = match args.first().map(String::as_str) {
        Some("ref-reset") if args.len() == 3 => {
            ref_reset(&client, &args[1], parse_id(&args[2])?).await?
        }
        Some("ref-delete") if args.len() == 2 => ref_delete(&client, &args[1]).await?,
        Some("query") if args.len() >= 2 => query(&client, &args[1..]).await?,
        Some("soak") => soak(client, SoakArgs::parse(&mut args[1..].to_vec())?).await?,
        _ => bail!(USAGE),
    };
    println!("{out}");
    Ok(())
}

/// Remove `--name value` from `args`, returning the value.
fn take_opt(args: &mut Vec<String>, name: &str) -> Result<Option<String>> {
    let Some(i) = args.iter().position(|a| a == name) else {
        return Ok(None);
    };
    ensure!(i + 1 < args.len(), "{name} needs a value");
    let v = args.remove(i + 1);
    args.remove(i);
    Ok(Some(v))
}

fn parse_id(hex: &str) -> Result<ObjectId> {
    hex.parse::<ObjectId>()
        .with_context(|| format!("not a changeset id: {hex}"))
}

fn id_of(bytes: &[u8]) -> Result<ObjectId> {
    Ok(ObjectId::from_bytes(bytes)?)
}

async fn resolve(client: &ScionClient, name: &str) -> Result<Option<(ObjectId, u64)>> {
    match client
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: name.into(),
            at_version: None,
        })
        .await
    {
        Ok(r) => {
            let r = r.into_inner();
            Ok(Some((id_of(&r.target)?, r.version)))
        }
        Err(s) if error_from_status(&s).code == ErrorCode::NotFound => Ok(None),
        Err(s) => Err(error_from_status(&s).into()),
    }
}

async fn ref_delete(client: &ScionClient, name: &str) -> Result<Value> {
    let Some((_, version)) = resolve(client, name).await? else {
        return Ok(json!({"name": name, "deleted": false}));
    };
    client
        .refs()
        .delete_ref(v1::DeleteRefRequest {
            name: name.into(),
            expected_version: version,
            reason: "loadgen".into(),
        })
        .await
        .map_err(|s| error_from_status(&s))?;
    Ok(json!({"name": name, "deleted": true}))
}

async fn ref_reset(client: &ScionClient, name: &str, target: ObjectId) -> Result<Value> {
    ref_delete(client, name).await?;
    client
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: name.into(),
            target: target.as_bytes().to_vec(),
            reason: "loadgen".into(),
        })
        .await
        .map_err(|s| error_from_status(&s))?;
    let (target, version) = resolve(client, name).await?.context("ref vanished")?;
    Ok(json!({"name": name, "target": target.to_string(), "version": version}))
}

async fn query(client: &ScionClient, args: &[String]) -> Result<Value> {
    let mut q = client.query();
    match args {
        [kind, rev, path] if kind == "blame" => {
            let r = q
                .blame(v1::BlameRequest {
                    changeset: parse_id(rev)?.as_bytes().to_vec(),
                    path: path.as_bytes().to_vec(),
                })
                .await
                .map_err(|s| error_from_status(&s))?
                .into_inner();
            let mut origins = HashSet::new();
            let mut lines = 0u64;
            for range in &r.ranges {
                origins.insert(id_of(&range.origin_changeset)?.to_string());
                lines += u64::from(range.line_count);
            }
            Ok(json!({"ranges": r.ranges.len(), "lines": lines, "origins": origins}))
        }
        [kind, a, b] if kind == "diff" => {
            let mut s = q
                .diff(v1::DiffRequest {
                    a: parse_id(a)?.as_bytes().to_vec(),
                    b: parse_id(b)?.as_bytes().to_vec(),
                    paths: Vec::new(),
                    include_content_hunks: true,
                })
                .await
                .map_err(|s| error_from_status(&s))?
                .into_inner();
            let (mut paths, mut hunks) = (Vec::new(), 0usize);
            while let Some(item) = s.message().await.map_err(|s| error_from_status(&s))? {
                hunks += item.hunks.len();
                paths.push(String::from_utf8_lossy(&item.path).into_owned());
            }
            Ok(json!({"paths": paths, "hunks": hunks}))
        }
        [kind, a, b] if kind == "merge-base" => {
            let r = q
                .merge_base(v1::MergeBaseRequest {
                    a: parse_id(a)?.as_bytes().to_vec(),
                    b: parse_id(b)?.as_bytes().to_vec(),
                })
                .await
                .map_err(|s| error_from_status(&s))?
                .into_inner();
            let bases: Result<Vec<String>> =
                r.bases.iter().map(|b| Ok(id_of(b)?.to_string())).collect();
            Ok(json!({"bases": bases?}))
        }
        _ => bail!("usage: query blame <rev> <path> | diff <a> <b> | merge-base <a> <b>"),
    }
}

struct SoakArgs {
    reference: String,
    base: ObjectId,
    rate: f64,
    duration: Duration,
    files: usize,
    dir: String,
    max_in_flight: usize,
    window: f64,
    max_attempts: u32,
    seed: u64,
    poisson: bool,
    metrics_url: Option<String>,
    output: Option<String>,
}

impl SoakArgs {
    fn parse(args: &mut Vec<String>) -> Result<Self> {
        let req = |a: &mut Vec<String>, n: &str| -> Result<String> {
            take_opt(a, n)?.with_context(|| format!("soak needs {n}"))
        };
        let num = |a: &mut Vec<String>, n: &str, d: f64| -> Result<f64> {
            Ok(take_opt(a, n)?.map(|v| v.parse()).transpose()?.unwrap_or(d))
        };
        let s = SoakArgs {
            reference: req(args, "--ref")?,
            base: parse_id(&req(args, "--base")?)?,
            rate: req(args, "--rate")?.parse()?,
            duration: Duration::from_secs_f64(req(args, "--duration")?.parse()?),
            files: num(args, "--files", 1000.0)? as usize,
            dir: take_opt(args, "--dir")?.unwrap_or_else(|| "loadgen-soak".into()),
            max_in_flight: num(args, "--max-in-flight", 512.0)? as usize,
            window: num(args, "--window", 60.0)?,
            max_attempts: num(args, "--max-attempts", 5.0)? as u32,
            seed: num(args, "--seed", 1.0)? as u64,
            poisson: match take_opt(args, "--arrivals")?.as_deref() {
                None | Some("poisson") => true,
                Some("uniform") => false,
                Some(o) => bail!("unknown --arrivals {o}"),
            },
            metrics_url: take_opt(args, "--metrics-url")?,
            output: take_opt(args, "--output")?,
        };
        ensure!(args.is_empty(), "unknown soak arguments: {args:?}");
        ensure!(s.rate > 0.0 && s.files > 0 && s.max_in_flight > 0 && s.window > 0.0);
        ensure!(s.max_attempts > 0);
        Ok(s)
    }
}

/// One scheduled landing's outcome.
struct Sample {
    scheduled: f64,
    latency: f64,
    land_rpc: f64,
    attempts: u32,
    conflicts: u32,
    result: std::result::Result<(ObjectId, u64), String>,
}

fn meta(msg: String) -> v1::MetaFields {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    v1::MetaFields {
        author: b"Loadgen <loadgen@example.com>".to_vec(),
        author_time: Some(v1::Timestamp {
            seconds: now.as_secs() as i64,
            tz_offset_minutes: 0,
        }),
        committer: None,
        committer_time: None,
        message: msg.into_bytes(),
        extensions: Vec::new(),
    }
}

/// Upload `files` (path, content) and create a changeset putting them on `parent`.
async fn create(
    client: &ScionClient,
    parent: ObjectId,
    files: &[(String, Vec<u8>)],
    msg: String,
) -> Result<ObjectId> {
    let items: Vec<v1::UploadItem> = files
        .iter()
        .map(|(_, c)| v1::UploadItem {
            id: ObjectId::hash_blob(c).as_bytes().to_vec(),
            kind: u32::from(Kind::Blob.byte()),
            data: c.clone(),
        })
        .collect();
    let results = client
        .write()
        .upload_objects(futures::stream::iter(items))
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner()
        .results;
    ensure!(
        results.len() == files.len()
            && results
                .iter()
                .all(|r| r.status == v1::ItemStatus::Ok as i32),
        "upload failed: {results:?}"
    );
    let mut delta: Vec<v1::DeltaEntryMsg> = files
        .iter()
        .map(|(p, c)| v1::DeltaEntryMsg {
            path: p.as_bytes().to_vec(),
            op: DELTA_OP_PUT,
            mode: 1,
            id: ObjectId::hash_blob(c).as_bytes().to_vec(),
            size: c.len() as u64,
        })
        .collect();
    delta.sort_by(|a, b| a.path.cmp(&b.path));
    let r = client
        .write()
        .create_changeset(v1::CreateChangesetRequest {
            parents: vec![parent.as_bytes().to_vec()],
            delta,
            meta_fields: Some(meta(msg)),
            change_id: rand::random::<[u8; 16]>().to_vec(),
            expected_root_tree: None,
            import_mode: false,
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    id_of(&r.changeset_id)
}

async fn land(client: &ScionClient, reference: &str, head: ObjectId) -> Result<v1::LandResponse> {
    Ok(client
        .write()
        .land(v1::LandRequest {
            r#ref: reference.into(),
            head: head.as_bytes().to_vec(),
            allow_empty: false,
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner())
}

/// One agent-style landing: read the tip, change one file, create, land; retry conflicts.
async fn one_landing(
    client: &ScionClient,
    a: &SoakArgs,
    n: u64,
    file: usize,
    scheduled: f64,
    start: Instant,
) -> Sample {
    let (mut attempts, mut conflicts, mut land_rpc) = (0, 0, 0.0);
    let result = loop {
        attempts += 1;
        let step = async {
            let (tip, _) = resolve(client, &a.reference)
                .await?
                .context("ref missing")?;
            let path = format!("{}/f{file:05}.txt", a.dir);
            let body = format!(
                "landing {n} attempt {attempts} {:x}\n",
                rand::random::<u64>()
            );
            let head = create(
                client,
                tip,
                &[(path, body.into_bytes())],
                format!("soak {n}"),
            )
            .await?;
            let t = Instant::now();
            let r = land(client, &a.reference, head).await;
            land_rpc = t.elapsed().as_secs_f64();
            r
        };
        match step.await {
            Ok(r) => match id_of(&r.target) {
                Ok(target) => break Ok((target, r.version)),
                Err(e) => break Err(format!("{e:#}")),
            },
            Err(e) => {
                let code = e.downcast_ref::<scion_core::ScionError>().map(|e| e.code);
                if code == Some(ErrorCode::MergeConflict) && attempts < a.max_attempts {
                    conflicts += 1;
                    continue;
                }
                break Err(match code {
                    Some(c) => c.to_string(),
                    None => format!("{e:#}"),
                });
            }
        }
    };
    Sample {
        scheduled,
        latency: start.elapsed().as_secs_f64() - scheduled,
        land_rpc,
        attempts,
        conflicts,
        result,
    }
}

fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    Some(sorted[i])
}

fn latency_summary(mut v: Vec<f64>) -> Value {
    v.sort_by(f64::total_cmp);
    json!({
        "p50": percentile(&v, 0.50),
        "p95": percentile(&v, 0.95),
        "p99": percentile(&v, 0.99),
        "max": v.last(),
    })
}

/// Selected counters from the server's Prometheus endpoint, keyed by series.
async fn land_metrics(url: &Option<String>) -> Result<BTreeMap<String, f64>> {
    let Some(url) = url else {
        return Ok(BTreeMap::new());
    };
    let text = reqwest::get(url).await?.text().await?;
    Ok(text
        .lines()
        .filter(|l| {
            l.starts_with("scion_land_attempts")
                || l.starts_with("scion_ref_cas_failures_total")
                || l.starts_with("scion_land_duration_seconds_sum")
                || l.starts_with("scion_land_duration_seconds_count")
        })
        .filter_map(|l| {
            let (k, v) = l.rsplit_once(' ')?;
            Some((k.to_owned(), v.parse().ok()?))
        })
        .collect())
}

async fn soak(client: ScionClient, a: SoakArgs) -> Result<Value> {
    // Setup (untimed): a fresh ref with the files the landings will rewrite.
    ref_reset(&client, &a.reference, a.base).await?;
    let seed_files: Vec<(String, Vec<u8>)> = (0..a.files)
        .map(|i| {
            (
                format!("{}/f{i:05}.txt", a.dir),
                format!("seed {i}\n").into_bytes(),
            )
        })
        .collect();
    let seeded = create(&client, a.base, &seed_files, "loadgen soak seed".into()).await?;
    let seeded = land(&client, &a.reference, seeded).await?;
    let v0 = seeded.version;
    let before = land_metrics(&a.metrics_url).await?;

    let a = Arc::new(a);
    let client = Arc::new(client);
    let slots = Arc::new(Semaphore::new(a.max_in_flight));
    let mut rng = ChaCha8Rng::seed_from_u64(a.seed);
    let mut tasks = tokio::task::JoinSet::new();
    let (mut scheduled, mut shed) = (0u64, 0u64);
    let start = Instant::now();
    let mut next = 0.0f64;
    while next < a.duration.as_secs_f64() {
        tokio::time::sleep_until((start + Duration::from_secs_f64(next)).into()).await;
        let n = scheduled;
        scheduled += 1;
        let file = rng.random_range(0..a.files);
        match slots.clone().try_acquire_owned() {
            Ok(permit) => {
                let (client, a) = (client.clone(), a.clone());
                let at = next;
                tasks.spawn(async move {
                    let s = one_landing(&client, &a, n, file, at, start).await;
                    drop(permit);
                    s
                });
            }
            // The client backlog is full: the server is not keeping up with the arrival rate.
            Err(_) => shed += 1,
        }
        next += if a.poisson {
            -(1.0 - rng.random::<f64>()).ln() / a.rate
        } else {
            1.0 / a.rate
        };
    }
    let arrivals_done = start.elapsed().as_secs_f64();
    let mut samples = Vec::new();
    while let Some(s) = tasks.join_next().await {
        samples.push(s?);
    }
    let drained = start.elapsed().as_secs_f64();
    let after = land_metrics(&a.metrics_url).await?;

    // Validation: every success is on the ref, at a distinct version, with nothing lost.
    let ok: Vec<&Sample> = samples.iter().filter(|s| s.result.is_ok()).collect();
    let (tip, version) = resolve(&client, &a.reference)
        .await?
        .context("ref missing")?;
    let mut versions: Vec<u64> = ok
        .iter()
        .map(|s| s.result.as_ref().map_or(0, |r| r.1))
        .collect();
    versions.sort_unstable();
    let distinct = versions.windows(2).all(|w| w[0] != w[1]);
    let contiguous = versions.iter().copied().eq(v0 + 1..=v0 + ok.len() as u64);
    let mut history = HashSet::new();
    let mut page = Vec::new();
    while history.len() < ok.len() + 1 {
        let r = client
            .query()
            .log(v1::LogRequest {
                from: tip.as_bytes().to_vec(),
                path: None,
                first_parent_only: true,
                limit: 1000,
                page_token: page,
            })
            .await
            .map_err(|s| error_from_status(&s))?
            .into_inner();
        for e in &r.entries {
            history.insert(id_of(&e.id)?);
        }
        if r.next_page_token.is_empty() || r.entries.is_empty() {
            break;
        }
        page = r.next_page_token;
    }
    let lost = ok
        .iter()
        .filter(|s| s.result.as_ref().is_ok_and(|r| !history.contains(&r.0)))
        .count();

    let mut errors: BTreeMap<String, u64> = BTreeMap::new();
    for s in &samples {
        if let Err(e) = &s.result {
            *errors.entry(e.clone()).or_default() += 1;
        }
    }
    let windows: Vec<Value> = (0..)
        .map(|w| w as f64 * a.window)
        .take_while(|t| *t < a.duration.as_secs_f64())
        .map(|t0| {
            let inw: Vec<&Sample> = samples
                .iter()
                .filter(|s| s.scheduled >= t0 && s.scheduled < t0 + a.window)
                .collect();
            let lat: Vec<f64> = inw
                .iter()
                .filter(|s| s.result.is_ok())
                .map(|s| s.latency)
                .collect();
            json!({
                "start_s": t0,
                "scheduled": inw.len(),
                "landed": lat.len(),
                "conflict_retries": inw.iter().map(|s| u64::from(s.conflicts)).sum::<u64>(),
                "errors": inw.iter().filter(|s| s.result.is_err()).count(),
                "latency_s": latency_summary(lat),
            })
        })
        .collect();
    let delta: BTreeMap<&String, f64> = after
        .iter()
        .map(|(k, v)| (k, v - before.get(k).copied().unwrap_or(0.0)))
        .collect();
    let target_per_day = a.rate * 86_400.0;
    let achieved = ok.len() as f64 / drained;
    let out = json!({
        "ref": a.reference,
        "target_rate_per_s": a.rate,
        "target_per_day": target_per_day,
        "duration_s": a.duration.as_secs_f64(),
        "arrivals": if a.poisson { "poisson" } else { "uniform" },
        "files": a.files,
        "max_in_flight": a.max_in_flight,
        "scheduled": scheduled,
        "shed_client_backlog_full": shed,
        "landed": ok.len(),
        "errors": errors,
        "conflict_retries": samples.iter().map(|s| u64::from(s.conflicts)).sum::<u64>(),
        "attempts": samples.iter().map(|s| u64::from(s.attempts)).sum::<u64>(),
        "arrivals_done_s": arrivals_done,
        "drained_s": drained,
        "achieved_rate_per_s": achieved,
        "achieved_per_day": achieved * 86_400.0,
        "latency_s": latency_summary(ok.iter().map(|s| s.latency).collect()),
        "land_rpc_s": latency_summary(ok.iter().map(|s| s.land_rpc).collect()),
        "server_metric_deltas": delta,
        "windows": windows,
        "validation": {
            "ref_version_delta": version - v0,
            "versions_distinct": distinct,
            "versions_contiguous": contiguous,
            "lost_landings": lost,
            "passed": distinct && contiguous && lost == 0 && version - v0 == ok.len() as u64,
        },
    });
    if let Some(f) = &a.output {
        std::fs::write(f, serde_json::to_string_pretty(&out)? + "\n")?;
    }
    // stdout: the summary without the per-window detail.
    let mut summary = out.clone();
    if let Some(m) = summary.as_object_mut() {
        m.remove("windows");
    }
    Ok(summary)
}
