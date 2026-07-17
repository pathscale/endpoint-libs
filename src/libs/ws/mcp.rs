//! JSON-RPC 2.0 envelope and MCP (Model Context Protocol) server surface.
//!
//! This module is purely additive: it defines the JSON-RPC message types, the
//! mapping from application [`ErrorCode`]s to JSON-RPC errors, and
//! [`McpState`] — precomputed MCP tool metadata built from the registered
//! endpoint handlers. The legacy `{method, seq, params}` protocol is untouched;
//! routing between the two lives in the session layer.
//!
//! MCP methods handled here: `initialize`, `ping`, `tools/list`,
//! `notifications/*` (ignored). `tools/call` is resolved to a
//! [`McpAction::ToolCall`] for the session layer to dispatch to the endpoint's
//! request handler.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::libs::error_code::ErrorCode;
use crate::libs::ws::WsEndpoint;
use crate::model::TypeRegistry;

/// MCP protocol revision implemented by this server.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelope
// ---------------------------------------------------------------------------

/// JSON-RPC reserved error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// A JSON-RPC request id: a number or a string.
///
/// A request without an id is a notification and receives no response. An
/// explicit `"id": null` is treated the same way (only ever legal in *error
/// responses* per spec, so conflating it costs nothing in practice).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Num(i64),
    Str(String),
}

impl JsonRpcId {
    fn to_value(id: &Option<JsonRpcId>) -> Value {
        match id {
            Some(JsonRpcId::Num(n)) => json!(n),
            Some(JsonRpcId::Str(s)) => json!(s),
            None => Value::Null,
        }
    }
}

/// Incoming JSON-RPC 2.0 request or notification.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<JsonRpcId>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// True when this request is a notification (no id → no response is sent).
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Serializes a JSON-RPC success response.
pub fn jsonrpc_result(id: &Option<JsonRpcId>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": JsonRpcId::to_value(id),
        "result": result,
    })
}

/// Serializes a JSON-RPC error response.
pub fn jsonrpc_error(id: &Option<JsonRpcId>, error: JsonRpcError) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": JsonRpcId::to_value(id),
        "error": error,
    })
}

/// Detects and parses a JSON-RPC frame.
///
/// Returns `Some` iff the payload is a JSON object with a top-level
/// `"jsonrpc": "2.0"` member — legacy `{method: u32, seq, params}` frames can
/// never match, so detection is unambiguous. A frame that declares
/// `"jsonrpc": "2.0"` but fails to parse as a request yields
/// `Some(Err(error))` carrying the ready-to-send error response.
pub fn try_parse_jsonrpc(payload: &str) -> Option<Result<JsonRpcRequest, Value>> {
    let value: Value = serde_json::from_str(payload).ok()?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return None;
    }
    match serde_json::from_value::<JsonRpcRequest>(value) {
        Ok(req) => Some(Ok(req)),
        Err(err) => Some(Err(jsonrpc_error(
            &None,
            JsonRpcError::new(INVALID_REQUEST, format!("Invalid Request: {err}")),
        ))),
    }
}

/// Maps an application [`ErrorCode`] (100xxx) to a JSON-RPC error.
///
/// Codes with a reserved JSON-RPC equivalent are translated; all others pass
/// through as positive application-domain codes. The original kind and code
/// are preserved in `error.data`.
pub fn error_code_to_jsonrpc(code: ErrorCode, data: Value) -> JsonRpcError {
    let (rpc_code, message) = match code {
        ErrorCode::BAD_REQUEST => (INVALID_PARAMS, "Invalid params"),
        ErrorCode::NOT_IMPLEMENTED => (METHOD_NOT_FOUND, "Method not found"),
        ErrorCode::INTERNAL_ERROR => (INTERNAL_ERROR, "Internal error"),
        other => (other.to_u32() as i64, other.kind()),
    };
    let mut err = JsonRpcError::new(rpc_code, message);
    let mut merged = json!({ "kind": code.kind(), "appCode": code.to_u32() });
    if let (Value::Object(target), Value::Object(extra)) = (&mut merged, data) {
        for (k, v) in extra {
            target.insert(k, v);
        }
    }
    err.data = Some(merged);
    err
}

