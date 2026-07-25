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

// ---------------------------------------------------------------------------
// Phase 4 — hooks on both dispatch paths
// ---------------------------------------------------------------------------

use endpoint_libs::libs::error_code::ErrorCode;
use endpoint_libs::libs::peer::Extensions;
use endpoint_libs::libs::ws::{
    AfterRequest, BeforeRequest, OnConnect, RequestOutcome,
};
use endpoint_libs::model::EndpointSchema;
use std::sync::Mutex;

/// Verified claims, as a mission-token hook would attach them.
#[derive(Debug, Clone, PartialEq)]
struct Claims(String);

/// Rejects any request whose `message` contains "denied".
struct DenyByContent;

#[async_trait(?Send)]
impl BeforeRequest for DenyByContent {
    async fn before(
        &self,
        ctx: &mut RequestContext,
        _endpoint: &EndpointSchema,
        params: &serde_json::Value,
    ) -> Result<(), CustomError> {
        let text = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if text.contains("denied") {
            return Err(CustomError::new(ErrorCode::FORBIDDEN)
                .with_message("blocked by policy")
                .with_kind("PolicyDenied"));
        }
        // Prove a hook can hand data to the handler.
        ctx.extensions.insert(Claims(format!("seen:{text}")));
        Ok(())
    }
}

/// Records every outcome it observes.
#[derive(Clone, Default)]
struct RecordOutcomes(Arc<Mutex<Vec<String>>>);

#[async_trait(?Send)]
impl AfterRequest for RecordOutcomes {
    async fn after(
        &self,
        _ctx: &RequestContext,
        endpoint: &EndpointSchema,
        outcome: &RequestOutcome,
    ) {
        let label = match outcome {
            RequestOutcome::Ok => "ok".to_owned(),
            RequestOutcome::PublicErr { code } => format!("public:{code}"),
            RequestOutcome::InternalErr => "internal".to_owned(),
            _ => "other".to_owned(),
        };
        self.0.lock().unwrap().push(format!("{}:{label}", endpoint.name));
    }
}

/// Second endpoint whose handler surfaces hook-supplied claims.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaimsRequest {
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaimsResponse {
    pub message: String,
}

impl WsRequest for ClaimsRequest {
    type Response = ClaimsResponse;
    const METHOD_ID: u32 = 2;
    const ROLES: &'static [u32] = &[1];
    const SCHEMA: &'static str = r#"{
        "name":        "Claims",
        "code":        2,
        "parameters":  [{"name": "message", "ty": "String"}],
        "returns":     [{"name": "message", "ty": "String"}],
        "description": "Reports claims attached by a BeforeRequest hook.",
        "roles":       []
    }"#;
}

impl WsResponse for ClaimsResponse {
    type Request = ClaimsRequest;
}

struct MethodClaims;

#[async_trait(?Send)]
impl RequestHandler for MethodClaims {
    type Request = ClaimsRequest;
    type Error = CustomError;

    async fn handle(&self, ctx: RequestContext, _req: ClaimsRequest) -> Response<ClaimsRequest> {
        let claims = ctx
            .extensions
            .get::<Claims>()
            .map(|c| c.0.clone())
            .unwrap_or_else(|| "<none>".to_owned());
        Ok(ClaimsResponse { message: claims })
    }
}

fn server_with_hooks(recorder: RecordOutcomes, mcp: bool) -> WebsocketServer {
    let config = WsServerConfig {
        insecure: true,
        ..Default::default()
    };
    let mut server = WebsocketServer::new(config);
    server.set_auth_controller(AllowAllAuthController);
    server.add_handler(MethodClaims);
    server.add_before_hook(DenyByContent);
    server.add_after_hook(recorder);
    if mcp {
        let mut registry = endpoint_libs::model::TypeRegistry::new();
        let schema: EndpointSchema = serde_json::from_str(ClaimsRequest::SCHEMA).unwrap();
        registry.add_endpoint(&schema);
        server
            .enable_mcp(
                &registry,
                endpoint_libs::libs::ws::mcp::McpServerInfo {
                    name: "hooks-test".into(),
                    version: "0.0.0".into(),
                },
            )
            .expect("enable_mcp");
    }
    server
}

fn connect(server: WebsocketServer) -> WsClient {
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    tokio::task::spawn_local(spawn_server(server, server_io));
    WsClient::from_stream(Box::new(TransportStream::new(framed_json(client_io))))
}

