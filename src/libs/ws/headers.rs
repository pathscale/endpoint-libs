use convert_case::Case;
use convert_case::Casing;
use eyre::{Context, ContextCompat, Result, bail};
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::*;

use crate::libs::error_code::ErrorCode;
use crate::libs::handler::Response;
use crate::libs::toolbox::{ArcToolbox, CustomError, RequestContext, Toolbox};
use crate::model::EndpointSchema;
use crate::model::Type;

use super::{
    ConnectionId, WsConnection, WsRequest, WsResponseError, WsResponseValue, WsSuccessResponse,
};

/// The encoded decision from an authentication handler.
///
/// Controllers that wrap a typed controller should delegate `auth_reply` so the
/// encoded response remains pending until the server's outer handshake gate ends.
pub enum AuthReply {
    Accepted(Option<WsSuccessResponse>),
    Rejected(Option<WsResponseError>),
}

impl AuthReply {
    /// Apply a reply using the legacy toolbox delivery contract.
    /// The server uses its connection queue directly to confirm admission.
    #[doc(hidden)]
    pub fn finish(self, toolbox: &ArcToolbox, connection_id: ConnectionId) -> Result<()> {
        self.finish_with(|reply| toolbox.send(connection_id, reply))
    }

    pub(super) fn finish_with(self, send: impl FnOnce(WsResponseValue) -> bool) -> Result<()> {
        match self {
            Self::Accepted(reply) => {
                if let Some(reply) = reply {
                    eyre::ensure!(
                        send(WsResponseValue::Immediate(reply)),
                        "authentication reply could not be queued"
                    );
                }
                Ok(())
            }
            Self::Rejected(reply) => {
                if let Some(reply) = reply {
                    send(WsResponseValue::Error(reply));
                }
                Err(eyre::Report::new(AuthRejected))
            }
        }
    }
}

#[derive(Debug)]
struct AuthRejected;

impl std::error::Error for AuthRejected {}

impl std::fmt::Display for AuthRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("authentication reply rejected")
    }
}

pub(super) fn is_auth_rejection(err: &eyre::Report) -> bool {
    err.downcast_ref::<AuthRejected>().is_some()
}

pub trait AuthController: Sync + Send {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<()>>;

    /// Return a deferred authentication decision. Legacy controllers keep their
    /// existing behavior through this default; wrappers around typed controllers
    /// should override it and delegate the inner reply.
    fn auth_reply(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<AuthReply>> {
        let auth = self.auth(toolbox, header, conn);
        async move {
            auth.await?;
            Ok(AuthReply::Accepted(None))
        }
        .boxed_local()
    }
}

pub struct SimpleAuthController;

impl AuthController for SimpleAuthController {
    fn auth(
        self: Arc<Self>,
        _toolbox: &ArcToolbox,
        _header: String,
        _conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<()>> {
        async move { Ok(()) }.boxed()
    }
}

pub trait SubAuthController: Sync + Send {
    type Request: WsRequest + 'static;
    type Error: Into<CustomError> + 'static;

    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        req: Self::Request,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, AuthResponse<Self::Request, Self::Error>>;
}

#[allow(type_alias_bounds)]
pub type AuthResponse<T: WsRequest, E = CustomError> = Response<T, E>;

#[doc(hidden)]
pub trait SubAuthControllerErased: Sync + Send {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, ()>;

    /// Deferred form of `auth`; the default preserves manual erased handlers
    /// that historically send their own response.
    fn auth_reply(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, AuthReply> {
        let auth = self.auth(toolbox, param, ctx, conn);
        async move {
            auth.await;
            AuthReply::Accepted(None)
        }
        .boxed_local()
    }
}

impl<T: SubAuthController + 'static> SubAuthControllerErased for T {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, ()> {
        let toolbox = Arc::clone(toolbox);
        async move {
            let reply = self.auth_reply(&toolbox, param, ctx.clone(), conn).await;
            let _ = reply.finish(&toolbox, ctx.connection_id);
        }
        .boxed_local()
    }

