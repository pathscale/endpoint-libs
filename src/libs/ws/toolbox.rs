use crate::libs::ws::WsMessage as Message;
use dashmap::DashMap;
use eyre::Result;
use serde::*;
use serde_json::{Map, Value};
use std::fmt::{Debug, Display, Formatter};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, OnceLock};
use tracing::*;

use crate::libs::error_code::ErrorCode;
use crate::libs::handler::HandlerError;
use crate::libs::log::LogLevel;
use crate::libs::ws::{
    ConnectionId, WsConnection, WsLogResponse, WsRequest, WsResponseError, WsResponseValue,
    WsStreamState, WsSuccessResponse, custom_error_to_resp, internal_error_to_resp,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NoResponseError;

impl Display for NoResponseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("NoResp")
    }
}

impl std::error::Error for NoResponseError {}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CustomError {
    pub code: ErrorCode,
    pub params: Value,
}

impl CustomError {
    pub fn new(code: impl Into<ErrorCode>) -> Self {
        let code = code.into();
        Self {
            code,
            params: Value::Object(Map::new()),
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        if let Value::Object(map) = &mut self.params {
            map.insert("message".to_owned(), Value::String(message.into()));
        }
        self
    }

    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        if let Value::Object(map) = &mut self.params {
            map.insert("kind".to_owned(), Value::String(kind.into()));
        }
        self
    }

    pub fn with_details(mut self, details: impl Serialize) -> Self {
        let details = serde_json::to_value(details).unwrap_or(Value::Null);
        let Value::Object(params) = &mut self.params else {
            return self;
        };

        match details {
            Value::Object(details) => {
                for (key, value) in details {
                    if key != "kind" && key != "message" {
                        params.insert(key, value);
                    }
                }
            }
            Value::Null => {}
            value => {
                params.insert("details".to_owned(), value);
            }
        }
        self
    }

    pub fn from_sql_error(err: &str, msg: impl Display) -> Result<Self> {
        let code = u32::from_str_radix(err, 36)?;
        Ok(Self::new(ErrorCode::new(code)).with_message(msg.to_string()))
    }
}

impl Display for CustomError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.params.to_string())
    }
}

impl std::error::Error for CustomError {}

#[derive(Clone)]
pub struct RequestContext {
    pub connection_id: ConnectionId,
    pub user_id: u64,
    pub seq: u32,
    pub method: u32,
    pub log_id: u64,
    pub roles: Arc<Vec<u32>>,
    pub ip_addr: IpAddr,
}

impl RequestContext {
    pub fn empty() -> Self {
        Self {
            connection_id: 0,
            user_id: 0,
            seq: 0,
            method: 0,
            log_id: 0,
            roles: Arc::new(Vec::new()),
            ip_addr: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        }
    }
    pub fn from_conn(conn: &WsConnection) -> Self {
        let roles = conn.roles.read().clone();
        Self {
            connection_id: conn.connection_id,
            user_id: conn.get_user_id(),
            seq: 0,
            method: 0,
            log_id: conn.log_id,
            roles,
            ip_addr: conn.address.ip(),
        }
    }
}

type SendFn = dyn Fn(ConnectionId, WsResponseValue) -> bool + Send + Sync;
type SendFnArc = Arc<SendFn>;

