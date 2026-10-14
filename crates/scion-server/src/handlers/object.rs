//! Handlers: GetObjects, HasObjects, ResolveRef (SPEC 7.3, 12.5) (P25).

use crate::app::AppContext;
use crate::authz::{AccessClass, ReadDecision};
use crate::context::caller;
use crate::errors::{StatusExt, to_status};
use crate::handlers::BoxStream;
use bytes::Bytes;
use scion_core::{ErrorCode, ObjectId, Principal, Result, ScionError};
use scion_object::RefName;
use scion_proto::convert::{id_bytes, missing_field, parse_ids};
use scion_proto::v1;
use scion_store::StoredFrame;
use std::sync::Arc;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

/// Largest `data` payload of one `ObjectChunk` (bytes). Objects larger than this are streamed
/// as several chunks of the same id with increasing `offset`; `final` marks the last one.
pub const CHUNK_BYTES: usize = 1 << 20;

/// Chunks buffered between the producer task and the gRPC response stream.
const STREAM_BUFFER: usize = 4;

/// The wire access class (SPEC 7.3).
pub fn access_class_to_proto(c: AccessClass) -> v1::AccessClass {
    match c {
        AccessClass::Root => v1::AccessClass::Root,
        AccessClass::Restricted => v1::AccessClass::Restricted,
    }
}

/// SPEC 16.1 class 7 / PLAN 6.10: count a server-side integrity failure in
/// `scion_integrity_failures_total{tier=server}` (the ObjectRepo has already incremented the
/// storage counter and emitted the `integrity_alert` ERROR event).
pub(crate) fn record_integrity_alert(ctx: &AppContext, id: &ObjectId) {
    ctx.metrics
        .integrity_failures
        .with_label_values(&["server"])
        .inc();
    tracing::debug!(object = %id, "GetObjects item failed with INTEGRITY");
}

fn status_chunk(id: &ObjectId, status: v1::ItemStatus) -> v1::ObjectChunk {
    v1::ObjectChunk {
        id: id_bytes(id),
        status: status as i32,
        r#final: true,
        ..Default::default()
    }
}

/// The OK payload of one id, sent as chunks of at most [`CHUNK_BYTES`] (one empty chunk for
/// empty data). Chunks are cut lazily so a large object is held once, not twice.
struct Payload {
    id: ObjectId,
    encoding: v1::Encoding,
    data: Bytes,
    base_offset: u64,
    total_size: u64,
    class: AccessClass,
}

impl Payload {
    /// Number of chunks (at least one).
    fn chunk_count(&self) -> usize {
        self.data.len().div_ceil(CHUNK_BYTES).max(1)
    }

    /// Chunk `i` of [`Payload::chunk_count`].
    fn chunk(&self, i: usize) -> v1::ObjectChunk {
        let n = self.chunk_count();
        let start = i.saturating_mul(CHUNK_BYTES).min(self.data.len());
        let end = start.saturating_add(CHUNK_BYTES).min(self.data.len());
        v1::ObjectChunk {
            id: id_bytes(&self.id),
            status: v1::ItemStatus::Ok as i32,
            encoding: self.encoding as i32,
            data: self.data.slice(start..end).to_vec(),
            total_size: self.total_size,
            access_class: access_class_to_proto(self.class) as i32,
            r#final: i + 1 >= n,
            offset: self.base_offset + start as u64,
        }
    }
}

/// What one requested id produced.
enum Served {
    /// A single item-status chunk (`not_found`, `denied`, `redacted`).
    Status(v1::ObjectChunk),
    /// The object's data.
    Data(Payload),
    /// The server's own copy is corrupt (SPEC 16.1 class 7).
    Integrity(ScionError),
}

/// Range of `[offset, offset + length)` clamped to `total`.
fn clamp_range(offset: Option<u64>, length: Option<u64>, total: usize) -> (usize, usize) {
    let total = total as u64;
    let start = offset.unwrap_or(0).min(total);
    let end = start.saturating_add(length.unwrap_or(u64::MAX)).min(total);
    // Both bounds are at most `total`, which came from a usize.
    (
        usize::try_from(start).unwrap_or(usize::MAX),
        usize::try_from(end).unwrap_or(usize::MAX),
    )
}

