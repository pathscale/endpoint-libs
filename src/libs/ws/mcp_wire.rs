//! Minimal JSON-RPC 2.0 wire types shared by MCP-compatible local protocols.
//!
//! The full MCP tool registry and endpoint dispatcher remain behind `ws-core`.
//! This module belongs to `wire-core`, so local agent-control clients can speak
//! MCP-compatible frames without compiling the server, HTTP, TLS, or schema stack.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn call(id: JsonRpcId, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id: Some(id),
            method: method.into(),
            params: Some(params),
        }
    }

    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id: None,
            method: method.into(),
            params: Some(params),
        }
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<JsonRpcId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn result(id: Option<JsonRpcId>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<JsonRpcId>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
}

pub fn parse(payload: &str) -> Result<JsonRpcMessage, JsonRpcError> {
    let value: Value = serde_json::from_str(payload)
        .map_err(|error| JsonRpcError::new(PARSE_ERROR, format!("Parse error: {error}")))?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Err(JsonRpcError::new(INVALID_REQUEST, "Invalid Request"));
    }
    serde_json::from_value(value)
        .map_err(|error| JsonRpcError::new(INVALID_REQUEST, format!("Invalid Request: {error}")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn request_and_notification_round_trip() {
        let request = JsonRpcMessage::Request(JsonRpcRequest::call(
            JsonRpcId::Number(7),
            "tools/call",
            json!({"name": "agent_click", "arguments": {"nodeId": 42}}),
        ));
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(parse(&encoded).unwrap(), request);

        let notification = JsonRpcMessage::Request(JsonRpcRequest::notification(
            "notifications/agent/treeChanged",
            json!({"revision": 9}),
        ));
        let encoded = serde_json::to_string(&notification).unwrap();
        assert_eq!(parse(&encoded).unwrap(), notification);
    }

    #[test]
    fn malformed_or_non_jsonrpc_frames_fail_explicitly() {
        assert_eq!(parse("not-json").unwrap_err().code, PARSE_ERROR);
        assert_eq!(
            parse(r#"{"method":"tools/list"}"#).unwrap_err().code,
            INVALID_REQUEST
        );
    }
}
