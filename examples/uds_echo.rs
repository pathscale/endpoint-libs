//! The transport seam on a real OS transport: the ordinary endpoint machinery served
//! over a Unix domain socket, with no TCP, no TLS and no HTTP upgrade.
//!
//! ```bash
//! cargo run --example uds_echo --features full,framed-transport,ws-client
//! ```
//!
//! This is the shape a platform-transport crate implements for real. Note what is
//! *not* here: no attestation. A plain `UnixListener` cannot tell you what code is on
//! the other end — that is what `SO_PEERPIDFD` + an executable digest (Linux), a SID
//! DACL (Windows), or an XPC code-signing requirement (macOS) are for, and why they
//! belong in `endpoint-transport-local` rather than in this crate.

use std::sync::Arc;

use async_trait::async_trait;
use endpoint_libs::libs::handler::{RequestHandler, Response};
use endpoint_libs::libs::peer::{Attestation, LocalPeer, PeerIdentity};
use endpoint_libs::libs::toolbox::{ArcToolbox, CustomError, RequestContext};
use endpoint_libs::libs::ws::transport::{TransportStream, framed_json};
use endpoint_libs::libs::ws::{
    AuthController, MessageStream, SessionListener, WebsocketServer, WsClient, WsConnection,
    WsRequest, WsResponse, WsServerConfig,
};
use eyre::Result;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::LocalSet;

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
        println!("[server] handling request from {}", ctx.peer);
        Ok(EchoResponse {
            message: format!("echo: {}", req.message),
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

/// A `SessionListener` over a Unix socket — the seam a platform crate implements.
struct UdsListener {
    inner: UnixListener,
}

#[async_trait]
impl SessionListener for UdsListener {
    async fn accept(&self) -> Result<(Box<dyn MessageStream>, PeerIdentity)> {
        let (stream, _addr) = self.inner.accept().await?;

        // SO_PEERCRED gives pid/uid for free on Unix. It identifies the *process*,
        // not the *code* — hence Attestation::None. Upgrading this to
        // Attestation::Verified is exactly what the sibling crate adds.
        let peer = PeerIdentity::Local(LocalPeer {
            pid: stream.peer_cred().ok().and_then(|c| c.pid()).map(|p| p as u32),
            uid: stream.peer_cred().ok().map(|c| c.uid()),
            attestation: Attestation::None,
        });

        let framed: Box<dyn MessageStream> = Box::new(TransportStream::new(framed_json(stream)));
        Ok((framed, peer))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let path = std::env::temp_dir().join(format!("endpoint-libs-uds-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let listener = UdsListener {
        inner: UnixListener::bind(&path)?,
    };
    println!("[server] listening on {}", path.display());

    let config = WsServerConfig {
        insecure: true,
        ..Default::default()
    };
    let mut server = WebsocketServer::new(config);
    server.set_auth_controller(AllowAllAuthController);
    server.add_handler(MethodEcho);

    // MessageStream's futures are not Send, so everything runs on a LocalSet.
    let local = LocalSet::new();
    let client_path = path.clone();
    local
        .run_until(async move {
            tokio::task::spawn_local(async move {
                if let Err(err) = server.serve_with(listener).await {
                    eprintln!("[server] stopped: {err}");
                }
            });

            // Give the listener a moment, then connect as a client over the same
            // socket using the transport-agnostic constructor.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let stream = UnixStream::connect(&client_path).await?;
            let client_stream: Box<dyn MessageStream> =
                Box::new(TransportStream::new(framed_json(stream)));
            let mut client = WsClient::from_stream(client_stream);

            let resp: EchoResponse = client
                .request(EchoRequest {
                    message: "over a unix socket".into(),
                })
                .await?;
            println!("[client] got: {}", resp.message);
            assert_eq!(resp.message, "echo: over a unix socket");
            println!("[client] OK — endpoint machinery ran with no TCP, TLS or HTTP");
            Ok::<_, eyre::Error>(())
        })
        .await?;

    let _ = std::fs::remove_file(&path);
    Ok(())
}
