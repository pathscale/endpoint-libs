use async_trait::async_trait;
use serde_json::Value;

use crate::libs::{
    error_code::ErrorCode,
    toolbox::{ArcToolbox, CustomError, RequestContext, Toolbox},
    ws::{WsRequest, request_error_to_resp},
};

#[allow(type_alias_bounds)]
pub type Response<T: WsRequest, E = CustomError> = Result<T::Response, HandlerError<E>>;

#[derive(Debug)]
pub enum HandlerError<E> {
    Public(E),
    Internal(eyre::Report),
    NoResponse,
}

impl<E> HandlerError<E> {
    pub fn internal(err: impl Into<eyre::Report>) -> Self {
        Self::Internal(err.into())
    }

    pub fn no_response() -> Self {
        Self::NoResponse
    }
}

impl<E> From<E> for HandlerError<E> {
    fn from(err: E) -> Self {
        Self::Public(err)
    }
}

pub trait HandlerResultExt<T, E> {
    fn internal(self) -> Result<T, HandlerError<E>>;
}

impl<T, E, Err> HandlerResultExt<T, E> for Result<T, Err>
where
    Err: Into<eyre::Report>,
{
    fn internal(self) -> Result<T, HandlerError<E>> {
        self.map_err(HandlerError::internal)
    }
}

#[async_trait(?Send)]
pub trait RequestHandler: Send + Sync {
    type Request: WsRequest + 'static;
    type Error: Into<CustomError> + 'static;

    async fn handle(
        &self,
        ctx: RequestContext,
        req: Self::Request,
    ) -> Response<Self::Request, Self::Error>;
}

#[doc(hidden)]
#[async_trait(?Send)]
pub trait RequestHandlerErased: Send + Sync {
    async fn handle(&self, toolbox: &ArcToolbox, ctx: RequestContext, req: Value);
}

#[async_trait(?Send)]
impl<T: RequestHandler> RequestHandlerErased for T {
    async fn handle(&self, toolbox: &ArcToolbox, ctx: RequestContext, req: Value) {
        // TODO: find a better way to avoid double parsing or serialization
        let buf = serde_json::to_string(&req).unwrap();
        let data: T::Request = match serde_json::from_value(req) {
            Ok(data) => data,
            Err(err) => {
                let jd = &mut serde_json::Deserializer::from_str(&buf);
                let data: std::result::Result<T::Request, _> = serde_path_to_error::deserialize(jd);
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

        let fut = RequestHandler::handle(self, ctx.clone(), data);

        let resp = fut.await;
        if let Some(resp) =
            Toolbox::encode_handler_response::<T::Request, T::Error>(ctx.clone(), resp)
        {
            toolbox.send(ctx.connection_id, resp);
        }
    }
}
