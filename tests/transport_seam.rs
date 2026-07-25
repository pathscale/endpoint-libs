//! The acceptance test for 2.0's core claim: the session/dispatch/MCP machinery runs
//! over a transport that is not a WebSocket and never touches a TCP socket.
//!
//! Both halves of the seam are exercised by the same test — the server through
//! [`WebsocketServer::serve_connection`], the client through
//! [`WsClient::from_stream`] — over an in-memory `tokio::io::duplex` pipe framed with
//! [`framed_json`].
//!
//! If this file fails to compile, the transport seam has regressed.

#![cfg(all(feature = "framed-transport", feature = "ws-client"))]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use endpoint_libs::libs::handler::{RequestHandler, Response};
use endpoint_libs::libs::peer::{Attestation, LocalPeer, PeerIdentity};
use endpoint_libs::libs::toolbox::{ArcToolbox, CustomError, RequestContext};
use endpoint_libs::libs::ws::transport::{TransportStream, framed_json};
use endpoint_libs::libs::ws::{
    AuthController, MessageStream, WebsocketServer, WebsocketStates, WsClient, WsConnection,
    WsRequest, WsResponse, WsServerConfig,
};
use eyre::Result;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use serde::{Deserialize, Serialize};
use tokio::task::LocalSet;

// --- A real endpoint, registered the ordinary way -------------------------

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

struct MethodEcho;

#[async_trait(?Send)]
impl RequestHandler for MethodEcho {
    type Request = EchoRequest;
    type Error = CustomError;

    async fn handle(&self, ctx: RequestContext, req: EchoRequest) -> Response<EchoRequest> {
        // Proves the attested peer identity reaches handler code — the whole point
        // of threading PeerIdentity through in Phase 2.
        let peer = match &ctx.peer {
            // Attestation is #[non_exhaustive] (Phase 2b), so an out-of-crate match
            // needs a wildcard — future mechanisms must not break this test.
            PeerIdentity::Local(local) => match &local.attestation {
                Attestation::Verified { mechanism, .. } => format!("local/{mechanism}"),
                Attestation::None => "local/unattested".to_owned(),
                _ => "local/unknown-attestation".to_owned(),
            },
            PeerIdentity::Network(_) => "network".to_owned(),
            _ => "unknown".to_owned(),
        };
        Ok(EchoResponse {
            message: format!("echo[{peer}]: {}", req.message),
        })
    }
}

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

fn build_server(enable_mcp: bool) -> WebsocketServer {
    let config = WsServerConfig {
        insecure: true,
        ..Default::default()
    };
    let mut server = WebsocketServer::new(config);
    server.set_auth_controller(AllowAllAuthController);
    server.add_handler(MethodEcho);
    if enable_mcp {
        let mut registry = endpoint_libs::model::TypeRegistry::new();
        let schema: endpoint_libs::model::EndpointSchema =
            serde_json::from_str(EchoRequest::SCHEMA).unwrap();
        registry.add_endpoint(&schema);
        server
            .enable_mcp(
                &registry,
                endpoint_libs::libs::ws::mcp::McpServerInfo {
                    name: "seam-test".into(),
                    version: "0.0.0".into(),
                },
            )
            .expect("enable_mcp");
    }
    server
}

/// An attested local peer, as a platform transport would report one.
fn attested_peer() -> PeerIdentity {
    PeerIdentity::Local(LocalPeer {
        pid: Some(std::process::id()),
        uid: None,
        attestation: Attestation::Verified {
            mechanism: "test-harness",
            subject: "acceptance".to_owned(),
        },
    })
}

fn spawn_server(
    server: WebsocketServer,
    server_io: tokio::io::DuplexStream,
) -> impl std::future::Future<Output = ()> {
    let server = Arc::new(server);
    let states = Arc::new(WebsocketStates::new());
    server.toolbox.set_ws_states(states.clone_states(), false, false);
    let stream: Box<dyn MessageStream> = Box::new(TransportStream::new(framed_json(server_io)));
    server.serve_connection(attested_peer(), states, stream, None)
}

/// (a) A legacy `{method, seq, params}` request reaches a real registered handler and
/// its response comes back — over a duplex pipe, with no TCP socket anywhere.
#[tokio::test(flavor = "current_thread")]
async fn legacy_request_round_trips_over_a_non_websocket_transport() {
    let local = LocalSet::new();
    local
        .run_until(async {
            let (server_io, client_io) = tokio::io::duplex(256 * 1024);

            tokio::task::spawn_local(spawn_server(build_server(false), server_io));

            let client_stream: Box<dyn MessageStream> =
                Box::new(TransportStream::new(framed_json(client_io)));
            let mut client = WsClient::from_stream(client_stream);

            let resp: EchoResponse = tokio::time::timeout(
                Duration::from_secs(5),
                client.request(EchoRequest {
                    message: "hello".into(),
                }),
            )
            .await
            .expect("request timed out")
            .expect("request failed");

            // The handler saw the attestation the transport supplied.
            assert_eq!(resp.message, "echo[local/test-harness]: hello");
        })
        .await;
}

/// (b) MCP `initialize` → `tools/list` → `tools/call` completes on the *same*
/// connection type, proving the JSON-RPC surface is not tied to WebSockets either.
#[tokio::test(flavor = "current_thread")]
async fn mcp_initialize_and_tool_call_work_over_the_same_transport() {
    let local = LocalSet::new();
    local
        .run_until(async {
            let (server_io, client_io) = tokio::io::duplex(256 * 1024);

            tokio::task::spawn_local(spawn_server(build_server(true), server_io));

            let client_stream: Box<dyn MessageStream> =
                Box::new(TransportStream::new(framed_json(client_io)));
            let mut client = WsClient::from_stream(client_stream);

            let initialize = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                           "clientInfo": {"name": "seam-test", "version": "0.0.0"}}
            });
            client
                .send_raw(initialize.to_string().as_bytes())
                .await
                .expect("send initialize");
            let resp = tokio::time::timeout(Duration::from_secs(5), client.recv_raw())
                .await
                .expect("initialize timed out")
                .expect("initialize failed");
            assert_eq!(resp["id"], 1, "initialize response: {resp}");
            assert!(
                resp["result"]["serverInfo"]["name"] == "seam-test",
                "unexpected initialize result: {resp}"
            );

            let list = serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
            });
            client
                .send_raw(list.to_string().as_bytes())
                .await
                .expect("send tools/list");
            let resp = tokio::time::timeout(Duration::from_secs(5), client.recv_raw())
                .await
                .expect("tools/list timed out")
                .expect("tools/list failed");
            let tools = resp["result"]["tools"]
                .as_array()
                .unwrap_or_else(|| panic!("no tools array in {resp}"));
            assert_eq!(tools.len(), 1, "expected exactly the echo tool: {resp}");
            assert_eq!(tools[0]["name"], "echo");

            let call = serde_json::json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "echo", "arguments": {"message": "via-mcp"}}
            });
            client
                .send_raw(call.to_string().as_bytes())
                .await
                .expect("send tools/call");
            let resp = tokio::time::timeout(Duration::from_secs(5), client.recv_raw())
                .await
                .expect("tools/call timed out")
                .expect("tools/call failed");
            assert_eq!(resp["id"], 3, "tools/call response: {resp}");
            let text = resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("no text content in {resp}"));
            assert!(
                text.contains("echo[local/test-harness]: via-mcp"),
                "tool call did not reach the handler: {text}"
            );
        })
        .await;
}
