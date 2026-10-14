//! Helpers over [`scion_proto::ScionClient`]: ref/revision resolution, GetChangesets, batched
//! GetObjects with per-item status handling (P18).
//!
//! [`Fetcher::get_objects`] implements the verification and resumption rules of SPEC 7.3 and
//! 10.1: every item is checked against its id (BLAKE3 of the decompressed content) before it is
//! handed to the caller, and a broken stream is resumed by re-requesting only the incomplete
//! objects, a partially received raw object from the byte offset already held (a partially
//! received zstd frame cannot be resumed by offset, so it is re-requested as a raw range from
//! offset 0).

use bytes::Bytes;
use scion_core::{ErrorCode, Kind, ObjectId, Result, ScionError, subcode};
use scion_object::{Canonical, Changeset, RefName};
use scion_proto::convert::{id_bytes, ids_bytes, required_id};
use scion_proto::{ScionClient, error_from_status, v1};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// Rounds without progress before a batch fetch gives up.
const MAX_STALLS: u32 = 3;
/// Digest mismatches tolerated per object before INTEGRITY is returned (SPEC 16.1 class 7).
const MAX_INTEGRITY_RETRIES: u32 = 2;

/// Per-item outcome of GetObjects (SPEC 7.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    /// Verified raw content.
    Ok(Bytes),
    /// `not_found`.
    NotFound,
    /// `denied`.
    Denied,
    /// `redacted`.
    Redacted,
}

/// Resolve a revision (SPEC 11.8) to a changeset. A plain ref name is resolved with
/// `ResolveRef` and reported as the tracking ref; anything else goes through `Resolve`.
pub(crate) async fn resolve_rev(
    client: &ScionClient,
    rev: &str,
) -> Result<(ObjectId, Option<RefName>)> {
    if let Ok(id) = ObjectId::parse_text(rev)
        && id.kind() == Kind::Changeset
    {
        return Ok((id, None));
    }
    if !rev.contains(['~', '^', '@', ':'])
        && let Ok(name) = RefName::parse(rev)
    {
        let req = v1::ResolveRefRequest {
            name: name.as_str().to_owned(),
            at_version: None,
        };
        match client.objects().resolve_ref(req).await {
            Ok(r) => {
                let target = required_id(&r.into_inner().target, "target")?;
                return Ok((target, Some(name)));
            }
            Err(s) => {
                let e = error_from_status(&s);
                if e.code != ErrorCode::NotFound {
                    return Err(e);
                }
            }
        }
    }
    let req = v1::ResolveRequest {
        revision: rev.to_owned(),
    };
    let r = client
        .query()
        .resolve(req)
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    Ok((required_id(&r.changeset, "changeset")?, None))
}

/// The root tree of changeset `cs` via `GetChangesets` (SPEC 10.1 step 1). A full encoding is
/// verified against `cs`; a filtered view (SPEC 12.6) supplies its `root_tree` field.
pub(crate) async fn root_tree_of(client: &ScionClient, cs: &ObjectId) -> Result<ObjectId> {
    let req = v1::GetChangesetsRequest {
        ids: vec![id_bytes(cs)],
    };
    let resp = client
        .query()
        .get_changesets(req)
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    let want = id_bytes(cs);
    let item = resp
        .items
        .into_iter()
        .find(|i| i.id == want)
        .ok_or_else(|| {
            ScionError::unavailable(
                subcode::UPSTREAM_UNAVAILABLE,
                "GetChangesets response omitted the requested changeset",
            )
        })?;
    match v1::ItemStatus::try_from(item.status) {
        Ok(v1::ItemStatus::Ok) => {}
        Ok(v1::ItemStatus::NotFound) => {
            return Err(ScionError::not_found(format!("changeset {cs} not found")));
        }
        Ok(v1::ItemStatus::Denied) => {
            return Err(ScionError::permission_denied(format!(
                "changeset {cs} is not readable"
            )));
        }
        Ok(v1::ItemStatus::Redacted) => return Err(ScionError::redacted(cs)),
        _ => {
            return Err(ScionError::unavailable(
                subcode::UPSTREAM_UNAVAILABLE,
                "GetChangesets returned an unspecified item status",
            ));
        }
    }
    match item.view {
        Some(v1::changeset_item::View::Encoded(enc)) => {
            if ObjectId::hash_encoded(Kind::Changeset, &enc) != *cs {
                return Err(ScionError::integrity(cs));
            }
            Ok(Changeset::decode(&enc)?.root_tree)
        }
        Some(v1::changeset_item::View::FilteredView(f)) => required_id(&f.root_tree, "root_tree"),
        None => Err(scion_proto::convert::missing_field("view")),
    }
}

