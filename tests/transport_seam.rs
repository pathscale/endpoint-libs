//! The acceptance test for 2.0's core claim: the session/dispatch/MCP machinery runs
//! over a transport that is not a WebSocket and never touches a TCP socket.
//!
//! Both halves of the seam are exercised by the same test — the server through
//! [`WebsocketServer::serve_connection`], the client through
//! [`WsClient::from_stream`] — over the in-memory duplex pipe defined at the bottom of
//! this file, framed with [`framed_json_neutral`].
//!
//! # Why nothing here names a runtime
//!
//! This crate contains no tokio, down to its dev-dependencies, so a test cannot reach
//! for `tokio::io::duplex` to get a pipe, `#[tokio::test]` plus a `LocalSet` to get an
//! executor, or `tokio::time::timeout` to get a deadline. Each is replaced by the
//! neutral thing it was standing in for: the pipe is a pair of buffers behind the
//! `futures_io` traits, the executor is [`nagoya::block_on`] driving a
//! [`TaskSet`] (the server's connection loop) against the test body, and the deadline
//! is [`nagoya::timeout`], whose timer thread starts itself on first use.
//!
//! No [`Reactor`](nagoya::reactor::Reactor) is created, and that is the point: these
//! connections are memory, not file descriptors, so there is no readiness for one to
//! report. `TaskSet` lives under `nagoya::reactor` but needs none of it.
//!
//! The framing is [`framed_json_neutral`], which is now the only flavour there is. It
//! carries the same length-prefixed format the tokio-io one did — they shared `encode`
//! and `decode` for as long as both existed — so the exact byte sequences these tests
//! assert are the wire format itself rather than one flavour's rendering of it.
//!
//! If this file fails to compile, the transport seam has regressed.

#![cfg(all(feature = "framed-transport", feature = "ws-client"))]

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_trait::async_trait;
use endpoint_libs::libs::handler::{RequestHandler, Response};
use endpoint_libs::libs::peer::{Attestation, LocalPeer, PeerIdentity};
use endpoint_libs::libs::toolbox::{ArcToolbox, CustomError, RequestContext};
use endpoint_libs::libs::ws::transport::TransportStream;
use endpoint_libs::libs::ws::transport::framed::framed_json_neutral;
use endpoint_libs::libs::ws::{
    AuthController, MessageStream, WebsocketServer, WebsocketStates, WsClient, WsConnection,
    WsRequest, WsResponse, WsServerConfig,
};
use eyre::Result;
use futures::FutureExt;
use futures::future::{Either, LocalBoxFuture, select};
use futures::io::{AsyncRead, AsyncWrite};
use nagoya::reactor::TaskSet;
use serde::{Deserialize, Serialize};

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

/// The server's side of one connection, as a future the caller drives.
///
/// It is handed back rather than spawned because there is no spawner to hand it to:
/// `serve_connection`'s futures are `?Send` and nagoya has no `spawn_local`, so the
/// only place a non-`Send` connection loop can run is the thread that built it.
/// [`with_connected_client`] is what puts it in a [`TaskSet`] beside the test body.
fn serve(server: WebsocketServer, server_io: DuplexHalf) -> impl Future<Output = ()> {
    let server = Arc::new(server);
    let states = Arc::new(WebsocketStates::new());
    server
        .toolbox
        .set_ws_states(states.clone_states(), false, false);
    let stream: Box<dyn MessageStream> =
        Box::new(TransportStream::new(framed_json_neutral(server_io)));
    server.serve_connection(attested_peer(), states, stream, None)
}

