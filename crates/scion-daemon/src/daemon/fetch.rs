//! Upstream object fetches: batching, coalescing, limiting, retries, verification and cache
//! insertion (SPEC 7.3, 10.2, 10.5, 16.1 class 7).

use super::state::{Caller, Inner, MAX_ATTEMPTS};
use bytes::Bytes;
use parking_lot::Mutex;
use scion_cache::Payload;
use scion_core::{ErrorCode, ObjectId, Result, ScionError, subcode};
use scion_proto::{error_from_status, v1};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub(crate) const STATUS_OK: i32 = v1::ItemStatus::Ok as i32;
pub(crate) const STATUS_DENIED: i32 = v1::ItemStatus::Denied as i32;
/// Upstream objects verified and cached concurrently while one GetObjects stream flows.
const VERIFY_CONCURRENCY: usize = 16;
pub(crate) const CLASS_ROOT: i32 = v1::AccessClass::Root as i32;

/// One object as answered upstream (or read from the cache when `principal` is empty).
#[derive(Clone, Debug)]
pub(crate) struct FetchItem {
    /// The principal whose token fetched it; empty when it came from the local cache.
    pub principal: Arc<str>,
    /// `v1::ItemStatus` as answered upstream.
    pub status: i32,
    /// `v1::AccessClass` as answered upstream.
    pub access_class: i32,
    /// `total_size` as answered upstream (status-only items).
    pub total_size: u64,
    /// Verified raw content (OK items).
    pub content: Option<Bytes>,
}

pub(crate) type ItemResult = std::result::Result<FetchItem, ScionError>;

/// An OK item's chunks being reassembled.
struct Partial {
    encoding: i32,
    access_class: i32,
    data: Vec<u8>,
}

/// A batch of missing ids fetched by one upstream GetObjects on behalf of one request: the
/// batch-aware coalescer of SPEC 10.2 (concurrent requests for the same missing object produce
/// exactly one upstream request). While it runs, its ids map to it in [`Inner::inflight`];
/// other requests needing those ids wait for its results instead of fetching.
pub(crate) struct Batch {
    caller: Caller,
    results: Mutex<HashMap<ObjectId, ItemResult>>,
    done: tokio::sync::watch::Sender<bool>,
}

/// Unregisters a batch's ids from the in-flight map on every exit path, so a lost task can
/// never leave ids pointing at a batch that will not answer.
struct InflightGuard<'a> {
    inner: &'a Inner,
    batch: &'a Arc<Batch>,
    ids: &'a [ObjectId],
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        let mut m = self.inner.inflight.lock();
        for id in self.ids {
            if m.get(id).is_some_and(|b| Arc::ptr_eq(b, self.batch)) {
                m.remove(id);
            }
        }
    }
}

impl Batch {
    pub fn new(caller: Caller) -> Arc<Self> {
        Arc::new(Batch {
            caller,
            results: Mutex::new(HashMap::new()),
            done: tokio::sync::watch::channel(false).0,
        })
    }

    /// Run the upstream fetch for `ids` (already registered in `inflight`) and publish the
    /// results. Verified objects are in the cache before the ids leave `inflight`.
    pub async fn run(self: Arc<Self>, inner: Arc<Inner>, ids: Vec<ObjectId>) {
        {
            let _guard = InflightGuard {
                inner: &inner,
                batch: &self,
                ids: &ids,
            };
            let res = inner.fetch_upstream(&ids, &self.caller).await;
            *self.results.lock() = res;
        }
        self.done.send_replace(true);
    }

    /// The result for `id`.
    pub async fn wait(&self, id: ObjectId) -> ItemResult {
        let mut rx = self.done.subscribe();
        if rx.wait_for(|d| *d).await.is_err() {
            return Err(ScionError::unavailable(
                subcode::UPSTREAM_UNAVAILABLE,
                "upstream fetch was abandoned",
            ));
        }
        self.results.lock().get(&id).cloned().unwrap_or_else(|| {
            Err(ScionError::unavailable(
                subcode::UPSTREAM_UNAVAILABLE,
                format!("upstream did not answer for {id}"),
            ))
        })
    }
}