    fn auth_reply(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, AuthReply> {
        let toolbox = Arc::clone(toolbox);
        async move {
            let data: T::Request = match serde_json::from_value(param) {
                Ok(data) => data,
                Err(_err) => {
                    return AuthReply::Rejected(Some(WsResponseError {
                        method: ctx.method,
                        code: ErrorCode::BAD_REQUEST.to_u32(),
                        seq: ctx.seq,
                        log_id: ctx.log_id.to_string(),
                        params: serde_json::json!({
                            "kind": ErrorCode::BAD_REQUEST.kind(),
                            "message": "authentication failed",
                        }),
                    }));
                }
            };

            let resp = SubAuthController::auth(self, &toolbox, data, ctx.clone(), conn).await;
            match Toolbox::encode_handler_response::<T::Request, T::Error>(ctx.clone(), resp) {
                Some(WsResponseValue::Immediate(reply)) => AuthReply::Accepted(Some(reply)),
                Some(WsResponseValue::Error(error)) => AuthReply::Rejected(Some(error)),
                None => AuthReply::Rejected(None),
                Some(_) => AuthReply::Rejected(Some(WsResponseError {
                    method: ctx.method,
                    code: ErrorCode::INTERNAL_ERROR.to_u32(),
                    seq: ctx.seq,
                    log_id: ctx.log_id.to_string(),
                    params: serde_json::json!({
                        "kind": ErrorCode::INTERNAL_ERROR.kind(),
                        "message": "authentication failed",
                    }),
                })),
            }
        }
        .boxed_local()
    }
}

pub struct EndpointAuthController {
    pub auth_endpoints: HashMap<String, WsAuthController>,
}
pub struct WsAuthController {
    pub schema: EndpointSchema,
    pub handler: Arc<dyn SubAuthControllerErased>,
}

impl Default for EndpointAuthController {
    fn default() -> Self {
        Self::new()
    }
}

impl EndpointAuthController {
    pub fn new() -> Self {
        Self {
            auth_endpoints: Default::default(),
        }
    }
    pub fn add_auth_endpoint(
        &mut self,
        schema: EndpointSchema,
        handler: impl SubAuthController + 'static,
    ) {
        self.auth_endpoints.insert(
            schema.name.to_ascii_lowercase(),
            WsAuthController {
                schema,
                handler: Arc::new(handler),
            },
        );
    }
}
fn parse_ty(ty: &Type, value: &str) -> Result<serde_json::Value> {
    Ok(match &ty {
        Type::String => {
            let decoded = urlencoding::decode(value)?;
            serde_json::Value::String(decoded.to_string())
        }
        Type::Int64 => serde_json::Value::Number(
            value
                .parse::<i64>()
                .with_context(|| format!("Failed to parse integer: {value}"))?
                .into(),
        ),
        Type::Boolean => serde_json::Value::Bool(
            value
                .parse::<bool>()
                .with_context(|| format!("Failed to parse boolean: {value}"))?,
        ),
        Type::Enum { .. } => serde_json::Value::String(value.to_string()),
        Type::EnumRef { .. } => serde_json::Value::String(value.to_string()),
        Type::UUID => serde_json::Value::String(value.to_string()),
        Type::NanoId { .. } => serde_json::Value::String(value.to_string()),
        Type::Optional(ty) => parse_ty(ty, value)?,
        Type::BlockchainAddress => serde_json::Value::String(value.to_string()),
        ty => bail!("Not implemented {:?}", ty),
    })
}

fn parse_protocol_header(header: &str) -> HashMap<&str, &str> {
    header
        .split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .filter(|x| x.as_bytes().first().is_some_and(u8::is_ascii_digit))
        .map(|x| x.split_at(1))
        .collect()
}

impl AuthController for EndpointAuthController {
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let connection_id = conn.connection_id;
        let toolbox = Arc::clone(toolbox);
        async move {
            self.auth_reply(&toolbox, header, conn)
                .await?
                .finish(&toolbox, connection_id)
        }
        .boxed_local()
    }

