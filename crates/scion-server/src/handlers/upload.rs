//! Handlers: UploadObjects (SPEC 7.5) (P25).

use crate::app::AppContext;
use crate::context::caller;
use crate::errors::to_status;
use bytes::Bytes;
use scion_core::{ErrorCode, Kind, ObjectId, Principal, Result, ScionError, subcode};
use scion_proto::convert::{missing_field, parse_id};
use scion_proto::v1;
use std::sync::Arc;
use tonic::Streaming;
use tonic::{Request, Response, Status};

/// The per-item outcome of one upload: the result to report, or an error that fails the whole
/// call (infrastructure failures, which the client retries).
enum ItemOutcome {
    Done(v1::UploadResult),
    Fatal(ScionError),
}

fn ok_result(id: &[u8]) -> v1::UploadResult {
    v1::UploadResult {
        id: id.to_vec(),
        status: v1::ItemStatus::Ok as i32,
        error_code: String::new(),
        message: String::new(),
    }
}

/// A failed item (SPEC 7.5): `status = REDACTED` for redacted ids, otherwise
/// `ITEM_STATUS_UNSPECIFIED`; `error_code` is the SPEC 7.2 code and `message` the reason.
pub fn error_result(id: &[u8], e: &ScionError) -> v1::UploadResult {
    let status = if e.code == ErrorCode::Redacted {
        v1::ItemStatus::Redacted
    } else {
        v1::ItemStatus::Unspecified
    };
    v1::UploadResult {
        id: id.to_vec(),
        status: status as i32,
        error_code: e.code.as_str().to_owned(),
        message: e.message.clone(),
    }
}

/// Errors that fail the whole call rather than one item: dependency outages and load
/// shedding, which are retryable and say nothing about the item.
fn is_fatal(e: &ScionError) -> bool {
    matches!(
        e.code,
        ErrorCode::Unavailable | ErrorCode::ResourceExhausted
    )
}

/// Kinds a client may upload: blobs, plus trees (validated in decoded form).
///
/// Changesets and Metas are created only by the server (CreateChangeset builds the Meta and
/// sets `creator`, SPEC 7.5), and derived objects only by the derivation engine. Metas in
/// particular are never accepted: a Meta has no `object_paths` rows and is visible to every
/// repository reader (SPEC 12.6), so an uploaded, unreferenced Meta would otherwise be
/// readable through GetObjects, which SPEC 12.5 forbids for unreferenced uploads.
fn uploadable(kind: Kind) -> bool {
    matches!(kind, Kind::Blob | Kind::Tree)
}

/// Validate and store one item for `p`.
async fn store_one(
    ctx: &AppContext,
    p: &Principal,
    item: v1::UploadItem,
) -> Result<(ObjectId, scion_store::InsertOutcome)> {
    if item.id.is_empty() {
        return Err(missing_field("id"));
    }
    let id = parse_id(&item.id, "id")?;
    let kind = u8::try_from(item.kind)
        .map_err(|_| ScionError::invalid(subcode::INVALID_KIND, "kind out of range"))
        .and_then(Kind::from_byte)?;
    if kind != id.kind() {
        return Err(ScionError::invalid(
            subcode::INVALID_KIND,
            format!(
                "kind 0x{:02x} does not match the id kind 0x{:02x}",
                kind.byte(),
                id.kind().byte()
            ),
        ));
    }
    if !uploadable(kind) {
        return Err(ScionError::invalid(
            subcode::INVALID_KIND,
            format!("objects of kind 0x{:02x} cannot be uploaded", kind.byte()),
        ));
    }
    // SPEC 13.6: a redacted id is a permanent blocklist entry. The in-memory set answers
    // without IO; `put_verified` re-checks the row strongly.
    if ctx.redactions.contains(&id) {
        return Err(ScionError::redacted(&id));
    }
    let outcome = ctx
        .objects
        .put_verified(&id, Bytes::from(item.data))
        .await?;
    ctx.storage
        .meta
        .record_upload(&id, &p.id_string(), ctx.clock.now())
        .await?;
    Ok((id, outcome))
}

async fn upload_one(ctx: &AppContext, p: &Principal, item: v1::UploadItem) -> ItemOutcome {
    let raw_id = item.id.clone();
    match store_one(ctx, p, item).await {
        // Inserted and AlreadyExists look the same: a redundant upload is deduplicated and
        // reveals nothing about prior existence (SPEC 12.5).
        Ok((id, outcome)) => {
            tracing::debug!(object = %id, ?outcome, "upload stored");
            ItemOutcome::Done(ok_result(&raw_id))
        }
        Err(e) if is_fatal(&e) => ItemOutcome::Fatal(e),
        Err(e) => {
            if e.code == ErrorCode::Integrity {
                // The client's bytes do not match the claimed id: not a server-side
                // corruption, so no integrity alert.
                tracing::info!("uploaded bytes do not match their id");
            }
            ItemOutcome::Done(error_result(&raw_id, &e))
        }
    }
}

/// `WriteService.UploadObjects` (SPEC 7.5, 12.5, 13.6, 17), a client stream of
/// `{id, kind, data}` with `data` the raw content (a blob's bytes, or the canonical encoding of
/// a tree).
///
/// Per item, in order: the kind must match the id (INVALID_ARGUMENT/invalid_kind; only blobs
/// and trees can be uploaded: changesets, metas and derived objects are server-built),
/// redacted ids fail REDACTED, the BLAKE3 digest of the data must match the id (INTEGRITY),
/// trees are decoded and validated (INVALID_ARGUMENT), sizes are limited (TOO_LARGE); then the object is stored through the
/// ObjectRepo and an `object_uploads` record is written for the caller. A redundant upload of
/// an existing object is accepted with status `ok`, exactly like a new one, so it reveals
/// nothing about prior existence.
///
/// Item failures are reported per item (see [`error_result`]); only infrastructure failures
/// (UNAVAILABLE, RESOURCE_EXHAUSTED) and stream errors fail the call.
pub async fn upload_objects(
    ctx: &Arc<AppContext>,
    req: Request<Streaming<v1::UploadItem>>,
) -> Result<Response<v1::UploadObjectsResponse>, Status> {
    let c = caller(&req)?;
    let mut stream = req.into_inner();
    let mut results = Vec::new();
    while let Some(item) = stream.message().await? {
        match upload_one(ctx, &c.principal, item).await {
            ItemOutcome::Done(r) => results.push(r),
            ItemOutcome::Fatal(e) => return Err(to_status(&e)),
        }
    }
    Ok(Response::new(v1::UploadObjectsResponse { results }))
}
