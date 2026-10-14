//! gRPC handler functions, one file per owning package (PLAN 2.14).
//!
//! Every handler has the shape
//! `pub async fn <rpc>(ctx: &Arc<AppContext>, req: Request<Req>) -> Result<Response<Resp>, Status>`
//! and is called in one line from [`crate::grpc`].

use std::pin::Pin;

pub mod access;
pub mod admin;
pub mod auth;
pub mod derived;
pub mod history_query;
pub mod object;
pub mod query;
pub mod refs;
pub mod server_info;
pub mod upload;
pub mod write;

/// The response stream type of every server-streaming RPC.
pub type BoxStream<T> =
    Pin<Box<dyn futures::Stream<Item = Result<T, tonic::Status>> + Send + 'static>>;
