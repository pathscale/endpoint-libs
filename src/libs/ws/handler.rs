use async_trait::async_trait;
use serde_json::Value;

use crate::libs::{
    error_code::ErrorCode,
    toolbox::{ArcToolbox, CustomError, RequestContext, Toolbox},
    ws::{
        WsRequest, WsResponseError, WsResponseValue,
        mcp::{
            self, JsonRpcError, McpCallCtx, encode_tool_error, encode_tool_result,
            error_code_to_jsonrpc, jsonrpc_error, jsonrpc_result,
        },
    },
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

    /// MCP `tools/call` entry point: same deserialization and handler
    /// dispatch as [`Self::handle`], but the response is encoded as a
    /// JSON-RPC/MCP frame addressed to `mcp.id`.
    ///
    /// The default body keeps manual `RequestHandlerErased` impls
    /// source-compatible; the blanket impl for [`RequestHandler`] overrides it.
    async fn handle_mcp(
        &self,
        toolbox: &ArcToolbox,
        ctx: RequestContext,
        mcp: McpCallCtx,
        req: Value,
    ) {
        let _ = req;
        toolbox.send_raw(
            ctx.connection_id,
            jsonrpc_error(
                &mcp.id,
                JsonRpcError::new(mcp::METHOD_NOT_FOUND, "Handler does not support MCP"),
            )
            .to_string(),
        );
    }
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
                let message = if let Some(path) = path {
                    format!("{path}: {err}")
                } else {
                    format!("{err}")
                };
                toolbox.send(
                    ctx.connection_id,
                    WsResponseValue::Error(WsResponseError {
                        method: ctx.method,
                        code: ErrorCode::BAD_REQUEST.to_u32(),
                        seq: ctx.seq,
                        log_id: ctx.log_id.to_string(),
                        params: serde_json::json!({
                            "kind": ErrorCode::BAD_REQUEST.kind(),
                            "message": message,
                        }),
                    }),
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

    async fn handle_mcp(
        &self,
        toolbox: &ArcToolbox,
        ctx: RequestContext,
        mcp: McpCallCtx,
        req: Value,
    ) {
        let data: T::Request = match serde_json::from_value(req.clone()) {
            Ok(data) => data,
            Err(err) => {
                // Re-run through serde_path_to_error for a field-path message,
                // mirroring the legacy handle() diagnostics.
                let buf = serde_json::to_string(&req).unwrap_or_default();
                let jd = &mut serde_json::Deserializer::from_str(&buf);
                let data: std::result::Result<T::Request, _> = serde_path_to_error::deserialize(jd);
                let message = match data.err().map(|err| err.path().to_string()) {
                    Some(path) => format!("{path}: {err}"),
                    None => format!("{err}"),
                };
                toolbox.send_raw(
                    ctx.connection_id,
                    jsonrpc_error(
                        &mcp.id,
                        JsonRpcError::new(
                            mcp::INVALID_PARAMS,
                            format!("Invalid params: {message}"),
                        ),
                    )
                    .to_string(),
                );
                return;
            }
        };

        let resp = RequestHandler::handle(self, ctx.clone(), data).await;
        let frame = match resp {
            Ok(ok) => match serde_json::to_value(&ok) {
                Ok(value) => jsonrpc_result(&mcp.id, encode_tool_result(&value)),
                Err(err) => {
                    tracing::error!(
                        ws_server = true,
                        conn_id = ctx.connection_id,
                        err = %err,
                        "Failed to serialize MCP response — sending error to client"
                    );
                    jsonrpc_error(
                        &mcp.id,
                        error_code_to_jsonrpc(
                            ErrorCode::INTERNAL_ERROR,
                            serde_json::json!({ "logId": ctx.log_id.to_string() }),
                        ),
                    )
                }
            },
            // Public errors are tool-level failures: isError:true results.
            Err(HandlerError::Public(err)) => {
                let err: CustomError = err.into();
                tracing::warn!(
                    ws_server = true,
                    conn_id = ctx.connection_id,
                    "MCP request error: {:?}",
                    err
                );
                jsonrpc_result(&mcp.id, encode_tool_error(err.code, &err.params))
            }
            Err(HandlerError::Internal(err)) => {
                tracing::error!(
                    ws_server = true,
                    conn_id = ctx.connection_id,
                    "MCP internal error: {:?}",
                    err
                );
                jsonrpc_error(
                    &mcp.id,
                    error_code_to_jsonrpc(
                        ErrorCode::INTERNAL_ERROR,
                        serde_json::json!({ "logId": ctx.log_id.to_string() }),
                    ),
                )
            }
            Err(HandlerError::NoResponse) => return,
        };
        toolbox.send_raw(ctx.connection_id, frame.to_string());
    }
}