/// Batched, verified, resumable GetObjects (SPEC 7.3, 10.1).
pub(crate) struct Fetcher {
    client: ScionClient,
    batch: usize,
    requested: Mutex<BTreeSet<ObjectId>>,
}

/// Progress state of one batch.
#[derive(Default)]
struct BatchState {
    /// Ids still to deliver.
    todo: BTreeSet<ObjectId>,
    /// Raw prefixes of incomplete objects, resumable by offset.
    partial: BTreeMap<ObjectId, Vec<u8>>,
    /// Digest mismatches seen per id.
    integrity: BTreeMap<ObjectId, u32>,
    /// Completed items.
    done: Vec<(ObjectId, Item)>,
}

impl BatchState {
    fn marker(&self) -> (usize, usize, usize, usize) {
        (
            self.todo.len(),
            self.partial.len(),
            self.partial.values().map(Vec::len).sum(),
            self.integrity.values().map(|v| *v as usize).sum(),
        )
    }

    fn finish(&mut self, id: ObjectId, item: Item) {
        if self.todo.remove(&id) {
            self.partial.remove(&id);
            self.done.push((id, item));
        }
    }

    /// Check a fully received object; returns an error after repeated digest mismatches.
    fn complete(&mut self, id: ObjectId, raw: Vec<u8>, total: u64) -> Result<()> {
        if raw.len() as u64 == total && ObjectId::hash_encoded(id.kind(), &raw) == id {
            self.finish(id, Item::Ok(Bytes::from(raw)));
            return Ok(());
        }
        self.partial.remove(&id);
        let n = self.integrity.entry(id).or_insert(0);
        *n += 1;
        tracing::warn!(object = %id, attempt = *n, "received object does not match its id");
        if *n > MAX_INTEGRITY_RETRIES {
            return Err(ScionError::integrity(&id));
        }
        Ok(())
    }
}

impl Fetcher {
    pub(crate) fn new(client: ScionClient, batch: usize) -> Self {
        Fetcher {
            client,
            batch: batch.max(1),
            requested: Mutex::new(BTreeSet::new()),
        }
    }

