//! The six generated tonic server traits, each method delegating in one line to
//! `handlers::<file>::<rpc>` (PLAN 2.14; frozen).

use crate::app::AppContext;
use crate::handlers::{self, BoxStream};
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status, Streaming};

/// Implements every `scion.v1` service over one [`AppContext`].
#[derive(Clone)]
pub struct GrpcServices {
    ctx: Arc<AppContext>,
}

impl GrpcServices {
    /// Services over `ctx`.
    pub fn new(ctx: Arc<AppContext>) -> Self {
        GrpcServices { ctx }
    }

    /// The shared application context.
    pub fn context(&self) -> &Arc<AppContext> {
        &self.ctx
    }
}

#[tonic::async_trait]
impl v1::object_service_server::ObjectService for GrpcServices {
    async fn get_server_info(
        &self,
        request: Request<v1::GetServerInfoRequest>,
    ) -> Result<Response<v1::ServerInfo>, Status> {
        handlers::server_info::get_server_info(&self.ctx, request).await
    }

    type GetObjectsStream = BoxStream<v1::ObjectChunk>;

    async fn get_objects(
        &self,
        request: Request<v1::GetObjectsRequest>,
    ) -> Result<Response<Self::GetObjectsStream>, Status> {
        handlers::object::get_objects(&self.ctx, request).await
    }

    async fn has_objects(
        &self,
        request: Request<v1::HasObjectsRequest>,
    ) -> Result<Response<v1::HasObjectsResponse>, Status> {
        handlers::object::has_objects(&self.ctx, request).await
    }

    async fn resolve_ref(
        &self,
        request: Request<v1::ResolveRefRequest>,
    ) -> Result<Response<v1::ResolveRefResponse>, Status> {
        handlers::object::resolve_ref(&self.ctx, request).await
    }
}

#[tonic::async_trait]
impl v1::query_service_server::QueryService for GrpcServices {
    async fn get_changesets(
        &self,
        request: Request<v1::GetChangesetsRequest>,
    ) -> Result<Response<v1::GetChangesetsResponse>, Status> {
        handlers::query::get_changesets(&self.ctx, request).await
    }

    type ReadPathStream = BoxStream<v1::ReadPathChunk>;

    async fn read_path(
        &self,
        request: Request<v1::ReadPathRequest>,
    ) -> Result<Response<Self::ReadPathStream>, Status> {
        handlers::query::read_path(&self.ctx, request).await
    }

    async fn list_tree(
        &self,
        request: Request<v1::ListTreeRequest>,
    ) -> Result<Response<v1::ListTreeResponse>, Status> {
        handlers::query::list_tree(&self.ctx, request).await
    }

    async fn log(
        &self,
        request: Request<v1::LogRequest>,
    ) -> Result<Response<v1::LogResponse>, Status> {
        handlers::history_query::log(&self.ctx, request).await
    }

    async fn blame(
        &self,
        request: Request<v1::BlameRequest>,
    ) -> Result<Response<v1::BlameResponse>, Status> {
        handlers::history_query::blame(&self.ctx, request).await
    }

    type DiffStream = BoxStream<v1::DiffItem>;

    async fn diff(
        &self,
        request: Request<v1::DiffRequest>,
    ) -> Result<Response<Self::DiffStream>, Status> {
        handlers::query::diff(&self.ctx, request).await
    }

    async fn is_ancestor(
        &self,
        request: Request<v1::IsAncestorRequest>,
    ) -> Result<Response<v1::IsAncestorResponse>, Status> {
        handlers::query::is_ancestor(&self.ctx, request).await
    }

    async fn merge_base(
        &self,
        request: Request<v1::MergeBaseRequest>,
    ) -> Result<Response<v1::MergeBaseResponse>, Status> {
        handlers::query::merge_base(&self.ctx, request).await
    }

    async fn resolve(
        &self,
        request: Request<v1::ResolveRequest>,
    ) -> Result<Response<v1::ResolveResponse>, Status> {
        handlers::query::resolve(&self.ctx, request).await
    }