/// Serve one id for `p` (SPEC 7.3, 12.5, 13.4).
async fn serve_one(
    ctx: &AppContext,
    p: &Principal,
    id: &ObjectId,
    offset: Option<u64>,
    length: Option<u64>,
) -> Result<Served> {
    let eval = ctx.authz.evaluate(p, id).await?;
    let class = match eval.decision {
        ReadDecision::Redacted => {
            return Ok(Served::Status(status_chunk(id, v1::ItemStatus::Redacted)));
        }
        ReadDecision::Deny => {
            let status = if eval.referenced {
                v1::ItemStatus::Denied
            } else {
                v1::ItemStatus::NotFound
            };
            return Ok(Served::Status(status_chunk(id, status)));
        }
        ReadDecision::Allow(c) => c,
    };
    let ranged = offset.is_some() || length.is_some();
    let loaded = if ranged {
        ctx.objects.get(id).await.map(|raw| {
            let (start, end) = clamp_range(offset, length, raw.len());
            let total = raw.len() as u64;
            (
                v1::Encoding::Raw,
                raw.slice(start..end),
                start as u64,
                total,
            )
        })
    } else {
        ctx.objects
            .get_frame(id)
            .await
            .map(|(frame, row)| match frame {
                StoredFrame::Zstd(b) => (v1::Encoding::Zstd, b, 0, row.size),
                StoredFrame::Raw(b) => (v1::Encoding::Raw, b, 0, row.size),
            })
    };
    let (encoding, data, base, total) = match loaded {
        Ok(v) => v,
        Err(e) => {
            return match e.code {
                ErrorCode::NotFound => {
                    Ok(Served::Status(status_chunk(id, v1::ItemStatus::NotFound)))
                }
                ErrorCode::Redacted => {
                    Ok(Served::Status(status_chunk(id, v1::ItemStatus::Redacted)))
                }
                ErrorCode::Integrity => {
                    record_integrity_alert(ctx, id);
                    Ok(Served::Integrity(e))
                }
                _ => Err(e),
            };
        }
    };
    let label = match encoding {
        v1::Encoding::Zstd => "zstd",
        _ => "raw",
    };
    ctx.metrics
        .object_bytes_served
        .with_label_values(&["server", label])
        .inc_by(data.len() as u64);
    Ok(Served::Data(Payload {
        id: *id,
        encoding,
        data,
        base_offset: base,
        total_size: total,
        class,
    }))
}

/// Produce the GetObjects stream for `ids` into `tx`.
async fn produce(
    ctx: Arc<AppContext>,
    p: Principal,
    ids: Vec<ObjectId>,
    offset: Option<u64>,
    length: Option<u64>,
    tx: mpsc::Sender<std::result::Result<v1::ObjectChunk, Status>>,
) {
    let mut integrity: Option<ScionError> = None;
    for id in &ids {
        match serve_one(&ctx, &p, id, offset, length).await {
            Ok(Served::Status(c)) => {
                if tx.send(Ok(c)).await.is_err() {
                    return; // The caller went away.
                }
            }
            Ok(Served::Data(payload)) => {
                for i in 0..payload.chunk_count() {
                    if tx.send(Ok(payload.chunk(i))).await.is_err() {
                        return; // The caller went away.
                    }
                }
            }
            Ok(Served::Integrity(e)) => {
                integrity.get_or_insert(e);
            }
            Err(e) => {
                let _ = tx.send(Err(to_status(&e))).await;
                return;
            }
        }
    }
    if let Some(e) = integrity {
        let _ = tx.send(Err(to_status(&e))).await;
    }
}

/// `ObjectService.GetObjects` (SPEC 7.3, 12.5, 13.4).
///
/// Streams the requested ids in request order. Per id:
/// - `ok`: the object as stored, verified first. Objects kept in object storage are sent as
///   their stored zstd frame unmodified (`encoding = ZSTD`), inline objects as raw bytes
///   (`RAW`). `total_size` is the decompressed size and `access_class` the class of the
///   caller's readable path (SPEC 12.5). Large payloads span several chunks (`offset`
///   increasing, `final` on the last).
/// - Range reads (`offset` and/or `length` set) return the decompressed slice as `RAW`, with
///   `offset` the position of the slice in the object.
/// - `redacted` for redacted ids, `denied` for objects referenced by history at no path the
///   caller can read, `not_found` for absent or unreferenced objects (an unreferenced upload is
///   readable by no one and its existence is not revealed).
///
/// A missing, denied or redacted item never fails the batch. A corrupt server copy (SPEC 16.1
/// class 7) increments `scion_integrity_failures_total{tier=server}` and emits the
/// `integrity_alert` event; every other item is still served, and the stream then ends with an
/// INTEGRITY status naming the (first) corrupt id, so the item fails with INTEGRITY. Storage
/// outages end the stream with UNAVAILABLE. Malformed ids are INVALID_ARGUMENT. Bytes sent are
/// counted in `scion_object_bytes_served_total{tier=server,encoding}`.
pub async fn get_objects(
    ctx: &Arc<AppContext>,
    req: Request<v1::GetObjectsRequest>,
) -> Result<Response<BoxStream<v1::ObjectChunk>>, Status> {
    let c = caller(&req)?;
    let r = req.into_inner();
    let ids = parse_ids(&r.ids, "ids").map_status()?;
    let (tx, rx) = mpsc::channel(STREAM_BUFFER);
    tokio::spawn(produce(
        ctx.clone(),
        c.principal,
        ids,
        r.offset,
        r.length,
        tx,
    ));
    let stream: BoxStream<v1::ObjectChunk> =
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx));
    Ok(Response::new(stream))
}