pub struct Toolbox {
    pub send_msg: OnceLock<SendFnArc>,
}
pub type ArcToolbox = Arc<Toolbox>;
impl Toolbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            send_msg: OnceLock::new(),
        })
    }

    pub fn set_ws_states(
        &self,
        states: Arc<DashMap<ConnectionId, Arc<WsStreamState>>>,
        oneshot: bool,
        drop_on_buffer_full: bool,
    ) {
        let send_fn: SendFnArc = Arc::new(move |conn_id, msg| {
            let state = if let Some(state) = states.get(&conn_id) {
                state
            } else {
                return false;
            };
            Self::send_ws_msg(
                &state.message_queue,
                msg,
                oneshot,
                conn_id,
                drop_on_buffer_full,
            );
            true
        });
        if self.send_msg.set(send_fn).is_err() {
            warn!(
                ws_server = true,
                "set_ws_states called twice — ignoring second call"
            );
        }
    }

    pub fn send_ws_msg(
        sender: &tokio::sync::mpsc::Sender<Message>,
        resp: WsResponseValue,
        oneshot: bool,
        conn_id: ConnectionId,
        drop_on_full: bool,
    ) {
        let serialized = match serde_json::to_string(&resp) {
            Ok(s) => s,
            Err(e) => {
                error!(ws_server = true, conn_id, err=%e, "Failed to serialize WS response — dropping message");
                return;
            }
        };
        match sender.try_send(serialized.into()) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                error!(
                    ws_server = true,
                    conn_id, "Send buffer full — client too slow or disconnected"
                );
                if drop_on_full {
                    let _ = sender.try_send(Message::Close(None));
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                debug!(
                    ws_server = true,
                    conn_id, "Send channel closed — client already disconnected"
                );
            }
        }
        if oneshot {
            let _ = sender.try_send(Message::Close(None));
        }
    }
    pub fn send(&self, conn_id: ConnectionId, resp: WsResponseValue) -> bool {
        match self.send_msg.get() {
            Some(f) => f(conn_id, resp),
            None => false,
        }
    }
    pub fn send_response(&self, ctx: &RequestContext, resp: impl Serialize) {
        let params = match serde_json::value::to_raw_value(&resp) {
            Ok(p) => p,
            Err(e) => {
                error!(ws_server = true, conn_id=ctx.connection_id, err=%e, "Failed to serialize response — sending error to client");
                self.send(
                    ctx.connection_id,
                    WsResponseValue::Error(WsResponseError {
                        method: ctx.method,
                        code: ErrorCode::INTERNAL_ERROR.to_u32(),
                        seq: ctx.seq,
                        log_id: ctx.log_id.to_string(),
                        params: serde_json::json!({
                            "kind": ErrorCode::INTERNAL_ERROR.kind(),
                            "message": "Failed to serialize response",
                        }),
                    }),
                );
                return;
            }
        };
        self.send(
            ctx.connection_id,
            WsResponseValue::Immediate(WsSuccessResponse {
                method: ctx.method,
                seq: ctx.seq,
                params,
            }),
        );
    }
    pub fn send_internal_error(&self, ctx: &RequestContext, code: ErrorCode, err: eyre::Error) {
        self.send(ctx.connection_id, internal_error_to_resp(ctx, code, err));
    }
    pub fn send_request_error(&self, ctx: &RequestContext, code: ErrorCode, err: impl Display) {
        self.send(
            ctx.connection_id,
            WsResponseValue::Error(WsResponseError {
                method: ctx.method,
                code: code.to_u32(),
                seq: ctx.seq,
                log_id: ctx.log_id.to_string(),
                params: serde_json::json!({
                    "kind": code.kind(),
                    "message": err.to_string(),
                }),
            }),
        );
    }
    pub fn send_log(&self, ctx: &RequestContext, level: LogLevel, msg: impl Into<String>) {
        self.send(
            ctx.connection_id,
            WsResponseValue::Log(WsLogResponse {
                seq: ctx.seq,
                log_id: ctx.log_id,
                level,
                message: msg.into(),
            }),
        );
    }
    pub fn encode_ws_response<Resp: Serialize>(
        ctx: RequestContext,
        resp: Result<Resp>,
    ) -> Option<WsResponseValue> {
        #[allow(unused_variables)]
        let RequestContext {
            connection_id,
            user_id,
            seq,
            method,
            log_id,
            ..
        } = ctx;
        let resp = match resp {
            Ok(ok) => match serde_json::value::to_raw_value(&ok) {
                Ok(params) => WsResponseValue::Immediate(WsSuccessResponse {
                    method,
                    seq,
                    params,
                }),
                Err(e) => {
                    error!(ws_server = true, connection_id, err=%e, "Failed to serialize response — sending error to client");
                    WsResponseValue::Error(WsResponseError {
                        method,
                        code: ErrorCode::INTERNAL_ERROR.to_u32(),
                        seq,
                        log_id: log_id.to_string(),
                        params: serde_json::json!({
                            "kind": ErrorCode::INTERNAL_ERROR.kind(),
                            "message": "Failed to serialize response",
                        }),
                    })
                }
            },
            Err(err) if err.is::<NoResponseError>() => {
                return None;
            }
            Err(err) => match err.downcast::<CustomError>() {
                Ok(err) => custom_error_to_resp(&ctx, err),
                Err(err) => internal_error_to_resp(&ctx, ErrorCode::INTERNAL_ERROR, err),
            },
        };
        Some(resp)
    }

    pub fn encode_handler_response<Req, Err>(
        ctx: RequestContext,
        resp: crate::libs::handler::Response<Req, Err>,
    ) -> Option<WsResponseValue>
    where
        Req: WsRequest,
        Err: Into<CustomError>,
    {
        #[allow(unused_variables)]
        let RequestContext {
            connection_id,
            user_id,
            seq,
            method,
            log_id,
            ..
        } = ctx;
        let resp = match resp {
            Ok(ok) => match serde_json::value::to_raw_value(&ok) {
                Ok(params) => WsResponseValue::Immediate(WsSuccessResponse {
                    method,
                    seq,
                    params,
                }),
                Err(e) => {
                    error!(ws_server = true, connection_id, err=%e, "Failed to serialize response — sending error to client");
                    WsResponseValue::Error(WsResponseError {
                        method,
                        code: ErrorCode::INTERNAL_ERROR.to_u32(),
                        seq,
                        log_id: log_id.to_string(),
                        params: serde_json::json!({
                            "kind": ErrorCode::INTERNAL_ERROR.kind(),
                            "message": "Failed to serialize response",
                        }),
                    })
                }
            },
            Err(HandlerError::Public(err)) => {
                let err = err.into();
                custom_error_to_resp(&ctx, err)
            }
            Err(HandlerError::Internal(err)) => {
                internal_error_to_resp(&ctx, ErrorCode::INTERNAL_ERROR, err)
            }
            Err(HandlerError::NoResponse) => return None,
        };
        Some(resp)
    }
}
tokio::task_local! {
    pub static TOOLBOX: ArcToolbox;
}
