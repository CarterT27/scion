//! [`DeriveCtx`] over an [`ObjectRepo`] (P14).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::types::DeriveCtx;
use async_trait::async_trait;
use bytes::Bytes;
use scion_core::{Kind, ObjectId, Result};
use scion_history::ChangesetRecord;
use scion_object::{Meta, Tree};
use scion_store::{DerivedKey, ObjectRepo};
use std::sync::Arc;

/// The production derivation context.
#[derive(Clone)]
pub struct StoreDeriveCtx {
    objects: ObjectRepo,
}

impl StoreDeriveCtx {
    /// A context over `objects`.
    pub fn new(objects: ObjectRepo) -> Self {
        StoreDeriveCtx { objects }
    }
}

#[async_trait]
impl DeriveCtx for StoreDeriveCtx {
    async fn changeset(&self, id: &ObjectId) -> Result<ChangesetRecord> {
        todo!("P14")
    }

    async fn tree(&self, id: &ObjectId) -> Result<Arc<Tree>> {
        todo!("P14")
    }

    async fn meta(&self, id: &ObjectId) -> Result<Option<Arc<Meta>>> {
        todo!("P14")
    }

    async fn blob_or_empty(&self, id: &ObjectId) -> Result<Bytes> {
        todo!("P14")
    }

    async fn get_object(&self, id: &ObjectId) -> Result<Bytes> {
        todo!("P14")
    }

    async fn put_object(&self, kind: Kind, canonical: Bytes) -> Result<ObjectId> {
        todo!("P14")
    }

    async fn stored(&self, key: &DerivedKey) -> Result<Option<Bytes>> {
        todo!("P14")
    }
}
