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
use endpoint_libs::libs::error_code::ErrorCode;
use endpoint_libs::libs::ws::toolbox::CustomError;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HoneyReceiveUserInfoRequest {
    pub user_pub_id: Uuid,
    pub username: String,
    #[serde(default)]
    pub app_pub_id: Option<Uuid>,
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HoneyReceiveUserInfoResponse {}

impl WsRequest for HoneyReceiveUserInfoRequest {
    type Response = HoneyReceiveUserInfoResponse;
    const METHOD_ID: u32 = 211;
    const ROLES: &'static [u32] = &[1];
    const SCHEMA: &'static str = r#"{
  "name": "ReceiveUserInfo",
  "code": 211,
  "parameters": [
    {"name": "userPubId", "ty": "UUID"},
    {"name": "username", "ty": "String"},
    {"name": "appPubId", "ty": {"Optional": "UUID"}},
    {"name": "token", "ty": {"Optional": "String"}}
  ],
  "returns": [],
  "stream_response": null,
  "description": "Test endpoint mirroring ReceiveUserInfo (code 211)",
  "json_schema": null,
  "roles": []
}"#;
}

impl WsResponse for HoneyReceiveUserInfoResponse {
    type Request = HoneyReceiveUserInfoRequest;
}

// --- Handlers ---

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

pub struct MethodReceiveUserInfo;

#[async_trait(?Send)]
impl RequestHandler for MethodReceiveUserInfo {
    type Request = HoneyReceiveUserInfoRequest;

    async fn handle(
        &self,
        _ctx: RequestContext,
        req: HoneyReceiveUserInfoRequest,
    ) -> Result<HoneyReceiveUserInfoResponse> {
        Err(CustomError::new(
            ErrorCode::BAD_REQUEST,
            format!(
                "Test passed: received ReceiveUserInfo for user '{}' (id: {}){} — \
                 this is a test server and will not process the request",
                req.username,
                req.user_pub_id,
                req.app_pub_id
                    .map(|id| format!(", app: {id}"))
                    .unwrap_or_default(),
            ),
        )
        .into())
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
    server.add_handler(MethodReceiveUserInfo);

    tracing::info!("WebSocket echo server listening on 0.0.0.0:8080");
    server.listen().await
}
