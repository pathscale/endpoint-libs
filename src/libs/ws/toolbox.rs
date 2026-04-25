use dashmap::DashMap;
use eyre::{Context, Result};
use serde::*;
use serde_json::Value;
use std::fmt::{Debug, Display, Formatter};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, OnceLock};
use tokio_tungstenite::tungstenite::Message;
use tracing::*;

use crate::libs::error_code::ErrorCode;
use crate::libs::log::LogLevel;
use crate::libs::ws::{
    ConnectionId, WsConnection, WsLogResponse, WsResponseValue, WsStreamState, WsSuccessResponse,
    internal_error_to_resp, request_error_to_resp,
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
    pub fn new(code: impl Into<ErrorCode>, reason: impl Serialize) -> Self {
        Self {
            code: code.into(),
            params: serde_json::to_value(reason)
                .context("Failed to serialize error reason")
                .unwrap(),
        }
    }
    pub fn from_sql_error(err: &str, msg: impl Display) -> Result<Self> {
        let code = u32::from_str_radix(err, 36)?;
        let error_code = ErrorCode::new(code);
        let this = Self {
            code: error_code,
            params: msg.to_string().into(),
        };

        Ok(this)
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
    ) {
        let send_fn: SendFnArc = Arc::new(move |conn_id, msg| {
            let state = if let Some(state) = states.get(&conn_id) {
                state
            } else {
                return false;
            };
            Self::send_ws_msg(&state.message_queue, msg, oneshot);
            true
        });
        if self.send_msg.set(send_fn).is_err() {
            panic!("set_ws_states called twice");
        }
    }

    pub fn send_ws_msg(
        sender: &tokio::sync::mpsc::Sender<Message>,
        resp: WsResponseValue,
        oneshot: bool,
    ) {
        let resp = serde_json::to_string(&resp).unwrap();
        match sender.try_send(resp.into()) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                error!("WebSocket send buffer full — client is too slow or disconnected");
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                debug!("WebSocket send channel closed (client already disconnected)");
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
        self.send(
            ctx.connection_id,
            WsResponseValue::Immediate(WsSuccessResponse {
                method: ctx.method,
                seq: ctx.seq,
                params: serde_json::value::to_raw_value(&resp)
                    .expect("Failed to serialize response"),
            }),
        );
    }
    pub fn send_internal_error(&self, ctx: &RequestContext, code: ErrorCode, err: eyre::Error) {
        self.send(ctx.connection_id, internal_error_to_resp(ctx, code, err));
    }
    pub fn send_request_error(&self, ctx: &RequestContext, code: ErrorCode, err: impl Into<Value>) {
        self.send(ctx.connection_id, request_error_to_resp(ctx, code, err));
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
            Ok(ok) => WsResponseValue::Immediate(WsSuccessResponse {
                method,
                seq,
                params: serde_json::value::to_raw_value(&ok).expect("Failed to serialize response"),
            }),
            Err(err) if err.is::<NoResponseError>() => {
                return None;
            }
            Err(err) => match err.downcast::<CustomError>() {
                Ok(err) => request_error_to_resp(&ctx, err.code, err.params),
                Err(err) => internal_error_to_resp(&ctx, ErrorCode::INTERNAL_ERROR, err),
            },
        };
        Some(resp)
    }
}
tokio::task_local! {
    pub static TOOLBOX: ArcToolbox;
}
