use parking_lot::RwLock;
use serde::de::DeserializeOwned;
use serde::*;
use serde_json::Value;
use serde_json::value::RawValue;
use std::collections::HashSet;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use crate::libs::error_code::ErrorCode;
use crate::libs::handler::RequestHandlerErased;
use crate::libs::log::{CustomEyreHandler, LogLevel};
use crate::libs::toolbox::RequestContext;
use crate::model::EndpointSchema;

pub type ConnectionId = u32;

pub trait WsRequest: Serialize + DeserializeOwned + Send + Sync + Clone {
    type Response: WsResponse;
    const METHOD_ID: u32;
    const SCHEMA: &'static str;
    const ROLES: &'static [u32];
}
pub trait WsResponse: Serialize + DeserializeOwned + Send + Sync + Clone {
    type Request: WsRequest;
}
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct WsRequestGeneric<Req> {
    pub method: u32,
    pub seq: u32,
    pub params: Req,
}
pub type WsRequestValue = WsRequestGeneric<Value>;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct WsResponseError {
    pub method: u32,
    pub code: u32,
    pub seq: u32,
    pub log_id: String,
    pub params: Value,
}

#[derive(Debug)]
pub struct WsConnection {
    pub connection_id: ConnectionId,
    pub user_id: AtomicU64,
    pub roles: Arc<RwLock<Arc<Vec<u32>>>>,
    pub address: SocketAddr,
    pub log_id: u64,
}
impl WsConnection {
    pub fn get_user_id(&self) -> u64 {
        self.user_id.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn get_roles(&self) -> Arc<Vec<u32>> {
        self.roles.read().clone()
    }

    pub fn set_user_id(&self, user_id: u64) {
        self.user_id
            .store(user_id, std::sync::atomic::Ordering::Release);
    }

    pub fn set_roles(&self, roles: Arc<Vec<u32>>) {
        let mut roles_lock = self.roles.write();
        *roles_lock = roles;
    }
}

pub type WsSuccessResponse = WsSuccessResponseGeneric<Box<RawValue>>;
pub type WsStreamResponse = WsStreamResponseGeneric<Box<RawValue>>;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WsForwardedResponse {
    pub method: u32,
    pub seq: u32,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WsSuccessResponseGeneric<Params> {
    pub method: u32,
    pub seq: u32,
    pub params: Params,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WsStreamResponseGeneric<Params> {
    pub original_seq: u32,
    pub method: u32,
    pub stream_seq: u32,
    pub stream_code: u32,
    pub data: Params,
}
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WsLogResponse {
    pub seq: u32,
    pub log_id: u64,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum WsResponseGeneric<Resp> {
    Immediate(WsSuccessResponseGeneric<Resp>),
    Stream(WsStreamResponseGeneric<Resp>),
    Error(WsResponseError),
    Log(WsLogResponse),
    Forwarded(WsForwardedResponse),
    Close,
}

pub type WsResponseValue = WsResponseGeneric<Box<RawValue>>;

pub struct WsEndpoint {
    pub schema: EndpointSchema,
    pub handler: Arc<dyn RequestHandlerErased>,
    pub allowed_roles: HashSet<u32>,
}

pub fn internal_error_to_resp(
    ctx: &RequestContext,
    code: ErrorCode,
    err0: eyre::Error,
) -> WsResponseValue {
    let log_id = ctx.log_id.to_string();
    let err = WsResponseError {
        method: ctx.method,
        code: code.to_u32(),
        seq: ctx.seq,
        log_id,
        params: Value::Null,
    };

    let location = match err0.handler().downcast_ref::<CustomEyreHandler>() {
        Some(handler) => handler.get_location().map(|location| location.to_string()),
        None => None,
    };

    if let Some(location) = location {
        tracing::error!(
            ws_server = true,
            caller_location = location,
            "Internal error: {:?} {:?}",
            err,
            err0
        );
    } else {
        tracing::error!(ws_server = true, "Internal error: {:?} {:?}", err, err0);
    }

    WsResponseValue::Error(err)
}

pub fn request_error_to_resp(
    ctx: &RequestContext,
    code: ErrorCode,
    params: impl Into<Value>,
) -> WsResponseValue {
    let log_id = ctx.log_id.to_string();
    let params = params.into();
    let err = WsResponseError {
        method: ctx.method,
        code: code.to_u32(),
        seq: ctx.seq,
        log_id,
        params,
    };
    tracing::warn!(ws_server = true, "Request error: {:?}", err);
    WsResponseValue::Error(err)
}