    /// Every id requested through GetObjects so far (SPEC 19.10 asserted object set).
    pub(crate) fn requested(&self) -> BTreeSet<ObjectId> {
        self.requested
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Fetch `ids` (deduplicated) in batches of the configured size; every `Ok` item is
    /// verified. The result has one entry per distinct id.
    pub(crate) async fn get_objects(&self, ids: &[ObjectId]) -> Result<Vec<(ObjectId, Item)>> {
        let mut seen = BTreeSet::new();
        let uniq: Vec<ObjectId> = ids.iter().copied().filter(|i| seen.insert(*i)).collect();
        let mut out = Vec::with_capacity(uniq.len());
        for chunk in uniq.chunks(self.batch) {
            out.extend(self.fetch_batch(chunk).await?);
        }
        Ok(out)
    }

    async fn fetch_batch(&self, ids: &[ObjectId]) -> Result<Vec<(ObjectId, Item)>> {
        {
            let mut r = self.requested.lock().unwrap_or_else(|e| e.into_inner());
            r.extend(ids.iter().copied());
        }
        let mut st = BatchState {
            todo: ids.iter().copied().collect(),
            ..Default::default()
        };
        let mut stalls = 0u32;
        let mut last_err: Option<ScionError> = None;
        while !st.todo.is_empty() {
            let before = st.marker();
            let fresh: Vec<ObjectId> = st
                .todo
                .iter()
                .filter(|id| !st.partial.contains_key(id))
                .copied()
                .collect();
            if !fresh.is_empty()
                && let Err(e) = self.stream(&fresh, None, &mut st).await
            {
                if !e.retryable() {
                    return Err(e);
                }
                last_err = Some(e);
            }
            let resumes: Vec<(ObjectId, u64)> = st
                .partial
                .iter()
                .filter(|(id, _)| st.todo.contains(id))
                .map(|(id, b)| (*id, b.len() as u64))
                .collect();
            for (id, offset) in resumes {
                tracing::debug!(object = %id, offset, "resuming an incomplete object");
                if let Err(e) = self.stream(&[id], Some(offset), &mut st).await {
                    if !e.retryable() {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
            if st.marker() == before {
                stalls += 1;
                if stalls >= MAX_STALLS {
                    return Err(last_err.unwrap_or_else(|| {
                        ScionError::unavailable(
                            subcode::UPSTREAM_UNAVAILABLE,
                            "GetObjects did not deliver every requested object",
                        )
                    }));
                }
            } else {
                stalls = 0;
            }
        }
        Ok(st.done)
    }

    /// One GetObjects stream; chunks are applied to `st` as they arrive.
    async fn stream(
        &self,
        ids: &[ObjectId],
        offset: Option<u64>,
        st: &mut BatchState,
    ) -> Result<()> {
        let req = v1::GetObjectsRequest {
            ids: ids_bytes(ids),
            offset,
            length: None,
        };
        let mut s = self
            .client
            .objects()
            .get_objects(req)
            .await
            .map_err(|s| error_from_status(&s))?
            .into_inner();
        let mut zbuf: BTreeMap<ObjectId, Vec<u8>> = BTreeMap::new();
        let result = loop {
            match s.message().await {
                Ok(Some(chunk)) => {
                    if let Err(e) = on_chunk(chunk, &mut zbuf, st) {
                        break Err(e);
                    }
                }
                Ok(None) => break Ok(()),
                Err(status) => break Err(error_from_status(&status)),
            }
        };
        // Incomplete zstd frames cannot be resumed by offset: re-request them as raw ranges.
        for id in zbuf.into_keys() {
            if st.todo.contains(&id) {
                st.partial.entry(id).or_default();
            }
        }
        result
    }

    /// Per-item statuses only (SPEC 7.3), via zero-length range reads; used to detect newly
    /// redacted blobs (SPEC 13.7).
    pub(crate) async fn probe_status(
        &self,
        ids: &[ObjectId],
    ) -> Result<BTreeMap<ObjectId, v1::ItemStatus>> {
        let mut out = BTreeMap::new();
        for chunk in ids.chunks(self.batch) {
            let req = v1::GetObjectsRequest {
                ids: ids_bytes(chunk),
                offset: Some(0),
                length: Some(0),
            };
            let mut s = self
                .client
                .objects()
                .get_objects(req)
                .await
                .map_err(|s| error_from_status(&s))?
                .into_inner();
            while let Some(c) = s.message().await.map_err(|s| error_from_status(&s))? {
                let Ok(id) = ObjectId::from_bytes(&c.id) else {
                    continue;
                };
                let status =
                    v1::ItemStatus::try_from(c.status).unwrap_or(v1::ItemStatus::Unspecified);
                out.entry(id).or_insert(status);
            }
        }
        Ok(out)
    }
}

/// Apply one received chunk.
fn on_chunk(
    chunk: v1::ObjectChunk,
    zbuf: &mut BTreeMap<ObjectId, Vec<u8>>,
    st: &mut BatchState,
) -> Result<()> {
    let id = ObjectId::from_bytes(&chunk.id)?;
    if !st.todo.contains(&id) {
        tracing::debug!(object = %id, "ignoring a chunk for an object not awaited");
        return Ok(());
    }
    match v1::ItemStatus::try_from(chunk.status) {
        Ok(v1::ItemStatus::Ok) => {}
        Ok(v1::ItemStatus::NotFound) => {
            st.finish(id, Item::NotFound);
            return Ok(());
        }
        Ok(v1::ItemStatus::Denied) => {
            st.finish(id, Item::Denied);
            return Ok(());
        }
        Ok(v1::ItemStatus::Redacted) => {
            st.finish(id, Item::Redacted);
            return Ok(());
        }
        _ => {
            return Err(ScionError::unavailable(
                subcode::UPSTREAM_UNAVAILABLE,
                "GetObjects returned an unspecified item status",
            ));
        }
    }
    if v1::Encoding::try_from(chunk.encoding) == Ok(v1::Encoding::Zstd) {
        let buf = zbuf.entry(id).or_default();
        buf.extend_from_slice(&chunk.data);
        if chunk.r#final {
            let frame = zbuf.remove(&id).unwrap_or_default();
            match zstd::stream::decode_all(&frame[..]) {
                Ok(raw) => st.complete(id, raw, chunk.total_size)?,
                Err(_) => st.complete(id, Vec::new(), u64::MAX)?,
            }
        }
        return Ok(());
    }
    let buf = st.partial.entry(id).or_default();
    let have = buf.len() as u64;
    if chunk.offset <= have {
        let skip = usize::try_from(have - chunk.offset).unwrap_or(usize::MAX);
        if let Some(tail) = chunk.data.get(skip..) {
            buf.extend_from_slice(tail);
        }
    } else {
        // A gap: keep what we have; the resume request asks for the missing range.
        return Ok(());
    }
    if chunk.r#final {
        let raw = st.partial.remove(&id).unwrap_or_default();
        st.complete(id, raw, chunk.total_size)?;
    }
    Ok(())
}