/// SPEC 12.5 HasObjects for one id: `true` only when the object is readable by `p` or `p`
/// uploaded it; redacted objects are never present.
async fn has_one(ctx: &AppContext, p: &Principal, id: &ObjectId) -> Result<bool> {
    match ctx.authz.can_read(p, id).await? {
        ReadDecision::Allow(_) => Ok(true),
        ReadDecision::Redacted => Ok(false),
        ReadDecision::Deny => ctx.storage.meta.has_upload(id, &p.id_string()).await,
    }
}

/// `ObjectService.HasObjects` (SPEC 7.3, 12.5): per id, `true` only when the object exists and
/// is readable by the caller, or the caller uploaded it; otherwise `false`, even when the
/// object exists (no existence probing). Redacted ids are `false`. Malformed ids are
/// INVALID_ARGUMENT.
pub async fn has_objects(
    ctx: &Arc<AppContext>,
    req: Request<v1::HasObjectsRequest>,
) -> Result<Response<v1::HasObjectsResponse>, Status> {
    let c = caller(&req)?;
    let ids = parse_ids(&req.get_ref().ids, "ids").map_status()?;
    let mut present = Vec::with_capacity(ids.len());
    for id in &ids {
        present.push(has_one(ctx, &c.principal, id).await.map_status()?);
    }
    Ok(Response::new(v1::HasObjectsResponse { present }))
}

/// `ObjectService.ResolveRef` (SPEC 7.3, 8.5): the ref's current `{target, version}`, read
/// strongly, or the entry at `at_version` from the ref log. NOT_FOUND for an absent ref or
/// version (or a version that deleted the ref); INVALID_ARGUMENT for a missing or malformed
/// name. Any authenticated principal may resolve refs (refs are not path-scoped).
pub async fn resolve_ref(
    ctx: &Arc<AppContext>,
    req: Request<v1::ResolveRefRequest>,
) -> Result<Response<v1::ResolveRefResponse>, Status> {
    let _c = caller(&req)?;
    let r = req.get_ref();
    if r.name.is_empty() {
        return Err(to_status(&missing_field("name")));
    }
    let name = RefName::parse(&r.name).map_status()?;
    crate::logging::annotate(&req, |f| f.ref_name = Some(name.as_str().to_owned()));
    let row = ctx
        .history
        .resolve_ref(&name, r.at_version)
        .await
        .map_status()?;
    crate::logging::annotate(&req, |f| f.ref_version = Some(row.version));
    Ok(Response::new(v1::ResolveRefResponse {
        target: id_bytes(&row.target),
        version: row.version,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(data: Bytes, encoding: v1::Encoding, base: u64, class: AccessClass) -> Payload {
        Payload {
            id: ObjectId::hash_blob(b"x"),
            encoding,
            data,
            base_offset: base,
            total_size: 9,
            class,
        }
    }

    #[test]
    fn chunking_and_ranges() {
        let one = payload(Bytes::new(), v1::Encoding::Raw, 0, AccessClass::Root);
        assert_eq!(one.chunk_count(), 1);
        assert!(one.chunk(0).r#final);
        assert!(one.chunk(0).data.is_empty());
        let big = Bytes::from(vec![7u8; CHUNK_BYTES * 2 + 3]);
        let p = payload(big, v1::Encoding::Zstd, 5, AccessClass::Restricted);
        let parts: Vec<_> = (0..p.chunk_count()).map(|i| p.chunk(i)).collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].offset, 5 + CHUNK_BYTES as u64);
        assert_eq!(parts[2].data.len(), 3);
        assert!(parts[2].r#final && !parts[0].r#final && !parts[1].r#final);
        assert_eq!(parts[0].access_class, v1::AccessClass::Restricted as i32);
        assert_eq!(parts[0].encoding, v1::Encoding::Zstd as i32);
        assert!(parts.iter().all(|c| c.total_size == 9));
        let exact = payload(
            Bytes::from(vec![1u8; CHUNK_BYTES]),
            v1::Encoding::Raw,
            0,
            AccessClass::Root,
        );
        assert_eq!(exact.chunk_count(), 1);
        assert!(exact.chunk(0).r#final);
        assert_eq!(clamp_range(Some(3), Some(4), 5), (3, 5));
        assert_eq!(clamp_range(Some(9), None, 5), (5, 5));
        assert_eq!(clamp_range(None, Some(2), 5), (0, 2));
        assert_eq!(clamp_range(Some(0), Some(0), 5), (0, 0));
    }
}