/// Verify an assembled OK item (SPEC 7.3: digest of the decompressed content before use or
/// caching) and insert it into the cache as received (SPEC 10.2). Runs on a blocking thread.
fn verify_and_insert(
    inner: &Inner,
    id: &ObjectId,
    encoding: i32,
    data: Vec<u8>,
) -> std::result::Result<Bytes, ScionError> {
    let frame = Bytes::from(data);
    let (content, payload) = if encoding == v1::Encoding::Zstd as i32 {
        let content = zstd::stream::decode_all(&frame[..])
            .map(Bytes::from)
            .map_err(|_| ScionError::integrity(id))?;
        (content, Payload::Zstd(frame))
    } else {
        (frame.clone(), Payload::Raw(frame))
    };
    if blake3::hash(&content).as_bytes() != id.digest() {
        return Err(ScionError::integrity(id));
    }
    if inner.redactions.contains(id) {
        // Learned redacted meanwhile: never cache it (SPEC 13.3).
        return Ok(content);
    }
    if let Err(e) = inner.cache.insert_verified(id, payload) {
        tracing::warn!(object = %id, error = %e, "could not cache a verified object");
    }
    // A redaction applied between the check above and the insert: the poll that applied it
    // may have looked for cached copies before this insert landed, so delete it here.
    if inner.redactions.contains(id)
        && let Err(e) = inner.cache.remove(id)
    {
        tracing::warn!(object = %id, error = %e, "could not delete a redacted cache entry");
    }
    Ok(content)
}

impl Inner {
    /// Read a cached object's raw content (verified when `verify_on_read`; a corrupt entry is
    /// deleted and reads as absent, SPEC 10.2).
    pub async fn read_cache(self: &Arc<Self>, id: ObjectId) -> Result<Option<Bytes>> {
        let me = self.clone();
        let r = tokio::task::spawn_blocking(move || me.cache.get(&id))
            .await
            .map_err(|e| ScionError::new(ErrorCode::Unavailable, format!("cache task: {e}")))?;
        self.sync_cache_metrics();
        r
    }

    /// Fetch `ids` upstream for `caller`: one limiter permit, at most [`MAX_ATTEMPTS`]
    /// GetObjects attempts with jittered backoff (incomplete ids only on retry), each OK item
    /// verified and cached (SPEC 10.2, 10.5).
    pub async fn fetch_upstream(
        self: &Arc<Self>,
        ids: &[ObjectId],
        caller: &Caller,
    ) -> HashMap<ObjectId, ItemResult> {
        let mut out: HashMap<ObjectId, ItemResult> = HashMap::new();
        let mut seen = HashSet::new();
        let mut remaining: Vec<ObjectId> =
            ids.iter().copied().filter(|i| seen.insert(*i)).collect();
        if remaining.is_empty() {
            return out;
        }
        let _permit = match self.limiter.acquire().await {
            Ok(p) => p,
            Err(e) => {
                for id in remaining {
                    out.insert(id, Err(e.clone()));
                }
                return out;
            }
        };
        let mut attempt = 0;
        while !remaining.is_empty() {
            attempt += 1;
            let err = self.get_objects_once(&remaining, caller, &mut out).await;
            remaining.retain(|id| !out.contains_key(id));
            let err = match err {
                None if remaining.is_empty() => break,
                None => ScionError::unavailable(
                    subcode::UPSTREAM_UNAVAILABLE,
                    "upstream ended the GetObjects stream early",
                ),
                Some(e) => e,
            };
            if err.code == ErrorCode::Unavailable && attempt < MAX_ATTEMPTS {
                self.backoff(attempt).await;
                continue;
            }
            for id in remaining.drain(..) {
                out.insert(id, Err(err.clone()));
            }
        }
        self.sync_cache_metrics();
        out
    }

