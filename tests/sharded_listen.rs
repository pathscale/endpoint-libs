//! `listen_until` with several shards, over real TCP.
//!
//! Each shard is a thread with its own reactor and its own `SO_REUSEPORT`
//! listener on one port. This drives a server with three of them: every client
//! must complete a handshake and a request whichever shard the kernel picks,
//! and one stop must bring every shard down so `listen_until` returns.

#![cfg(all(feature = "ws", feature = "ws-client"))]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use endpoint_libs::libs::handler::{RequestHandler, Response};
use endpoint_libs::libs::toolbox::{ArcToolbox, CustomError, RequestContext};
use endpoint_libs::libs::ws::{
    AuthController, WebsocketServer, WsClientBuilder, WsConnection, WsRequest, WsResponse,
    WsServerConfig,
};
use eyre::Result;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use nagoya::reactor::{Reactor, block_on_with};
use serde::{Deserialize, Serialize};

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

    async fn handle(&self, _ctx: RequestContext, req: EchoRequest) -> Response<EchoRequest> {
        Ok(EchoResponse {
            message: format!("echo[{}]: {}", thread_name(), req.message),
        })
    }
}

fn thread_name() -> String {
    std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string()
}

struct AllowAll;

impl AuthController for AllowAll {
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

/// A port nothing holds, found by binding zero and letting it go.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn three_shards_serve_one_port_and_stop_together() {
    // SAFETY: this test binary has one test, so nothing else reads the
    // environment concurrently.
    unsafe { std::env::set_var("WS_SHARDS", "3") };
    let port = free_port();

    let (stop_tx, stop_rx) = futures::channel::oneshot::channel::<()>();
    let server = std::thread::spawn(move || {
        let config = WsServerConfig {
            address: format!("127.0.0.1:{port}"),
            ..WsServerConfig::default()
        };
        let mut server = WebsocketServer::new(config);
        server.set_auth_controller(AllowAll);
        server.add_handler(MethodEcho);
        server.listen_until(async move {
            let _ = stop_rx.await;
        })
    });

    let reactor = Reactor::local().unwrap();
    let handle = reactor.handle();
    let url = format!("ws://127.0.0.1:{port}/");
    let replies = block_on_with(&reactor, async {
        // The shards bind as their threads start; give them a moment.
        let mut replies = Vec::new();
        for attempt in 0..12 {
            let connected = WsClientBuilder::new()
                .protocol_header("0echo")
                .build(&url, &handle)
                .await;
            let (mut client, _) = match connected {
                Ok(client) => client,
                Err(_) if replies.is_empty() && attempt < 11 => {
                    nagoya::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Err(err) => panic!("connect failed: {err}"),
            };
            let reply = client
                .request(EchoRequest {
                    message: format!("hello {attempt}"),
                })
                .await
                .expect("request");
            replies.push(reply.message);
        }
        replies
    });
    assert!(!replies.is_empty());
    for reply in &replies {
        assert!(reply.contains(": hello "), "{reply}");
        eprintln!("{reply}");
    }

    stop_tx.send(()).unwrap();
    let served = server.join().expect("server thread panicked");
    assert!(served.is_ok(), "{served:?}");
}