    fn auth_reply(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<AuthReply>> {
        let toolbox = toolbox.clone();

        async move {
            let splits = parse_protocol_header(&header);

            let method = splits.get("0").context("Could not find method")?;
            debug!(
                ws_server = true,
                method = %method,
                parameter_count = splits.len().saturating_sub(1),
                "EndpointAuthController: parsed protocol header"
            );
            let endpoint = self
                .auth_endpoints
                .get(*method)
                .with_context(|| format!("Could not find endpoint for method {method}"))?;
            let mut params = serde_json::Map::new();
            for (index, param) in endpoint.schema.parameters.iter().enumerate() {
                let index = index + 1;
                match splits.get(&index.to_string().as_str()) {
                    Some(value) => {
                        params.insert(param.name.to_case(Case::Camel), parse_ty(&param.ty, value)?);
                    }
                    None if !matches!(&param.ty, Type::Optional(_)) => {
                        bail!("Could not find param {} {}", param.name, index);
                    }
                    _ => {}
                }
            }
            let roles = conn.roles.read().clone();
            let ctx = RequestContext {
                connection_id: conn.connection_id,
                user_id: 0,
                seq: 0,
                method: endpoint.schema.code,
                log_id: conn.log_id,
                roles: roles.clone(),
                ip_addr: conn.peer.ip_addr(),
                peer: conn.peer.clone(),
                extensions: conn.extensions.clone(),
            };
            let reply = endpoint
                .handler
                .clone()
                .auth_reply(
                    &toolbox,
                    serde_json::Value::Object(params),
                    ctx.clone(),
                    conn,
                )
                .await;
            debug!(ws_server = true, "Auth response encoded");
            Ok(reply)
        }
        .boxed_local()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_header() {
        let map = parse_protocol_header("");
        assert!(map.is_empty());
    }

    #[test]
    fn parse_single_method() {
        let map = parse_protocol_header("0my_endpoint");
        assert_eq!(map.get("0"), Some(&"my_endpoint"));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn parse_method_and_token() {
        let map = parse_protocol_header("0login,1abc123");
        assert_eq!(map.get("0"), Some(&"login"));
        assert_eq!(map.get("1"), Some(&"abc123"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_multiple_params() {
        let map = parse_protocol_header("0my_endpoint,1token_value,2user_123,3extra");
        assert_eq!(map.get("0"), Some(&"my_endpoint"));
        assert_eq!(map.get("1"), Some(&"token_value"));
        assert_eq!(map.get("2"), Some(&"user_123"));
        assert_eq!(map.get("3"), Some(&"extra"));
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn parse_whitespace_tolerance() {
        let map = parse_protocol_header("0method, 1value , 2another");
        assert_eq!(map.get("0"), Some(&"method"));
        assert_eq!(map.get("1"), Some(&"value"));
        assert_eq!(map.get("2"), Some(&"another"));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn parse_empty_segments_filtered() {
        let map = parse_protocol_header("0method,,1value");
        assert_eq!(map.get("0"), Some(&"method"));
        assert_eq!(map.get("1"), Some(&"value"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_leading_trailing_commas() {
        let map = parse_protocol_header(",0method,1value,");
        assert_eq!(map.get("0"), Some(&"method"));
        assert_eq!(map.get("1"), Some(&"value"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_comma_in_value() {
        let map = parse_protocol_header("0method,1val,ue");
        assert_eq!(map.get("0"), Some(&"method"));
        assert_eq!(map.get("1"), Some(&"val"));
        assert!(!map.contains_key("u"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn malformed_unicode_and_non_digit_discriminators_are_ignored() {
        for header in ["éx", "🦀", " , , ", "xvalue", "０method"] {
            assert!(parse_protocol_header(header).is_empty());
        }
        let map = parse_protocol_header("éx,0first,1héllo world,0last");
        assert_eq!(map.get("0"), Some(&"last"));
        assert_eq!(map.get("1"), Some(&"héllo world"));
        assert_eq!(map.len(), 2);
    }
}

#[cfg(test)]
mod preauth_sanitization_regression {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use futures::FutureExt;
    use parking_lot::RwLock;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::*;
    use crate::libs::peer::{Extensions, PeerIdentity};
    use crate::libs::ws::WsResponse;

    #[derive(Clone, Deserialize, Serialize)]
    struct TypedAuthRequest {
        credential: u64,
    }

    #[derive(Clone, Deserialize, Serialize)]
    struct TypedAuthReply {
        accepted: bool,
    }

    impl WsRequest for TypedAuthRequest {
        type Response = TypedAuthReply;
        const METHOD_ID: u32 = 71;
        const SCHEMA: &'static str = "{}";
        const ROLES: &'static [u32] = &[];
    }

    impl WsResponse for TypedAuthReply {
        type Request = TypedAuthRequest;
    }

    struct TypedAuthHandler {
        calls: Arc<AtomicUsize>,
        accepted_credential: Arc<AtomicUsize>,
    }

    impl SubAuthController for TypedAuthHandler {
        type Request = TypedAuthRequest;
        type Error = CustomError;

        fn auth(
            self: Arc<Self>,
            _toolbox: &ArcToolbox,
            req: Self::Request,
            _ctx: RequestContext,
            _conn: Arc<WsConnection>,
        ) -> LocalBoxFuture<'static, AuthResponse<Self::Request, Self::Error>> {
            async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.accepted_credential
                    .store(req.credential as usize, Ordering::SeqCst);
                Ok(TypedAuthReply {
                    accepted: req.credential == 42,
                })
            }
            .boxed_local()
        }
    }

    fn test_connection() -> Arc<WsConnection> {
        Arc::new(WsConnection {
            connection_id: 9,
            user_id: std::sync::atomic::AtomicU64::new(0),
            roles: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            peer: PeerIdentity::Unknown,
            extensions: Extensions::new(),
            log_id: 123,
        })
    }

    #[test]
    fn erased_auth_sanitizes_malformed_typed_input_and_dispatches_valid_input() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent_by_toolbox = Arc::clone(&sent);
        let toolbox = Toolbox::new();
        assert!(
            toolbox
                .send_msg
                .set(Arc::new(move |connection_id, response| {
                    assert_eq!(connection_id, 9);
                    sent_by_toolbox.lock().unwrap().push(response);
                    true
                }))
                .is_ok()
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let accepted_credential = Arc::new(AtomicUsize::new(0));
        let controller: Arc<dyn SubAuthControllerErased> = Arc::new(TypedAuthHandler {
            calls: Arc::clone(&calls),
            accepted_credential: Arc::clone(&accepted_credential),
        });
        let conn = test_connection();
        let mut ctx = RequestContext::empty();
        ctx.connection_id = 9;
        ctx.method = 71;
        ctx.seq = 4;
        ctx.log_id = 123;

        let reply = futures::executor::block_on(controller.clone().auth_reply(
            &toolbox,
            json!({"credential": "secret-input-must-not-leak"}),
            ctx.clone(),
            Arc::clone(&conn),
        ));

        assert!(matches!(&reply, AuthReply::Rejected(_)));
        assert!(sent.lock().unwrap().is_empty());
        assert!(is_auth_rejection(&reply.finish(&toolbox, 9).unwrap_err()));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        {
            let sent = sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            let WsResponseValue::Error(error) = &sent[0] else {
                panic!("malformed authentication input should return an error");
            };
            assert_eq!(error.code, ErrorCode::BAD_REQUEST.to_u32());
            assert_eq!(
                error.params,
                json!({
                    "kind": ErrorCode::BAD_REQUEST.kind(),
                    "message": "authentication failed",
                })
            );
            let wire_params = serde_json::to_string(&error.params).unwrap();
            assert!(!wire_params.contains("secret-input-must-not-leak"));
            assert!(!wire_params.contains("invalid type"));
            assert!(!wire_params.contains("expected u64"));
        }

        let reply = futures::executor::block_on(controller.auth_reply(
            &toolbox,
            json!({"credential": 42}),
            ctx,
            conn,
        ));

        assert!(matches!(&reply, AuthReply::Accepted(_)));
        assert_eq!(sent.lock().unwrap().len(), 1);
        reply.finish(&toolbox, 9).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(accepted_credential.load(Ordering::SeqCst), 42);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(matches!(sent[1], WsResponseValue::Immediate(_)));
    }
}