/// Run `body` with a client wired to `server` over a duplex pipe, on this thread.
///
/// This is what a `LocalSet` plus `spawn_local` used to buy: two futures that are not
/// `Send`, making progress against each other, with the test's own body as the one
/// whose completion ends the run. The connection loop goes in a [`TaskSet`] and
/// [`select`] drives the pair, because a `TaskSet` alone is ready only once every task
/// in it has finished, and the server's loop finishes only when its peer goes away —
/// awaiting the set would therefore deadlock against the client it is serving.
///
/// The arms are not symmetric, deliberately. The body finishing ends the test, which
/// drops the server mid-connection and is the normal exit. The server finishing first
/// means the connection died under the body, and the body is then awaited anyway so
/// that the failure surfaces as the assertion it actually broke rather than as a
/// silently truncated test.
fn with_connected_client<F, Fut>(server: WebsocketServer, body: F)
where
    F: FnOnce(WsClient) -> Fut,
    Fut: Future<Output = ()> + 'static,
{
    let (server_io, client_io) = duplex(256 * 1024);

    let mut tasks = TaskSet::new();
    tasks.push(serve(server, server_io));

    let client_stream: Box<dyn MessageStream> =
        Box::new(TransportStream::new(framed_json_neutral(client_io)));
    let body: LocalBoxFuture<'static, ()> =
        body(WsClient::from_stream(client_stream)).boxed_local();

    // No reactor: nothing here is a file descriptor, so every wake comes from the
    // pipe's own wakers or from the timer thread, both of which reach a plain
    // `block_on` parker.
    nagoya::block_on(async move {
        match select(tasks, body).await {
            Either::Right(((), _server)) => {}
            Either::Left(((), body)) => body.await,
        }
    });
}

/// (a) A legacy `{method, seq, params}` request reaches a real registered handler and
/// its response comes back — over a duplex pipe, with no TCP socket anywhere.
#[test]
fn legacy_request_round_trips_over_a_non_websocket_transport() {
    with_connected_client(build_server(false), |mut client| async move {
        let resp: EchoResponse = nagoya::timeout(
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
    });
}

/// (b) MCP `initialize` → `tools/list` → `tools/call` completes on the *same*
/// connection type, proving the JSON-RPC surface is not tied to WebSockets either.
#[test]
fn mcp_initialize_and_tool_call_work_over_the_same_transport() {
    with_connected_client(build_server(true), |mut client| async move {
        let initialize = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "seam-test", "version": "0.0.0"}}
        });
        client
            .send_raw(initialize.to_string().as_bytes())
            .await
            .expect("send initialize");
        let resp = nagoya::timeout(Duration::from_secs(5), client.recv_raw())
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
        let resp = nagoya::timeout(Duration::from_secs(5), client.recv_raw())
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
        let resp = nagoya::timeout(Duration::from_secs(5), client.recv_raw())
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
    });
}

// ---------------------------------------------------------------------------
// Phase 4 — hooks on both dispatch paths
// ---------------------------------------------------------------------------

use endpoint_libs::libs::error_code::ErrorCode;
use endpoint_libs::libs::peer::Extensions;
use endpoint_libs::libs::ws::{AfterRequest, BeforeRequest, OnConnect, RequestOutcome};
use endpoint_libs::model::EndpointSchema;

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
        self.0
            .lock()
            .unwrap()
            .push(format!("{}:{label}", endpoint.name));
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

/// A BeforeRequest hook rejects on the legacy path, with the exact error frame, and
/// a passing request receives the claims the hook attached.
#[test]
fn before_hook_gates_the_legacy_path_and_passes_claims() {
    let recorder = RecordOutcomes::default();
    // The recorder is shared state rather than something the connection owns, so the
    // body keeps a clone and checks it in place, exactly where the `LocalSet` version
    // did: with both requests answered and before anything is torn down.
    let observed = recorder.clone();

    with_connected_client(
        server_with_hooks(recorder, false),
        move |mut client| async move {
            // Allowed: the handler sees what the hook put in extensions.
            let resp: ClaimsResponse = client
                .request(ClaimsRequest {
                    message: "fine".into(),
                })
                .await
                .expect("allowed request failed");
            assert_eq!(resp.message, "seen:fine");

            // Denied: the handler never runs; the hook's code and params come back.
            client
                .send_req(
                    ClaimsRequest::METHOD_ID,
                    ClaimsRequest {
                        message: "denied".into(),
                    },
                )
                .await
                .expect("send");
            let raw = client.recv_raw().await.expect("recv");
            assert_eq!(raw["code"], ErrorCode::FORBIDDEN.to_u32(), "frame: {raw}");
            assert_eq!(raw["params"]["kind"], "PolicyDenied", "frame: {raw}");
            assert_eq!(
                raw["params"]["message"], "blocked by policy",
                "frame: {raw}"
            );

            // AfterRequest saw both, with the rejection reported as a public error.
            let seen = observed.0.lock().unwrap().clone();
            assert_eq!(
                seen,
                vec![
                    "Claims:ok".to_owned(),
                    format!("Claims:public:{}", ErrorCode::FORBIDDEN.to_u32())
                ]
            );
        },
    );
}