    /// Record one verified (or rejected) upstream object in `out`.
    fn finish_item(
        &self,
        done: std::result::Result<(ObjectId, i32, Result<Bytes>), tokio::task::JoinError>,
        caller: &Caller,
        principal: &Arc<str>,
        out: &mut HashMap<ObjectId, ItemResult>,
    ) {
        let (id, class, verified) = match done {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "verify task failed");
                return;
            }
        };
        match verified {
            Ok(content) => {
                self.record_allowed(&caller.principal, id, class);
                out.insert(
                    id,
                    Ok(FetchItem {
                        principal: principal.clone(),
                        status: STATUS_OK,
                        access_class: class,
                        total_size: content.len() as u64,
                        content: Some(content),
                    }),
                );
            }
            Err(e) => {
                if e.code == ErrorCode::Integrity {
                    tracing::warn!(
                        object = %id,
                        "upstream object failed digest verification; not cached"
                    );
                }
                out.insert(id, Err(e));
            }
        }
    }

    /// One upstream GetObjects; completed items go into `out`. Returns the error that ended
    /// the stream early, if any.
    async fn get_objects_once(
        self: &Arc<Self>,
        ids: &[ObjectId],
        caller: &Caller,
        out: &mut HashMap<ObjectId, ItemResult>,
    ) -> Option<ScionError> {
        let client = match self.client().await {
            Ok(c) => c,
            Err(e) => return Some(e),
        };
        self.count_upstream();
        let req = v1::GetObjectsRequest {
            ids: ids.iter().map(|i| i.as_bytes().to_vec()).collect(),
            offset: None,
            length: None,
        };
        let resp = match client
            .with_token(Some(caller.token.clone()))
            .objects()
            .get_objects(req)
            .await
        {
            Ok(r) => r,
            Err(st) => {
                self.observe_metadata(st.metadata());
                self.reset_client_on(&st).await;
                return Some(error_from_status(&st));
            }
        };
        self.observe_metadata(resp.metadata());
        let wanted: HashSet<ObjectId> = ids.iter().copied().collect();
        let mut partial: HashMap<ObjectId, Partial> = HashMap::new();
        let principal: Arc<str> = Arc::from(caller.principal.as_str());
        let mut stream = resp.into_inner();
        // Completed objects are verified and cached on blocking threads while the stream
        // keeps flowing; at most VERIFY_CONCURRENCY at a time.
        let mut verifying: tokio::task::JoinSet<(ObjectId, i32, Result<Bytes>)> =
            tokio::task::JoinSet::new();
        let ended = loop {
            let chunk = match stream.message().await {
                Ok(Some(c)) => c,
                Ok(None) => break None,
                Err(st) => {
                    self.observe_metadata(st.metadata());
                    self.reset_client_on(&st).await;
                    break Some(error_from_status(&st));
                }
            };
            let Ok(id) = ObjectId::from_bytes(&chunk.id) else {
                tracing::warn!("upstream sent a chunk with a malformed id; ignored");
                continue;
            };
            if !wanted.contains(&id) || out.contains_key(&id) {
                continue;
            }
            if chunk.status != STATUS_OK {
                if chunk.status == STATUS_DENIED {
                    self.access.put(&caller.principal, &id, false, self.now());
                }
                out.insert(
                    id,
                    Ok(FetchItem {
                        principal: principal.clone(),
                        status: chunk.status,
                        access_class: chunk.access_class,
                        total_size: chunk.total_size,
                        content: None,
                    }),
                );
                continue;
            }
            let p = partial.entry(id).or_insert_with(|| Partial {
                encoding: chunk.encoding,
                access_class: chunk.access_class,
                data: Vec::new(),
            });
            if chunk.offset != p.data.len() as u64 {
                break Some(ScionError::unavailable(
                    subcode::UPSTREAM_UNAVAILABLE,
                    format!("upstream sent an out-of-order chunk for {id}"),
                ));
            }
            p.data.extend_from_slice(&chunk.data);
            if !chunk.r#final {
                continue;
            }
            let Some(p) = partial.remove(&id) else {
                continue;
            };
            let me = self.clone();
            let (encoding, class) = (p.encoding, p.access_class);
            verifying
                .spawn_blocking(move || (id, class, verify_and_insert(&me, &id, encoding, p.data)));
            if verifying.len() >= VERIFY_CONCURRENCY
                && let Some(done) = verifying.join_next().await
            {
                self.finish_item(done, caller, &principal, out);
            }
        };
        while let Some(done) = verifying.join_next().await {
            self.finish_item(done, caller, &principal, out);
        }
        if ended.is_some() {
            return ended;
        }
        if let Ok(Some(t)) = stream.trailers().await {
            self.observe_metadata(&t);
        }
        None
    }

    /// CheckAccess for `caller` on `ids` (SPEC 7.7, 10.4), recording the decisions: positive
    /// ones per (principal, object) plus the access class; a positive answer on a root-class
    /// object confirms the principal as a root reader (PLAN 6.13).
    pub async fn check_access(
        self: &Arc<Self>,
        caller: &Caller,
        ids: &[ObjectId],
    ) -> Result<Vec<(bool, i32)>> {
        let _permit = self.limiter.acquire().await?;
        let req = v1::CheckAccessRequest {
            subject_token: caller.token.clone(),
            object_ids: ids.iter().map(|i| i.as_bytes().to_vec()).collect(),
        };
        let token = caller.token.clone();
        let resp = self
            .unary(|c| {
                let req = req.clone();
                let token = token.clone();
                async move { c.with_token(Some(token)).auth().check_access(req).await }
            })
            .await?;
        let now = self.now();
        let mut out = Vec::with_capacity(ids.len());
        for (i, id) in ids.iter().enumerate() {
            let allowed = resp.allowed.get(i).copied().unwrap_or(false);
            let class = resp.access_class.get(i).copied().unwrap_or(0);
            if allowed {
                self.record_allowed(&caller.principal, *id, class);
                if class == CLASS_ROOT {
                    self.access.set_root_reader(&caller.principal, true, now);
                }
            } else {
                self.access.put(&caller.principal, id, false, now);
            }
            out.push((allowed, class));
        }
        Ok(out)
    }

    /// A fresh positive decision for `caller` on `id`, with the access class (SPEC 10.4).
    pub fn decided(&self, caller: &Caller, id: &ObjectId) -> Option<i32> {
        let class = self.class_of(id)?;
        let now = self.now();
        if class == CLASS_ROOT && self.access.root_reader(&caller.principal, now) == Some(true) {
            return Some(class);
        }
        (self.access.get(&caller.principal, id, now) == Some(true)).then_some(class)
    }

    /// Is `caller` allowed to read `id` (cached decision, else CheckAccess)? `Some(class)`
    /// when allowed.
    pub async fn authorize(self: &Arc<Self>, caller: &Caller, id: ObjectId) -> Result<Option<i32>> {
        if let Some(c) = self.decided(caller, &id) {
            return Ok(Some(c));
        }
        let r = self.check_access(caller, &[id]).await?;
        Ok(r.first().and_then(|(ok, class)| ok.then_some(*class)))
    }

    /// Fetch one id upstream with the caller's own token (no coalescing across principals).
    pub async fn fetch_own(self: &Arc<Self>, caller: &Caller, id: ObjectId) -> ItemResult {
        let mut m = self.fetch_upstream(&[id], caller).await;
        m.remove(&id).unwrap_or_else(|| {
            Err(ScionError::unavailable(
                subcode::UPSTREAM_UNAVAILABLE,
                format!("upstream did not answer for {id}"),
            ))
        })
    }

    /// One id through the coalescer: the cache when it has the object meanwhile, otherwise
    /// an upstream fetch with the caller's token.
    pub async fn fetch_coalesced(self: &Arc<Self>, caller: &Caller, id: ObjectId) -> ItemResult {
        self.coalescer
            .get_or_fetch(id, || self.cache_or_fetch(caller, id))
            .await
    }

    /// The body of a coalesced fetch that is not part of a batch.
    pub async fn cache_or_fetch(self: &Arc<Self>, caller: &Caller, id: ObjectId) -> ItemResult {
        if self.cache.contains(&id)
            && let Some(content) = self.read_cache(id).await?
        {
            return Ok(FetchItem {
                principal: Arc::from(""),
                status: STATUS_OK,
                access_class: 0,
                total_size: content.len() as u64,
                content: Some(content),
            });
        }
        self.fetch_own(caller, id).await
    }
}
