//! The importer's RPCs with bounded retry (SPEC 7.2 retryable codes).
//!
//! RESOURCE_EXHAUSTED (rate limiting, load shedding) is retried for every call: the server
//! rejected the request before acting on it. UNAVAILABLE is retried only for idempotent calls
//! (reads, uploads, CreateChangeset, whose identical retries return the same ids); ref writes
//! are not retried on UNAVAILABLE, because the write may have committed.

use scion_core::{ErrorCode, ObjectId, Result, ScionError};
use scion_proto::convert::{id_bytes, parse_id};
use scion_proto::{ScionClient, error_from_status, v1};
use std::future::Future;
use std::time::Duration;

/// Attempts per call (the first try included).
const ATTEMPTS: u32 = 16;
/// First back-off delay; doubles up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_millis(5);
/// Largest back-off delay.
const MAX_BACKOFF: Duration = Duration::from_secs(2);

async fn retry<T, F, Fut>(what: &str, idempotent: bool, mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, tonic::Status>>,
{
    let mut delay = FIRST_BACKOFF;
    let mut attempt = 1;
    loop {
        match call().await {
            Ok(v) => return Ok(v),
            Err(s) => {
                let e = error_from_status(&s);
                let retryable = e.code == ErrorCode::ResourceExhausted
                    || (idempotent && e.code == ErrorCode::Unavailable);
                if !retryable || attempt >= ATTEMPTS {
                    return Err(e);
                }
                tracing::debug!(rpc = what, attempt, code = e.code.as_str(), "retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_BACKOFF);
                attempt += 1;
            }
        }
    }
}

/// The RPCs the importer issues.
#[derive(Clone, Copy)]
pub(crate) struct Rpc<'a> {
    pub client: &'a ScionClient,
}

impl Rpc<'_> {
    /// `Resolve("git:<sha>")`: `None` on NOT_FOUND.
    pub async fn resolve_git(&self, sha_hex: &str) -> Result<Option<ObjectId>> {
        let c = self.client;
        let req = v1::ResolveRequest {
            revision: format!("git:{sha_hex}"),
        };
        let r = retry("Resolve", true, || {
            let req = req.clone();
            async move { c.query().resolve(req).await }
        })
        .await;
        match r {
            Ok(resp) => Ok(Some(parse_id(&resp.into_inner().changeset, "changeset")?)),
            Err(e) if e.code == ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `HasObjects(ids)`.
    pub async fn has_objects(&self, ids: &[ObjectId]) -> Result<Vec<bool>> {
        let c = self.client;
        let req = v1::HasObjectsRequest {
            ids: ids.iter().map(id_bytes).collect(),
        };
        let resp = retry("HasObjects", true, || {
            let req = req.clone();
            async move { c.objects().has_objects(req).await }
        })
        .await?
        .into_inner();
        if resp.present.len() != ids.len() {
            return Err(ScionError::new(
                ErrorCode::Unavailable,
                "HasObjects answered a different number of ids",
            ));
        }
        Ok(resp.present)
    }

    /// `UploadObjects(items)`: one client stream.
    pub async fn upload(&self, items: &[v1::UploadItem]) -> Result<Vec<v1::UploadResult>> {
        let c = self.client;
        let resp = retry("UploadObjects", true, || {
            let items = items.to_vec();
            async move { c.write().upload_objects(futures::stream::iter(items)).await }
        })
        .await?
        .into_inner();
        Ok(resp.results)
    }

    /// `CreateChangeset(req)`: the changeset id.
    pub async fn create_changeset(&self, req: v1::CreateChangesetRequest) -> Result<ObjectId> {
        let c = self.client;
        let resp = retry("CreateChangeset", true, || {
            let req = req.clone();
            async move { c.write().create_changeset(req).await }
        })
        .await?
        .into_inner();
        parse_id(&resp.changeset_id, "changeset_id")
    }

    /// `ResolveRef(name)`: `(target, version)`, `None` on NOT_FOUND.
    pub async fn resolve_ref(&self, name: &str) -> Result<Option<(ObjectId, u64)>> {
        let c = self.client;
        let req = v1::ResolveRefRequest {
            name: name.to_owned(),
            at_version: None,
        };
        let r = retry("ResolveRef", true, || {
            let req = req.clone();
            async move { c.objects().resolve_ref(req).await }
        })
        .await;
        match r {
            Ok(resp) => {
                let resp = resp.into_inner();
                Ok(Some((parse_id(&resp.target, "target")?, resp.version)))
            }
            Err(e) if e.code == ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `CreateRef(name, target, reason)`: the new version.
    pub async fn create_ref(&self, name: &str, target: &ObjectId, reason: &str) -> Result<u64> {
        let c = self.client;
        let req = v1::CreateRefRequest {
            name: name.to_owned(),
            target: id_bytes(target),
            reason: reason.to_owned(),
        };
        let resp = retry("CreateRef", false, || {
            let req = req.clone();
            async move { c.refs().create_ref(req).await }
        })
        .await?
        .into_inner();
        Ok(resp.version)
    }

    /// `UpdateRef(name, expected_version, target, allow_non_fast_forward)`: the new version.
    pub async fn update_ref(
        &self,
        name: &str,
        expected_version: u64,
        target: &ObjectId,
        allow_non_fast_forward: bool,
    ) -> Result<u64> {
        let c = self.client;
        let req = v1::UpdateRefRequest {
            name: name.to_owned(),
            expected_version,
            new_target: id_bytes(target),
            allow_non_fast_forward,
        };
        let resp = retry("UpdateRef", false, || {
            let req = req.clone();
            async move { c.refs().update_ref(req).await }
        })
        .await?
        .into_inner();
        Ok(resp.version)
    }

    /// `IsAncestor(a, b)`.
    pub async fn is_ancestor(&self, a: &ObjectId, b: &ObjectId) -> Result<bool> {
        let c = self.client;
        let req = v1::IsAncestorRequest {
            a: id_bytes(a),
            b: id_bytes(b),
        };
        let resp = retry("IsAncestor", true, || {
            let req = req.clone();
            async move { c.query().is_ancestor(req).await }
        })
        .await?
        .into_inner();
        Ok(resp.result)
    }

    /// The canonical encoding of changeset `id` (`GetChangesets`), `None` when it is not
    /// served in full.
    pub async fn changeset_encoding(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        let c = self.client;
        let req = v1::GetChangesetsRequest {
            ids: vec![id_bytes(id)],
        };
        let resp = retry("GetChangesets", true, || {
            let req = req.clone();
            async move { c.query().get_changesets(req).await }
        })
        .await?
        .into_inner();
        Ok(resp
            .items
            .into_iter()
            .next()
            .and_then(|item| match item.view {
                Some(v1::changeset_item::View::Encoded(enc)) => Some(enc),
                _ => None,
            }))
    }
}