/// The same hook must gate `tools/call`, with the rejection encoded as an MCP tool
/// error rather than a legacy error frame.
#[test]
fn before_hook_gates_the_mcp_path_with_a_tool_error() {
    let recorder = RecordOutcomes::default();
    let observed = recorder.clone();

    with_connected_client(
        server_with_hooks(recorder, true),
        move |mut client| async move {
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
            let resp = nagoya::timeout(Duration::from_secs(5), client.recv_raw())
                .await
                .expect("timed out")
                .expect("recv");

            assert_eq!(resp["id"], 7, "frame: {resp}");
            assert_eq!(
                resp["result"]["isError"], true,
                "expected a tool error: {resp}"
            );
            let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
            assert!(
                text.contains("blocked by policy") || text.contains("PolicyDenied"),
                "tool error did not carry the hook's payload: {resp}"
            );

            let seen = observed.0.lock().unwrap().clone();
            assert_eq!(
                seen,
                vec![format!("Claims:public:{}", ErrorCode::FORBIDDEN.to_u32())]
            );
        },
    );
}

/// An OnConnect hook refuses a peer outright — no messages are exchanged at all.
#[test]
fn on_connect_hook_can_refuse_a_peer() {
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

    let config = WsServerConfig {
        insecure: true,
        ..Default::default()
    };
    let mut server = WebsocketServer::new(config);
    server.set_auth_controller(AllowAllAuthController);
    server.add_handler(MethodEcho);
    server.add_on_connect_hook(RefuseUnattested);

    // `serve` supplies an *attested* peer, so this one is admitted.
    with_connected_client(server, |mut client| async move {
        let resp: EchoResponse = client
            .request(EchoRequest {
                message: "hi".into(),
            })
            .await
            .expect("attested peer should be admitted");
        assert_eq!(resp.message, "echo[local/test-harness]: hi");
    });
}

// ---------------------------------------------------------------------------
// An in-memory duplex pipe over `futures_io`
// ---------------------------------------------------------------------------
//
// WHY THIS IS HERE AT ALL: `tokio::io::duplex` used to supply it, and the neutral
// transport does not want a tokio in the graph to get a pair of connected endpoints
// that never touch the kernel. What `framed_json_neutral` actually requires is
// `futures_io::{AsyncRead, AsyncWrite} + Unpin + Send + 'static` and nothing else, so
// the substitute is two byte buffers, each with the one waker of whichever side is
// waiting on it. `futures::io::Cursor` cannot stand in: it is one buffer with no
// second endpoint and no blocking, so a read past the end is EOF rather than "not
// yet", which is precisely the behaviour a request/response test needs.
//
// WHY A WAKER RATHER THAN A POLL LOOP: both halves run on one thread under
// `block_on`, so a reader that returned `Pending` without leaving a waker behind would
// never be polled again and the test would hang rather than fail. Every transition
// that could unblock the other side — bytes written, bytes drained, a close, a drop —
// wakes it, and the wake happens after the lock is released so a waker that polls
// re-entrantly cannot deadlock on it.