// ---------------------------------------------------------------------------
// MCP tool result encoding
// ---------------------------------------------------------------------------

/// Wraps a successful handler response in the MCP `tools/call` result shape.
pub fn encode_tool_result(response: &Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": response.to_string() }],
        "structuredContent": response,
        "isError": false,
    })
}

/// Wraps a public (expected) handler error in the MCP `tools/call` result
/// shape. Per MCP, tool-level failures are results with `isError: true`, not
/// protocol errors.
pub fn encode_tool_error(code: ErrorCode, params: &Value) -> Value {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| code.kind());
    json!({
        "content": [{ "type": "text", "text": message }],
        "structuredContent": {
            "error": { "kind": code.kind(), "appCode": code.to_u32(), "params": params },
        },
        "isError": true,
    })
}

// ---------------------------------------------------------------------------
// MCP server state
// ---------------------------------------------------------------------------

/// Server identity reported in the `initialize` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
}

/// Precomputed per-endpoint tool metadata, built once at startup.
#[derive(Debug, Clone)]
pub struct McpTool {
    /// Tool name: the endpoint name in snake_case.
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    /// Key back into `WebsocketServer::handlers`.
    pub method_code: u32,
    pub allowed_roles: HashSet<u32>,
    /// Endpoints with a stream response deliver only their immediate response
    /// over MCP; stream frames are not forwarded.
    pub streaming: bool,
}

impl McpTool {
    fn to_list_entry(&self) -> Value {
        let mut entry = json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        });
        if let Some(output) = &self.output_schema {
            entry["outputSchema"] = output.clone();
        }
        entry
    }
}

/// The action the session layer should take for a routed JSON-RPC request.
#[derive(Debug)]
pub enum McpAction {
    /// A ready-to-send response frame (already serialized to a JSON value).
    Respond(Value),
    /// Dispatch `tools/call` to the endpoint handler for `method_code`.
    ToolCall {
        id: Option<JsonRpcId>,
        method_code: u32,
        arguments: Value,
    },
    /// Nothing to do (notification).
    Ignore,
}

/// Immutable MCP state attached to a [`WebsocketServer`] when MCP is enabled.
///
/// [`WebsocketServer`]: crate::libs::ws::WebsocketServer
pub struct McpState {
    pub tools: Vec<McpTool>,
    pub by_name: HashMap<String, usize>,
    pub info: McpServerInfo,
    pub protocol_version: &'static str,
}

impl McpState {
    /// Builds MCP tool metadata from the registered handlers.
    ///
    /// Fails on unresolved type references (see
    /// [`Type::to_json_schema`](crate::model::Type::to_json_schema)) and on
    /// duplicate tool names, so misconfiguration surfaces at startup.
    pub fn build(
        handlers: &HashMap<u32, WsEndpoint>,
        registry: &TypeRegistry,
        info: McpServerInfo,
    ) -> eyre::Result<Self> {
        let mut tools = Vec::with_capacity(handlers.len());
        let mut by_name = HashMap::with_capacity(handlers.len());

        // Sort by code for a deterministic tools/list order.
        let mut endpoints: Vec<&WsEndpoint> = handlers.values().collect();
        endpoints.sort_by_key(|e| e.schema.code);

        for endpoint in endpoints {
            let schema = &endpoint.schema;
            let name = schema.tool_name();
            let streaming = schema.stream_response.is_some();

            let mut description = schema.description.clone();
            if streaming {
                if !description.is_empty() {
                    description.push(' ');
                }
                description
                    .push_str("(Streaming endpoint: stream frames are not delivered over MCP.)");
            }

            let input_schema = schema.to_mcp_input_schema(registry)?;
            let output_schema = if schema.returns.is_empty() {
                None
            } else {
                Some(schema.to_mcp_output_schema(registry)?)
            };

            let index = tools.len();
            if let Some(prev) = by_name.insert(name.clone(), index) {
                let prev: &McpTool = &tools[prev];
                eyre::bail!(
                    "duplicate MCP tool name {name} (endpoint codes {} and {})",
                    prev.method_code,
                    schema.code
                );
            }
            tools.push(McpTool {
                name,
                description,
                input_schema,
                output_schema,
                method_code: schema.code,
                allowed_roles: endpoint.allowed_roles.clone(),
                streaming,
            });
        }

        Ok(Self {
            tools,
            by_name,
            info,
            protocol_version: MCP_PROTOCOL_VERSION,
        })
    }

    fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": self.protocol_version,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": self.info.name, "version": self.info.version },
        })
    }

    fn tools_list_result(&self, roles: &[u32]) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .filter(|tool| roles_allowed(roles, &tool.allowed_roles))
            .map(McpTool::to_list_entry)
            .collect();
        json!({ "tools": tools })
    }

    /// Routes a parsed JSON-RPC request to an [`McpAction`].
    ///
    /// `roles` are the connection's current roles; `tools/list` is filtered by
    /// them and `tools/call` on a forbidden or unknown tool uniformly reports
    /// "unknown tool" (so unauthorized callers cannot probe for existence).
    pub fn route(&self, req: JsonRpcRequest, roles: &[u32]) -> McpAction {
        match req.method.as_str() {
            "initialize" => McpAction::Respond(jsonrpc_result(&req.id, self.initialize_result())),
            "ping" => McpAction::Respond(jsonrpc_result(&req.id, json!({}))),
            method if method.starts_with("notifications/") => McpAction::Ignore,
            "tools/list" => {
                McpAction::Respond(jsonrpc_result(&req.id, self.tools_list_result(roles)))
            }
            "tools/call" => self.route_tool_call(req, roles),
            _ if req.is_notification() => McpAction::Ignore,
            other => McpAction::Respond(jsonrpc_error(
                &req.id,
                JsonRpcError::new(METHOD_NOT_FOUND, format!("Method not found: {other}")),
            )),
        }
    }

    fn route_tool_call(&self, req: JsonRpcRequest, roles: &[u32]) -> McpAction {
        let params = req.params.unwrap_or(Value::Null);
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return McpAction::Respond(jsonrpc_error(
                &req.id,
                JsonRpcError::new(INVALID_PARAMS, "tools/call requires a string `name` param"),
            ));
        };

        let tool = self.by_name.get(name).map(|&i| &self.tools[i]);
        let allowed = tool.is_some_and(|t| roles_allowed(roles, &t.allowed_roles));
        let (Some(tool), true) = (tool, allowed) else {
            return McpAction::Respond(jsonrpc_error(
                &req.id,
                JsonRpcError::new(INVALID_PARAMS, format!("Unknown tool: {name}")),
            ));
        };

        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        McpAction::ToolCall {
            id: req.id,
            method_code: tool.method_code,
            arguments,
        }
    }
}

/// Same semantics as the legacy role check: allowed iff both sets are
/// non-empty and intersect.
fn roles_allowed(actual_roles: &[u32], allowed_roles: &HashSet<u32>) -> bool {
    if allowed_roles.is_empty() || actual_roles.is_empty() {
        return false;
    }
    actual_roles.iter().any(|role| allowed_roles.contains(role))
}

/// Per-call MCP context threaded through handler dispatch so the response can
/// be encoded as a `tools/call` result addressed to the originating id.
#[derive(Debug, Clone)]
pub struct McpCallCtx {
    pub id: Option<JsonRpcId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::handler::RequestHandlerErased;
    use crate::libs::toolbox::{ArcToolbox, RequestContext};
    use crate::model::{EndpointSchema, Field, Type};
    use async_trait::async_trait;
    use std::sync::Arc;

    struct NoopHandler;
    #[async_trait(?Send)]
    impl RequestHandlerErased for NoopHandler {
        async fn handle(&self, _toolbox: &ArcToolbox, _ctx: RequestContext, _req: Value) {}
    }

