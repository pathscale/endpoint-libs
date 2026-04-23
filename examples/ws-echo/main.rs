use std::sync::Arc;

use async_trait::async_trait;
use endpoint_libs::libs::error_code::ErrorCode;
use endpoint_libs::libs::handler::RequestHandler;
use endpoint_libs::libs::log::{LogLevel, LoggingConfig, OtelConfig, setup_logging};
#[cfg(feature = "error_aggregation")]
use endpoint_libs::libs::log::error_aggregation::ErrorAggregationConfig;
use endpoint_libs::libs::toolbox::{ArcToolbox, RequestContext};
use endpoint_libs::libs::ws::{
    AuthController, WsConnection, WsRequest, WsResponse, WsServerConfig, WebsocketServer,
};
use endpoint_libs::libs::ws::toolbox::CustomError;
use eyre::Result;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
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

    async fn handle(&self, ctx: RequestContext, req: EchoRequest) -> Result<EchoResponse> {
        tracing::info!(
            conn_id = %ctx.connection_id,
            message = %req.message,
            "Echo request received"
        );
        let response = EchoResponse {
            message: format!("echo: {}", req.message),
        };
        tracing::info!(
            conn_id = %ctx.connection_id,
            response_message = %response.message,
            "Echo response sent"
        );
        Ok(response)
    }
}

pub struct MethodReceiveUserInfo;

#[async_trait(?Send)]
impl RequestHandler for MethodReceiveUserInfo {
    type Request = HoneyReceiveUserInfoRequest;

    async fn handle(
        &self,
        ctx: RequestContext,
        req: HoneyReceiveUserInfoRequest,
    ) -> Result<HoneyReceiveUserInfoResponse> {
        tracing::info!(
            conn_id = %ctx.connection_id,
            user_pub_id = %req.user_pub_id,
            username = %req.username,
            app_pub_id = ?req.app_pub_id,
            has_token = req.token.is_some(),
            "ReceiveUserInfo request received (test server — will reject)"
        );
        let msg = format!(
            "Test passed: received ReceiveUserInfo for user '{}' (id: {}){} — \
             this is a test server and will not process the request",
            req.username,
            req.user_pub_id,
            req.app_pub_id
                .map(|id| format!(", app: {id}"))
                .unwrap_or_default(),
        );
        tracing::info!(
            conn_id = %ctx.connection_id,
            user_pub_id = %req.user_pub_id,
            "Rejecting ReceiveUserInfo with BAD_REQUEST"
        );
        Err(CustomError::new(ErrorCode::BAD_REQUEST, msg).into())
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
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<()>> {
        async move {
            let conn_id = conn.connection_id;
            tracing::info!(
                conn_id = %conn_id,
                ip = %conn.address,
                header_len = header.len(),
                "New connection — granting role 1 (allow-all auth)"
            );
            conn.set_roles(Arc::new(vec![1]));
            tracing::info!(conn_id = %conn_id, "Roles set successfully");
            Ok(())
        }
        .boxed_local()
    }
}

// --- Entry point ---

#[tokio::main]
async fn main() -> Result<()> {
    let _log = setup_logging(LoggingConfig {
        level: LogLevel::Debug,
        otel_config: OtelConfig::default(),
        file_config: None,
        #[cfg(feature = "error_aggregation")]
        error_aggregation: ErrorAggregationConfig {
            limit: 100,
            normalize: true,
        },
        #[cfg(feature = "log_throttling")]
        throttling_config: None,
    })?;

    tracing::info!("Logging initialised at INFO level");

    let config = WsServerConfig {
        name: "ws-echo".to_string(),
        address: "0.0.0.0:8080".to_string(),
        insecure: true,
        ..Default::default()
    };

    tracing::info!(
        name = %config.name,
        address = %config.address,
        insecure = config.insecure,
        "Configuring WebSocket server"
    );

    let mut server = WebsocketServer::new(config);
    server.set_auth_controller(AllowAllAuthController);
    server.add_handler(MethodEcho);
    server.add_handler(MethodReceiveUserInfo);

    tracing::info!("Registered handlers: Echo (method 1), ReceiveUserInfo (method 211)");
    tracing::info!("WebSocket echo server listening on 0.0.0.0:8080");
    server.listen().await
}
