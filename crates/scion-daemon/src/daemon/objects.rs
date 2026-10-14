//! The daemon's local ObjectService (SPEC 7.3 daemon semantics, 7.8, 10.2-10.4, 13.3).

use super::fetch::{Batch, FetchItem, ItemResult, STATUS_OK};
use super::state::{Caller, Inner};
use bytes::Bytes;
use scion_core::{ObjectId, Result, ScionError};
use scion_object::RefName;
use scion_proto::{status_from_error, v1};
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

/// Data bytes per daemon-served GetObjects chunk.
const CHUNK_BYTES: usize = 1 << 20;
/// Buffered chunks between the serving task and the response stream.
const STREAM_BUFFER: usize = 16;

type ChunkStream = Pin<Box<dyn Stream<Item = std::result::Result<v1::ObjectChunk, Status>> + Send>>;

/// The local ObjectService implementation.
#[derive(Clone)]
pub(crate) struct LocalObjects(pub Arc<Inner>);

/// How one requested id is answered.
enum Plan {
    /// Locally known redacted (SPEC 13.3).
    Redacted,
    /// In the cache.
    Cached,
    /// Not cached: the batch fetching it.
    Miss(Arc<Batch>),
}

/// What gets emitted for one item.
enum Out {
    /// A status-only item (not_found, denied, redacted) as upstream answered it.
    Status {
        status: i32,
        access_class: i32,
        total_size: u64,
    },
    /// Content to serve.
    Ok {
        content: Bytes,
        access_class: i32,
        hit: bool,
    },
}

fn status_only(status: v1::ItemStatus) -> Out {
    Out::Status {
        status: status as i32,
        access_class: 0,
        total_size: 0,
    }
}

