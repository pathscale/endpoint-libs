//! MCP echo example and end-to-end check.
//!
//! Starts a WebSocket server with a single `Echo` endpoint and the MCP surface
//! enabled, then connects a client and exercises both protocols on the same
//! connection:
//!
//! 1. MCP: `initialize` → `notifications/initialized` → `tools/list` →
//!    `tools/call` (`echo`)
//! 2. Legacy: a `{method, seq, params}` frame, verifying the pre-existing
//!    protocol still works unchanged next to MCP.
//!
//! Run with:
//! ```sh
//! cargo run --example mcp_echo --features ws-http1
//! ```
//! The process asserts on every response and exits 0 on success.

use std::sync::Arc;

use async_trait::async_trait;
use endpoint_libs::libs::handler::{RequestHandler, Response};
use endpoint_libs::libs::log::{LogLevel, LoggingConfig, OtelConfig, setup_logging};
use endpoint_libs::libs::toolbox::{ArcToolbox, RequestContext};
use endpoint_libs::libs::ws::mcp::McpServerInfo;
use endpoint_libs::libs::ws::toolbox::CustomError;
use endpoint_libs::libs::ws::{
    AuthController, WebsocketServer, WsConnection, WsRequest, WsResponse, WsServerConfig,
};
use endpoint_libs::model::TypeRegistry;
use eyre::{Result, bail};
use futures::future::LocalBoxFuture;
use futures::{FutureExt, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

const ADDR: &str = "127.0.0.1:8899";

// --- Echo endpoint (same conventions as the ws-echo example) ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EchoRequest {
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EchoResponse {
    pub message: String,
}

impl WsRequest for EchoRequest {
    type Response = EchoResponse;
    const METHOD_ID: u32 = 1;
    const ROLES: &'static [u32] = &[1];
    const SCHEMA: &'static str = r#"{
        "name":        "Echo",
        "code":        1,
        "parameters":  [{"name": "message", "ty": "String"}],
        "returns":     [{"name": "message", "ty": "String"}],
        "description": "Echoes the message back.",
        "roles":       []
    }"#;
}

impl WsResponse for EchoResponse {
    type Request = EchoRequest;
}

pub struct MethodEcho;

#[async_trait(?Send)]
impl RequestHandler for MethodEcho {
    type Request = EchoRequest;
    type Error = CustomError;

    async fn handle(&self, _ctx: RequestContext, req: EchoRequest) -> Response<EchoRequest> {
        Ok(EchoResponse {
            message: format!("echo: {}", req.message),
        })
    }
}

// --- Allow-all auth: grants role 1 so the Echo tool is visible/callable ---

struct AllowAllAuthController;

impl AuthController for AllowAllAuthController {
    fn auth(
        self: Arc<Self>,
        _toolbox: &ArcToolbox,
        _header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            conn.set_roles(Arc::new(vec![1]));
            Ok(())
        }
        .boxed_local()
    }
}

// --- Client helpers ---

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send_json(ws: &mut WsClient, frame: Value) -> Result<()> {
    ws.send(Message::Text(frame.to_string().into())).await?;
    Ok(())
}

async fn recv_json(ws: &mut WsClient) -> Result<Value> {
    loop {
        let Some(msg) = ws.next().await else {
            bail!("connection closed while waiting for a response");
        };
        match msg? {
            Message::Text(t) => return Ok(serde_json::from_str(&t)?),
            Message::Binary(b) => return Ok(serde_json::from_slice(&b)?),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => bail!("unexpected frame: {other:?}"),
        }
    }
}

fn expect(cond: bool, what: &str) -> Result<()> {
    if cond {
        println!("ok: {what}");
        Ok(())
    } else {
        bail!("FAILED: {what}");
    }
}

async fn run_client() -> Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{ADDR}")).await?;

    // 1. initialize
    send_json(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "mcp-echo-client", "version": "0.0.0"},
        }}),
    )
    .await?;
    let resp = recv_json(&mut ws).await?;
    expect(resp["id"] == json!(0), "initialize: id echoed")?;
    expect(
        resp["result"]["serverInfo"]["name"] == json!("mcp-echo"),
        "initialize: serverInfo.name",
    )?;
    expect(
        resp["result"]["capabilities"]["tools"].is_object(),
        "initialize: tools capability",
    )?;

    // 2. notifications/initialized — a notification, must produce no response
    send_json(
        &mut ws,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await?;

    // 3. tools/list
    send_json(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await?;
    let resp = recv_json(&mut ws).await?;
    let tools = resp["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    expect(tools.len() == 1, "tools/list: one tool")?;
    expect(tools[0]["name"] == json!("echo"), "tools/list: tool name")?;
    expect(
        tools[0]["inputSchema"]["required"] == json!(["message"]),
        "tools/list: inputSchema.required",
    )?;

    // 4. tools/call
    send_json(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "echo",
            "arguments": {"message": "hello mcp"},
        }}),
    )
    .await?;
    let resp = recv_json(&mut ws).await?;
    expect(resp["id"] == json!(2), "tools/call: id echoed")?;
    expect(
        resp["result"]["isError"] == json!(false),
        "tools/call: not an error",
    )?;
    expect(
        resp["result"]["structuredContent"]["message"] == json!("echo: hello mcp"),
        "tools/call: structuredContent",
    )?;

    // 5. tools/call with bad arguments → JSON-RPC invalid params (-32602)
    send_json(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "echo",
            "arguments": {"message": 42},
        }}),
    )
    .await?;
    let resp = recv_json(&mut ws).await?;
    expect(
        resp["error"]["code"] == json!(-32602),
        "tools/call: invalid params -> -32602",
    )?;

    // 6. Legacy protocol on the same connection — must be unaffected by MCP.
    send_json(
        &mut ws,
        json!({"method": 1, "seq": 42, "params": {"message": "legacy"}}),
    )
    .await?;
    let resp = recv_json(&mut ws).await?;
    expect(
        resp["type"] == json!("Immediate"),
        "legacy: Immediate response",
    )?;
    expect(resp["seq"] == json!(42), "legacy: seq echoed")?;
    expect(
        resp["params"]["message"] == json!("echo: legacy"),
        "legacy: echo payload",
    )?;

    ws.close(None).await.ok();
    Ok(())
}

// --- Entry point ---

fn main() -> Result<()> {
    let _log = setup_logging(LoggingConfig {
        level: LogLevel::Info,
        otel_config: OtelConfig::default(),
        file_config: None,
        #[cfg(feature = "error_aggregation")]
        error_aggregation: Default::default(),
        #[cfg(feature = "log_throttling")]
        throttling_config: None,
    })?;

    // Server on its own runtime/thread; listen() runs until killed.
    std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        rt.block_on(async {
            let config = WsServerConfig {
                name: "mcp-echo".to_string(),
                address: ADDR.to_string(),
                insecure: true,
                ..Default::default()
            };
            let mut server = WebsocketServer::new(config);
            server.set_auth_controller(AllowAllAuthController);
            server.add_handler(MethodEcho);

            // The Echo endpoint only uses primitives, so an empty registry
            // suffices; endpoints with StructRef/EnumRef parameters register
            // their referenced types here.
            let registry = TypeRegistry::new();
            server
                .enable_mcp(
                    &registry,
                    McpServerInfo {
                        name: "mcp-echo".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                    },
                )
                .expect("enable_mcp");

            server.listen().await.expect("server listen");
        });
    });

    // Client on the main runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        // Wait for the server to come up.
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(ADDR).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        run_client().await
    })?;

    println!("all checks passed");
    std::process::exit(0);
}
