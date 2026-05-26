use async_trait::async_trait;
use eyre::Result;
use serde_json::Value;
use std::hash::Hash;
use std::sync::Arc;

use crate::libs::{
    error_code::ErrorCode,
    toolbox::{ArcToolbox, RequestContext, Toolbox},
    ws::{WsRequest, request_error_to_resp},
};

#[allow(type_alias_bounds)]
pub type Response<T: WsRequest> = Result<T::Response>;

#[async_trait(?Send)]
pub trait RequestHandler: Send + Sync {
    type Request: WsRequest + 'static;

    async fn handle(
        &self,
        ctx: RequestContext<<Self::Request as WsRequest>::Role>,
        req: Self::Request,
    ) -> Response<Self::Request>;
}

#[doc(hidden)]
#[async_trait(?Send)]
pub trait RequestHandlerErased<R>: Send + Sync {
    async fn handle(
        &self,
        toolbox: &ArcToolbox,
        ctx: RequestContext<R>,
        req: Value,
    );
}

/// Wrapper that converts roles and calls the actual handler.
pub struct HandlerWrapper<T, R> {
    handler: Arc<T>,
    _phantom: std::marker::PhantomData<R>,
}

impl<T, R> HandlerWrapper<T, R> {
    pub fn new(handler: T) -> Arc<Self> {
        Arc::new(Self {
            handler: Arc::new(handler),
            _phantom: std::marker::PhantomData,
        })
    }
}

#[async_trait(?Send)]
impl<T, R> RequestHandlerErased<R> for HandlerWrapper<T, R>
where
    T: RequestHandler + 'static,
    R: Clone
        + Eq
        + Hash
        + Send
        + Sync
        + serde::Serialize
        + for<'de> serde::Deserialize<'de>
        + 'static
        + Into<<T::Request as WsRequest>::Role>,
{
    async fn handle(&self, toolbox: &ArcToolbox, ctx: RequestContext<R>, req: Value) {
        // Deserialize the request
        let buf = serde_json::to_string(&req).unwrap();
        let data: T::Request = match serde_json::from_value(req) {
            Ok(data) => data,
            Err(err) => {
                let jd = &mut serde_json::Deserializer::from_str(&buf);
                let data: Result<T::Request, _> = serde_path_to_error::deserialize(jd);
                let path = data.err().map(|err| err.path().to_string());
                toolbox.send(
                    ctx.connection_id,
                    request_error_to_resp(
                        &ctx,
                        ErrorCode::BAD_REQUEST,
                        if let Some(path) = path {
                            format!("{path}: {err}")
                        } else {
                            format!("{err}")
                        },
                    ),
                );
                return;
            }
        };

        // Convert roles: Vec<R> → Vec<EndpointRole>
        let endpoint_roles: Vec<<T::Request as WsRequest>::Role> =
            ctx.roles.iter().cloned().map(|r| r.into()).collect();

        // Create typed context with converted roles
        let typed_ctx: RequestContext<<T::Request as WsRequest>::Role> = RequestContext {
            connection_id: ctx.connection_id,
            user_id: ctx.user_id,
            seq: ctx.seq,
            method: ctx.method,
            log_id: ctx.log_id,
            roles: Arc::new(endpoint_roles),
            ip_addr: ctx.ip_addr,
        };

        let resp = RequestHandler::handle(&*self.handler, typed_ctx.clone(), data).await;

        if let Some(resp) = Toolbox::encode_ws_response(typed_ctx, resp) {
            toolbox.send(ctx.connection_id, resp);
        }
    }
}