fn request_id<T>(req: &Request<T>) -> String {
    req.metadata()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

impl LocalObjects {
    async fn auth<T>(&self, req: &Request<T>) -> std::result::Result<Caller, Status> {
        let h = req.metadata().clone().into_headers();
        self.0
            .authenticate(&h)
            .await
            .map_err(|e| status_from_error(&e))
    }
}

impl Inner {
    /// SPEC 13.3: before any cache serving after startup, the local redaction set must have
    /// been refreshed once. Returns whether it has.
    async fn ensure_synced(self: &Arc<Self>) -> bool {
        if self.synced.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        if self.may_attempt_sync()
            && let Err(e) = self.poll_redactions().await
        {
            tracing::debug!(error = %e, "initial redaction sync failed");
        }
        self.synced.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Serve one GetObjects request into `tx` (SPEC 7.3, 10.2-10.4).
    async fn serve_get_objects(
        self: Arc<Self>,
        caller: Caller,
        ids: Vec<ObjectId>,
        range: Option<(Option<u64>, Option<u64>)>,
        tx: tokio::sync::mpsc::Sender<std::result::Result<v1::ObjectChunk, Status>>,
    ) -> Result<()> {
        if self.fail_closed_now() {
            self.poll_now.notify_one();
        }
        let synced = self.ensure_synced().await;

        // Plan every id; misses join an in-flight batch or go into this request's batch
        // (SPEC 10.2 coalescing). The cache is re-checked under the in-flight lock: a batch
        // inserts into the cache before its ids leave the map, so no fetch is duplicated.
        let batch = Batch::new(caller.clone());
        let mut mine: Vec<ObjectId> = Vec::new();
        let mut plans = Vec::with_capacity(ids.len());
        let mut undecided: Vec<ObjectId> = Vec::new();
        let mut undecided_seen = HashSet::new();
        {
            let mut inflight = self.inflight.lock();
            for id in &ids {
                if self.redactions.contains(id) {
                    plans.push(Plan::Redacted);
                } else if let Some(b) = inflight.get(id) {
                    plans.push(Plan::Miss(b.clone()));
                } else if synced && self.cache.contains(id) {
                    if self.decided(&caller, id).is_none() && undecided_seen.insert(*id) {
                        undecided.push(*id);
                    }
                    plans.push(Plan::Cached);
                } else {
                    inflight.insert(*id, batch.clone());
                    mine.push(*id);
                    plans.push(Plan::Miss(batch.clone()));
                }
            }
        }
        if !mine.is_empty() {
            tokio::spawn(batch.clone().run(self.clone(), mine));
        }

        // Cached ids lacking a decision: one CheckAccess batch (SPEC 10.4).
        if !undecided.is_empty()
            && let Err(e) = self.check_access(&caller, &undecided).await
        {
            tracing::debug!(error = %e, "batched CheckAccess failed; deciding per item");
        }

        for (id, plan) in ids.iter().zip(plans) {
            let out = match plan {
                Plan::Redacted => status_only(v1::ItemStatus::Redacted),
                Plan::Cached => self.serve_cached(&caller, *id).await?,
                Plan::Miss(b) => {
                    let r = b.wait(*id).await;
                    self.resolve_fetched(&caller, *id, r).await?
                }
            };
            if !self.emit(id, out, range, &tx).await {
                return Ok(());
            }
        }
        Ok(())
    }

    /// A cached id: authorize, then read (a vanished or corrupt entry is re-fetched).
    async fn serve_cached(self: &Arc<Self>, caller: &Caller, id: ObjectId) -> Result<Out> {
        match self.authorize(caller, id).await? {
            Some(class) => match self.read_cache(id).await? {
                Some(content) => Ok(Out::Ok {
                    content,
                    access_class: class,
                    hit: true,
                }),
                None => {
                    let r = self.fetch_coalesced(caller, id).await;
                    self.resolve_fetched(caller, id, r).await
                }
            },
            None => {
                let r = self.fetch_own(caller, id).await;
                self.resolve_fetched(caller, id, r).await
            }
        }
    }

    /// Turn a fetch result into the item for `caller`: results fetched with another
    /// principal's token (or read from the cache) need `caller`'s own decision (SPEC 10.4).
    async fn resolve_fetched(
        self: &Arc<Self>,
        caller: &Caller,
        id: ObjectId,
        r: ItemResult,
    ) -> Result<Out> {
        let item = r?;
        if *item.principal == *caller.principal {
            return Ok(out_of(item, false));
        }
        if item.status == STATUS_OK
            && let Some(content) = item.content.clone()
            && let Some(class) = self.authorize(caller, id).await?
        {
            return Ok(Out::Ok {
                content,
                access_class: class,
                hit: item.principal.is_empty(),
            });
        }
        Ok(out_of(self.fetch_own(caller, id).await?, false))
    }

    /// Send the chunks of one item; false when the client went away.
    async fn emit(
        &self,
        id: &ObjectId,
        out: Out,
        range: Option<(Option<u64>, Option<u64>)>,
        tx: &tokio::sync::mpsc::Sender<std::result::Result<v1::ObjectChunk, Status>>,
    ) -> bool {
        let idb = id.as_bytes().to_vec();
        let (content, access_class, hit) = match out {
            Out::Status {
                status,
                access_class,
                total_size,
            } => {
                let c = v1::ObjectChunk {
                    id: idb,
                    status,
                    access_class,
                    total_size,
                    r#final: true,
                    ..Default::default()
                };
                return tx.send(Ok(c)).await.is_ok();
            }
            Out::Ok {
                content,
                access_class,
                hit,
            } => (content, access_class, hit),
        };
        // Last-moment check: a redaction learned while this request ran (SPEC 13.3).
        if self.redactions.contains(id) {
            let c = v1::ObjectChunk {
                id: idb,
                status: v1::ItemStatus::Redacted as i32,
                r#final: true,
                ..Default::default()
            };
            return tx.send(Ok(c)).await.is_ok();
        }
        self.count_cache(id, hit);
        let total = content.len() as u64;
        let (base, data) = match range {
            None => (0u64, content),
            Some((offset, length)) => {
                let start = offset.unwrap_or(0).min(total);
                let end = start.saturating_add(length.unwrap_or(u64::MAX)).min(total);
                (start, content.slice(start as usize..end as usize))
            }
        };
        self.metrics
            .object_bytes_served
            .with_label_values(&[super::state::TIER, "raw"])
            .inc_by(data.len() as u64);
        let pieces: Vec<Bytes> = if data.is_empty() {
            vec![Bytes::new()]
        } else {
            (0..data.len())
                .step_by(CHUNK_BYTES)
                .map(|s| data.slice(s..(s + CHUNK_BYTES).min(data.len())))
                .collect()
        };
        let n = pieces.len();
        let mut off = base;
        for (i, piece) in pieces.into_iter().enumerate() {
            let len = piece.len() as u64;
            let c = v1::ObjectChunk {
                id: idb.clone(),
                status: STATUS_OK,
                encoding: v1::Encoding::Raw as i32,
                data: piece.to_vec(),
                total_size: total,
                access_class,
                r#final: i + 1 == n,
                offset: off,
            };
            off += len;
            if tx.send(Ok(c)).await.is_err() {
                return false;
            }
        }
        true
    }

    /// ResolveRef through the RefCache (SPEC 10.3): served from cache only to principals
    /// known to read refs; otherwise (and on expiry) asked upstream with the caller's token,
    /// and the answer passes through the monotonic version filter.
    async fn resolve_ref(
        self: &Arc<Self>,
        caller: &Caller,
        req: v1::ResolveRefRequest,
    ) -> Result<v1::ResolveRefResponse> {
        let name = RefName::parse(&req.name).ok();
        if req.at_version.is_none()
            && let Some(n) = &name
            && self.ref_reader(&caller.principal)
            && let Some((target, version)) = self.refs.get(n, self.now())
        {
            return Ok(v1::ResolveRefResponse {
                target: target.as_bytes().to_vec(),
                version,
            });
        }
        let token = caller.token.clone();
        let _permit = self.limiter.acquire().await?;
        let resp = self
            .unary(|c| {
                let req = req.clone();
                let token = token.clone();
                async move { c.with_token(Some(token)).objects().resolve_ref(req).await }
            })
            .await?;
        self.note_ref_reader(&caller.principal);
        if req.at_version.is_some() {
            return Ok(resp);
        }
        let (Some(n), Ok(target)) = (name, ObjectId::from_bytes(&resp.target)) else {
            return Ok(resp);
        };
        let (target, version) = self.refs.observe(&n, target, resp.version, self.now());
        Ok(v1::ResolveRefResponse {
            target: target.as_bytes().to_vec(),
            version,
        })
    }
}

fn out_of(item: FetchItem, hit: bool) -> Out {
    match (item.status == STATUS_OK, item.content) {
        (true, Some(content)) => Out::Ok {
            content,
            access_class: item.access_class,
            hit,
        },
        _ => Out::Status {
            status: item.status,
            access_class: item.access_class,
            total_size: item.total_size,
        },
    }
}

fn code_str(e: &ScionError) -> &'static str {
    e.code.as_str()
}

#[tonic::async_trait]
impl v1::object_service_server::ObjectService for LocalObjects {
    /// SPEC 7.3: served from the cached upstream answer, refreshed periodically.
    async fn get_server_info(
        &self,
        req: Request<v1::GetServerInfoRequest>,
    ) -> std::result::Result<Response<v1::ServerInfo>, Status> {
        let started = Instant::now();
        let rid = request_id(&req);
        let inner = &self.0;
        let max_age = Duration::from_millis(inner.cfg.redaction_poll_interval_ms.max(1));
        let cached = inner.cached_server_info();
        let res = match &cached {
            Some((info, at)) if inner.now().saturating_duration_since(*at) < max_age => {
                Ok(info.clone())
            }
            _ => match inner.refresh_server_info().await {
                Ok(i) => Ok(i),
                Err(e) => cached.map(|c| c.0).ok_or(e),
            },
        };
        match res {
            Ok(mut info) => {
                if let Some(p) = inner.policy_epoch() {
                    info.policy_epoch = p;
                }
                info.redaction_epoch = info.redaction_epoch.max(inner.upstream_redaction_epoch());
                inner.log_rpc("GetServerInfo", &rid, None, "OK", started);
                Ok(Response::new(info))
            }
            Err(e) => {
                inner.log_rpc("GetServerInfo", &rid, None, code_str(&e), started);
                Err(status_from_error(&e))
            }
        }
    }

    type GetObjectsStream = ChunkStream;

    /// SPEC 7.3 / 10.2-10.4: cached objects after local authorization, misses coalesced and
    /// fetched upstream, every byte verified.
    async fn get_objects(
        &self,
        req: Request<v1::GetObjectsRequest>,
    ) -> std::result::Result<Response<Self::GetObjectsStream>, Status> {
        let started = Instant::now();
        let rid = request_id(&req);
        let inner = self.0.clone();
        let caller = match self.auth(&req).await {
            Ok(c) => c,
            Err(st) => {
                inner.log_rpc("GetObjects", &rid, None, "UNAUTHENTICATED", started);
                return Err(st);
            }
        };
        let r = req.into_inner();
        let ids = match scion_proto::convert::parse_ids(&r.ids, "ids") {
            Ok(i) => i,
            Err(e) => {
                inner.log_rpc(
                    "GetObjects",
                    &rid,
                    Some(caller.principal.clone()),
                    code_str(&e),
                    started,
                );
                return Err(status_from_error(&e));
            }
        };
        let range = (r.offset.is_some() || r.length.is_some()).then_some((r.offset, r.length));
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        tokio::spawn(async move {
            let principal = caller.principal.clone();
            let res = inner
                .clone()
                .serve_get_objects(caller, ids, range, tx.clone())
                .await;
            let status = match &res {
                Ok(()) => "OK",
                Err(e) => code_str(e),
            };
            if let Err(e) = res {
                let _ = tx.send(Err(status_from_error(&e))).await;
            }
            inner.log_rpc("GetObjects", &rid, Some(principal), status, started);
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }

    /// Forwarded (SPEC 12.5: the answer depends on server-side upload records). The router
    /// forwards this path verbatim; this typed fallback is used only if it is reached.
    async fn has_objects(
        &self,
        req: Request<v1::HasObjectsRequest>,
    ) -> std::result::Result<Response<v1::HasObjectsResponse>, Status> {
        let caller = self.auth(&req).await?;
        let r = req.into_inner();
        let token = caller.token.clone();
        self.0
            .unary(|c| {
                let r = r.clone();
                let token = token.clone();
                async move { c.with_token(Some(token)).objects().has_objects(r).await }
            })
            .await
            .map(Response::new)
            .map_err(|e| status_from_error(&e))
    }

    /// SPEC 10.3: TTL-cached with monotonic versions.
    async fn resolve_ref(
        &self,
        req: Request<v1::ResolveRefRequest>,
    ) -> std::result::Result<Response<v1::ResolveRefResponse>, Status> {
        let started = Instant::now();
        let rid = request_id(&req);
        let caller = match self.auth(&req).await {
            Ok(c) => c,
            Err(st) => {
                self.0
                    .log_rpc("ResolveRef", &rid, None, "UNAUTHENTICATED", started);
                return Err(st);
            }
        };
        let r = req.into_inner();
        let res = self.0.resolve_ref(&caller, r).await;
        let status = match &res {
            Ok(_) => "OK",
            Err(e) => code_str(e),
        };
        self.0.log_rpc(
            "ResolveRef",
            &rid,
            Some(caller.principal.clone()),
            status,
            started,
        );
        res.map(Response::new).map_err(|e| status_from_error(&e))
    }
}