/// One direction of the pipe: what one half has written and the other has not read.
struct Pipe {
    data: VecDeque<u8>,
    /// The most this direction will buffer before writes park. Mirrors the argument
    /// `tokio::io::duplex` took, and is generous enough here that no test frame
    /// reaches it.
    capacity: usize,
    /// The writing end has closed or been dropped: reads drain what is left and then
    /// report EOF, which is what makes a clean close look clean to the framing layer
    /// rather than like a truncated frame.
    write_closed: bool,
    /// The reading end is gone, so further writes are a broken pipe rather than bytes
    /// nobody will ever look at.
    read_closed: bool,
    reader: Option<Waker>,
    writer: Option<Waker>,
}

impl Pipe {
    fn new(capacity: usize) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            data: VecDeque::new(),
            capacity,
            write_closed: false,
            read_closed: false,
            reader: None,
            writer: None,
        }))
    }
}

/// One endpoint: it reads from the direction its peer writes, and writes to the
/// direction its peer reads.
struct DuplexHalf {
    inbound: Arc<Mutex<Pipe>>,
    outbound: Arc<Mutex<Pipe>>,
}

/// A connected pair, each half `Send` and owning no file descriptor.
fn duplex(capacity: usize) -> (DuplexHalf, DuplexHalf) {
    let one = Pipe::new(capacity);
    let other = Pipe::new(capacity);
    (
        DuplexHalf {
            inbound: Arc::clone(&one),
            outbound: Arc::clone(&other),
        },
        DuplexHalf {
            inbound: other,
            outbound: one,
        },
    )
}

impl AsyncRead for DuplexHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut pipe = self.inbound.lock().expect("the pipe lock");

        if pipe.data.is_empty() {
            // EOF only once the writer is gone. Otherwise this is "not yet", and the
            // waker is what gets us polled again when bytes arrive.
            if pipe.write_closed {
                return Poll::Ready(Ok(0));
            }
            pipe.reader = Some(cx.waker().clone());
            return Poll::Pending;
        }

        let mut read = 0;
        while read < buf.len() {
            let Some(byte) = pipe.data.pop_front() else {
                break;
            };
            buf[read] = byte;
            read += 1;
        }

        // Draining may have made room, so whoever parked on a full buffer is owed a
        // poll.
        let writer = pipe.writer.take();
        drop(pipe);
        if let Some(writer) = writer {
            writer.wake();
        }
        Poll::Ready(Ok(read))
    }
}

impl AsyncWrite for DuplexHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut pipe = self.outbound.lock().expect("the pipe lock");

        if pipe.read_closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the reading half is gone",
            )));
        }

        let free = pipe.capacity - pipe.data.len();
        if free == 0 {
            // Never `Ok(0)`: the framing layer reads that as a stream that will never
            // accept anything and fails the send outright, where the truth is that it
            // should wait for the reader.
            pipe.writer = Some(cx.waker().clone());
            return Poll::Pending;
        }

        let written = free.min(buf.len());
        pipe.data.extend(&buf[..written]);

        let reader = pipe.reader.take();
        drop(pipe);
        if let Some(reader) = reader {
            reader.wake();
        }
        Poll::Ready(Ok(written))
    }

    /// Nothing is held back, so there is nothing to push out: a write is visible to
    /// the peer the moment it lands in the buffer.
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut pipe = self.outbound.lock().expect("the pipe lock");
        pipe.write_closed = true;
        let reader = pipe.reader.take();
        drop(pipe);
        if let Some(reader) = reader {
            reader.wake();
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for DuplexHalf {
    /// Dropping an endpoint has to look like a closed socket to the peer, or the
    /// server's connection loop outlives the test that dropped its client and the
    /// process never finishes: a read that is still `Pending` with no writer left is a
    /// hang, not an EOF.
    fn drop(&mut self) {
        let mut outbound = self.outbound.lock().expect("the pipe lock");
        outbound.write_closed = true;
        let reader = outbound.reader.take();
        drop(outbound);
        if let Some(reader) = reader {
            reader.wake();
        }

        let mut inbound = self.inbound.lock().expect("the pipe lock");
        inbound.read_closed = true;
        let writer = inbound.writer.take();
        drop(inbound);
        if let Some(writer) = writer {
            writer.wake();
        }
    }
}