    async fn get_derived(
        &self,
        request: Request<v1::GetDerivedRequest>,
    ) -> Result<Response<v1::GetDerivedResponse>, Status> {
        handlers::derived::get_derived(&self.ctx, request).await
    }
}

#[tonic::async_trait]
impl v1::write_service_server::WriteService for GrpcServices {
    async fn upload_objects(
        &self,
        request: Request<Streaming<v1::UploadItem>>,
    ) -> Result<Response<v1::UploadObjectsResponse>, Status> {
        handlers::upload::upload_objects(&self.ctx, request).await
    }

    async fn create_changeset(
        &self,
        request: Request<v1::CreateChangesetRequest>,
    ) -> Result<Response<v1::CreateChangesetResponse>, Status> {
        handlers::write::create_changeset(&self.ctx, request).await
    }

    async fn rebase(
        &self,
        request: Request<v1::RebaseRequest>,
    ) -> Result<Response<v1::RebaseResponse>, Status> {
        handlers::write::rebase(&self.ctx, request).await
    }

    async fn merge(
        &self,
        request: Request<v1::MergeRequest>,
    ) -> Result<Response<v1::MergeResponse>, Status> {
        handlers::write::merge(&self.ctx, request).await
    }

    async fn land(
        &self,
        request: Request<v1::LandRequest>,
    ) -> Result<Response<v1::LandResponse>, Status> {
        handlers::write::land(&self.ctx, request).await
    }
}

#[tonic::async_trait]
impl v1::ref_service_server::RefService for GrpcServices {
    async fn update_ref(
        &self,
        request: Request<v1::UpdateRefRequest>,
    ) -> Result<Response<v1::UpdateRefResponse>, Status> {
        handlers::refs::update_ref(&self.ctx, request).await
    }

    async fn create_ref(
        &self,
        request: Request<v1::CreateRefRequest>,
    ) -> Result<Response<v1::CreateRefResponse>, Status> {
        handlers::refs::create_ref(&self.ctx, request).await
    }

    async fn delete_ref(
        &self,
        request: Request<v1::DeleteRefRequest>,
    ) -> Result<Response<v1::DeleteRefResponse>, Status> {
        handlers::refs::delete_ref(&self.ctx, request).await
    }

    async fn get_ref_log(
        &self,
        request: Request<v1::GetRefLogRequest>,
    ) -> Result<Response<v1::GetRefLogResponse>, Status> {
        handlers::refs::get_ref_log(&self.ctx, request).await
    }
}

#[tonic::async_trait]
impl v1::auth_service_server::AuthService for GrpcServices {
    async fn login(
        &self,
        request: Request<v1::LoginRequest>,
    ) -> Result<Response<v1::LoginResponse>, Status> {
        handlers::auth::login(&self.ctx, request).await
    }

    async fn who_am_i(
        &self,
        request: Request<v1::WhoAmIRequest>,
    ) -> Result<Response<v1::WhoAmIResponse>, Status> {
        handlers::auth::who_am_i(&self.ctx, request).await
    }

    async fn check_access(
        &self,
        request: Request<v1::CheckAccessRequest>,
    ) -> Result<Response<v1::CheckAccessResponse>, Status> {
        handlers::access::check_access(&self.ctx, request).await
    }
}

#[tonic::async_trait]
impl v1::admin_service_server::AdminService for GrpcServices {
    async fn redact(
        &self,
        request: Request<v1::RedactRequest>,
    ) -> Result<Response<v1::RedactResponse>, Status> {
        handlers::admin::redact(&self.ctx, request).await
    }

    async fn unredact(
        &self,
        request: Request<v1::UnredactRequest>,
    ) -> Result<Response<v1::UnredactResponse>, Status> {
        handlers::admin::unredact(&self.ctx, request).await
    }

    async fn purge_now(
        &self,
        request: Request<v1::PurgeNowRequest>,
    ) -> Result<Response<v1::PurgeNowResponse>, Status> {
        handlers::admin::purge_now(&self.ctx, request).await
    }

    async fn list_redactions(
        &self,
        request: Request<v1::ListRedactionsRequest>,
    ) -> Result<Response<v1::ListRedactionsResponse>, Status> {
        handlers::admin::list_redactions(&self.ctx, request).await
    }
}
