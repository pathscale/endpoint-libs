use std::sync::Arc;

use async_trait::async_trait;
use endpoint_libs::libs::handler::RequestHandler;
use endpoint_libs::libs::toolbox::{ArcToolbox, RequestContext};
use endpoint_libs::libs::ws::{
    AuthController, WsConnection, WsRequest, WsResponse, WsServerConfig, WebsocketServer,
};
use eyre::Result;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use serde::{Deserialize, Serialize};

// --- Request / response types ---
//
// Naming follows the convention enforced by `check_handler`:
//   schema name "Echo" → handler struct contains "MethodEcho", request struct contains "EchoRequest"

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
        "name":       "Echo",
        "code":       1,
        "parameters": [{"name": "message", "ty": "String"}],
        "returns":    [{"name": "message", "ty": "String"}],
        "roles":      []
    }"#;
}

impl WsResponse for EchoResponse {
    type Request = EchoRequest;
}

// --- Handler ---

pub struct MethodEcho;

#[async_trait(?Send)]
impl RequestHandler for MethodEcho {
    type Request = EchoRequest;

    async fn handle(&self, _ctx: RequestContext, req: EchoRequest) -> Result<EchoResponse> {
        Ok(EchoResponse {
            message: format!("echo: {}", req.message),
        })
    }
}

// --- Auth controller ---
//
// Grants role 1 to every connecting client so the Echo endpoint is reachable.
// Replace this with real auth logic (e.g. token validation) for production use.

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

// --- Entry point ---

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config = WsServerConfig {
        name: "ws-echo".to_string(),
        address: "0.0.0.0:8080".to_string(),
        insecure: true,
        ..Default::default()
    };

    let mut server = WebsocketServer::new(config);
    server.set_auth_controller(AllowAllAuthController);
    server.add_handler(MethodEcho);

    tracing::info!("WebSocket echo server listening on 0.0.0.0:8080");
    server.listen().await
}