    fn endpoint(name: &str, code: u32, roles: &[u32]) -> WsEndpoint {
        let schema = EndpointSchema::new(
            name,
            code,
            vec![Field::new("user_id", Type::Int64)],
            vec![Field::new("ok", Type::Boolean)],
        )
        .with_description(format!("{name} endpoint"));
        WsEndpoint {
            schema,
            handler: Arc::new(NoopHandler),
            allowed_roles: roles.iter().cloned().collect(),
        }
    }

    fn state() -> McpState {
        let mut handlers = HashMap::new();
        handlers.insert(10020, endpoint("UserListSymbols", 10020, &[1, 2]));
        handlers.insert(10030, endpoint("AdminBanUser", 10030, &[9]));
        McpState::build(
            &handlers,
            &TypeRegistry::new(),
            McpServerInfo {
                name: "test-server".into(),
                version: "1.0.0".into(),
            },
        )
        .unwrap()
    }

    fn parse(payload: &str) -> JsonRpcRequest {
        try_parse_jsonrpc(payload).unwrap().unwrap()
    }

    #[test]
    fn detection_ignores_legacy_and_non_json() {
        assert!(try_parse_jsonrpc(r#"{"method":10020,"seq":1,"params":{}}"#).is_none());
        assert!(try_parse_jsonrpc("not json").is_none());
        assert!(try_parse_jsonrpc(r#"{"jsonrpc":"1.0","method":"x"}"#).is_none());
        assert!(try_parse_jsonrpc(r#"{"jsonrpc":"2.0","method":"ping","id":1}"#).is_some());
    }

    #[test]
    fn malformed_jsonrpc_yields_error_frame() {
        // Declares jsonrpc 2.0 but method is missing.
        let result = try_parse_jsonrpc(r#"{"jsonrpc":"2.0","id":1}"#).unwrap();
        let err = result.unwrap_err();
        assert_eq!(err["error"]["code"], json!(INVALID_REQUEST));
        assert_eq!(err["id"], Value::Null);
    }

    #[test]
    fn id_round_trip() {
        let req = parse(r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#);
        assert_eq!(req.id, Some(JsonRpcId::Str("abc".into())));
        let req = parse(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#);
        assert_eq!(req.id, Some(JsonRpcId::Num(7)));
        let req = parse(r#"{"jsonrpc":"2.0","method":"ping"}"#);
        assert!(req.is_notification());
        let req = parse(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#);
        assert!(req.is_notification());
    }

    #[test]
    fn error_code_mapping() {
        let err = error_code_to_jsonrpc(ErrorCode::BAD_REQUEST, json!({}));
        assert_eq!(err.code, INVALID_PARAMS);
        let err = error_code_to_jsonrpc(ErrorCode::NOT_IMPLEMENTED, json!({}));
        assert_eq!(err.code, METHOD_NOT_FOUND);
        let err = error_code_to_jsonrpc(ErrorCode::INTERNAL_ERROR, json!({}));
        assert_eq!(err.code, INTERNAL_ERROR);
        // Non-reserved codes pass through as positive app codes.
        let err = error_code_to_jsonrpc(ErrorCode::FORBIDDEN, json!({ "message": "no" }));
        assert_eq!(err.code, 100403);
        let data = err.data.unwrap();
        assert_eq!(data["kind"], json!("Forbidden"));
        assert_eq!(data["appCode"], json!(100403));
        assert_eq!(data["message"], json!("no"));
    }

    #[test]
    fn initialize_golden() {
        let state = state();
        let req = parse(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#);
        let McpAction::Respond(resp) = state.route(req, &[]) else {
            panic!("expected response");
        };
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 0,
                "result": {
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "test-server", "version": "1.0.0" },
                },
            })
        );
    }

    #[test]
    fn tools_list_filters_by_role() {
        let state = state();
        let req = parse(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let McpAction::Respond(resp) = state.route(req, &[1]) else {
            panic!("expected response");
        };
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("user_list_symbols"));
        assert_eq!(tools[0]["description"], json!("UserListSymbols endpoint"));
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["userId"]));
        assert!(tools[0]["outputSchema"].is_object());

        // No roles → empty list.
        let req = parse(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let McpAction::Respond(resp) = state.route(req, &[]) else {
            panic!("expected response");
        };
        assert_eq!(resp["result"]["tools"], json!([]));
    }

    #[test]
    fn tools_call_routes_to_handler() {
        let state = state();
        let req = parse(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call",
               "params":{"name":"user_list_symbols","arguments":{"userId":1}}}"#,
        );
        let McpAction::ToolCall {
            id,
            method_code,
            arguments,
        } = state.route(req, &[1])
        else {
            panic!("expected tool call");
        };
        assert_eq!(id, Some(JsonRpcId::Num(5)));
        assert_eq!(method_code, 10020);
        assert_eq!(arguments, json!({ "userId": 1 }));
    }

    #[test]
    fn tools_call_unknown_and_forbidden_are_uniform() {
        let state = state();
        let unknown =
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"nope"}}"#);
        let McpAction::Respond(unknown) = state.route(unknown, &[1]) else {
            panic!("expected response");
        };
        // admin_ban_user exists but requires role 9; caller has role 1.
        let forbidden = parse(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"admin_ban_user"}}"#,
        );
        let McpAction::Respond(forbidden) = state.route(forbidden, &[1]) else {
            panic!("expected response");
        };
        assert_eq!(unknown["error"]["code"], forbidden["error"]["code"]);
        assert_eq!(unknown["error"]["code"], json!(INVALID_PARAMS));
        // Same shape modulo the tool name — no existence oracle.
        assert!(
            forbidden["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("Unknown tool")
        );
    }

    #[test]
    fn notifications_are_ignored() {
        let state = state();
        let req = parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert!(matches!(state.route(req, &[1]), McpAction::Ignore));
        // Unknown method as a notification → still no response.
        let req = parse(r#"{"jsonrpc":"2.0","method":"bogus"}"#);
        assert!(matches!(state.route(req, &[1]), McpAction::Ignore));
        // Unknown method with an id → METHOD_NOT_FOUND.
        let req = parse(r#"{"jsonrpc":"2.0","id":1,"method":"bogus"}"#);
        let McpAction::Respond(resp) = state.route(req, &[1]) else {
            panic!("expected response");
        };
        assert_eq!(resp["error"]["code"], json!(METHOD_NOT_FOUND));
    }

    #[test]
    fn tool_result_encoding() {
        let ok = encode_tool_result(&json!({ "ok": true }));
        assert_eq!(ok["isError"], json!(false));
        assert_eq!(ok["structuredContent"], json!({ "ok": true }));
        assert_eq!(ok["content"][0]["type"], json!("text"));

        let err = encode_tool_error(
            ErrorCode::CONFLICT,
            &json!({ "kind": "UsernameTaken", "message": "username taken" }),
        );
        assert_eq!(err["isError"], json!(true));
        assert_eq!(err["content"][0]["text"], json!("username taken"));
        assert_eq!(err["structuredContent"]["error"]["kind"], json!("Conflict"));
    }

    #[test]
    fn duplicate_tool_names_rejected() {
        let mut handlers = HashMap::new();
        // Same name, different codes → same snake_case tool name.
        handlers.insert(1, endpoint("UserGetInfo", 1, &[1]));
        handlers.insert(2, endpoint("UserGetInfo", 2, &[1]));
        let result = McpState::build(
            &handlers,
            &TypeRegistry::new(),
            McpServerInfo {
                name: "x".into(),
                version: "0".into(),
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn streaming_endpoints_are_annotated() {
        let mut handlers = HashMap::new();
        let mut ep = endpoint("PriceWatch", 1, &[1]);
        ep.schema.stream_response = Some(Type::Float64);
        handlers.insert(1, ep);
        let state = McpState::build(
            &handlers,
            &TypeRegistry::new(),
            McpServerInfo {
                name: "x".into(),
                version: "0".into(),
            },
        )
        .unwrap();
        assert!(state.tools[0].streaming);
        assert!(
            state.tools[0]
                .description
                .contains("not delivered over MCP")
        );
    }
}