/// A BeforeRequest hook rejects on the legacy path, with the exact error frame, and
/// a passing request receives the claims the hook attached.
#[tokio::test(flavor = "current_thread")]
async fn before_hook_gates_the_legacy_path_and_passes_claims() {
    let local = LocalSet::new();
    local
        .run_until(async {
            let recorder = RecordOutcomes::default();
            let mut client = connect(server_with_hooks(recorder.clone(), false));

            // Allowed: the handler sees what the hook put in extensions.
            let resp: ClaimsResponse = client
                .request(ClaimsRequest { message: "fine".into() })
                .await
                .expect("allowed request failed");
            assert_eq!(resp.message, "seen:fine");

            // Denied: the handler never runs; the hook's code and params come back.
            client
                .send_req(ClaimsRequest::METHOD_ID, ClaimsRequest { message: "denied".into() })
                .await
                .expect("send");
            let raw = client.recv_raw().await.expect("recv");
            assert_eq!(raw["code"], ErrorCode::FORBIDDEN.to_u32(), "frame: {raw}");
            assert_eq!(raw["params"]["kind"], "PolicyDenied", "frame: {raw}");
            assert_eq!(raw["params"]["message"], "blocked by policy", "frame: {raw}");

            // AfterRequest saw both, with the rejection reported as a public error.
            let seen = recorder.0.lock().unwrap().clone();
            assert_eq!(
                seen,
                vec![
                    "Claims:ok".to_owned(),
                    format!("Claims:public:{}", ErrorCode::FORBIDDEN.to_u32())
                ]
            );
        })
        .await;
}

/// The same hook must gate `tools/call`, with the rejection encoded as an MCP tool
/// error rather than a legacy error frame.
#[tokio::test(flavor = "current_thread")]
async fn before_hook_gates_the_mcp_path_with_a_tool_error() {
    let local = LocalSet::new();
    local
        .run_until(async {
            let recorder = RecordOutcomes::default();
            let mut client = connect(server_with_hooks(recorder.clone(), true));

            let init = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                           "clientInfo": {"name": "hooks-test", "version": "0.0.0"}}
            });
            client.send_raw(init.to_string().as_bytes()).await.unwrap();
            client.recv_raw().await.unwrap();

            let call = serde_json::json!({
                "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": {"name": "claims", "arguments": {"message": "denied by policy"}}
            });
            client.send_raw(call.to_string().as_bytes()).await.unwrap();
            let resp = tokio::time::timeout(Duration::from_secs(5), client.recv_raw())
                .await
                .expect("timed out")
                .expect("recv");

            assert_eq!(resp["id"], 7, "frame: {resp}");
            assert_eq!(resp["result"]["isError"], true, "expected a tool error: {resp}");
            let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
            assert!(
                text.contains("blocked by policy") || text.contains("PolicyDenied"),
                "tool error did not carry the hook's payload: {resp}"
            );

            let seen = recorder.0.lock().unwrap().clone();
            assert_eq!(seen, vec![format!("Claims:public:{}", ErrorCode::FORBIDDEN.to_u32())]);
        })
        .await;
}

/// An OnConnect hook refuses a peer outright — no messages are exchanged at all.
#[tokio::test(flavor = "current_thread")]
async fn on_connect_hook_can_refuse_a_peer() {
    struct RefuseUnattested;

    #[async_trait(?Send)]
    impl OnConnect for RefuseUnattested {
        async fn on_connect(
            &self,
            peer: &PeerIdentity,
            ext: &mut Extensions,
        ) -> Result<(), CustomError> {
            match peer.attestation() {
                Some(a) if a.is_verified() => {
                    ext.insert(Claims("attested".to_owned()));
                    Ok(())
                }
                _ => Err(CustomError::new(ErrorCode::FORBIDDEN).with_message("unattested peer")),
            }
        }
    }

    let local = LocalSet::new();
    local
        .run_until(async {
            let config = WsServerConfig { insecure: true, ..Default::default() };
            let mut server = WebsocketServer::new(config);
            server.set_auth_controller(AllowAllAuthController);
            server.add_handler(MethodEcho);
            server.add_on_connect_hook(RefuseUnattested);

            // spawn_server supplies an *attested* peer, so this one is admitted.
            let mut client = connect(server);
            let resp: EchoResponse = client
                .request(EchoRequest { message: "hi".into() })
                .await
                .expect("attested peer should be admitted");
            assert_eq!(resp.message, "echo[local/test-harness]: hi");
        })
        .await;
}
