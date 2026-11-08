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
/// Controllers return this value without sending the reply. A wrapper can apply
/// its own checks before returning the inner controller's decision to the caller.
pub enum AuthReply {
    Accepted(Option<WsSuccessResponse>),
    Rejected(Option<WsResponseError>),
}

impl AuthReply {
    /// Apply a reply through the toolbox sender. A successful decision with a
    /// reply requires the sender to report that the reply was queued.
    /// The server checks its own connection queue directly.
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
    /// Authenticate directly and apply the reply through the toolbox. Success
    /// requires acceptance and, if there is a reply, a successful send.
    /// Implementations should customize the decision through `auth_reply`.
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let connection_id = conn.connection_id;
        let reply = self.auth_reply(toolbox, header, conn);
        let toolbox = Arc::clone(toolbox);
        async move { reply.await?.finish(&toolbox, connection_id) }.boxed_local()
    }

    /// Decide whether to authenticate, leaving any reply for the caller to send.
    /// Use `Accepted(None)` for acceptance without a reply. Return `Rejected`
    /// to refuse the session with an optional error frame, or return an error
    /// to request the server's generic authentication failure.
    ///
    /// Wrappers must await the inner controller's `auth_reply` and return the
    /// final decision after their own checks.
    fn auth_reply(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<AuthReply>>;
}

pub struct SimpleAuthController;

impl AuthController for SimpleAuthController {
    fn auth_reply(
        self: Arc<Self>,
        _toolbox: &ArcToolbox,
        _header: String,
        _conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<AuthReply>> {
        async { Ok(AuthReply::Accepted(None)) }.boxed_local()
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
    /// Apply the deferred decision when calling an erased handler directly.
    fn auth(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<()>> {
        let connection_id = ctx.connection_id;
        let reply = self.auth_reply(toolbox, param, ctx, conn);
        let toolbox = Arc::clone(toolbox);
        async move { reply.await.finish(&toolbox, connection_id) }.boxed_local()
    }

    /// Decide explicitly, returning any encoded reply for the caller to send.
    fn auth_reply(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        param: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, AuthReply>;
}

impl<T: SubAuthController + 'static> SubAuthControllerErased for T {
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
                Err(err) => {
                    debug!(
                        ws_server = true,
                        error_class = ?err.classify(),
                        request_type = std::any::type_name::<T::Request>(),
                        method = ctx.method,
                        connection_id = ctx.connection_id,
                        log_id = ctx.log_id,
                        "Rejected malformed typed authentication request"
                    );
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
pub(super) mod preauth_sanitization_regression {
    use std::collections::BTreeMap;
    use std::fmt::Debug;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use futures::FutureExt;
    use parking_lot::RwLock;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use tracing::field::{Field, Visit};
    use tracing_subscriber::{Layer, layer::Context, prelude::*};

    use super::*;
    use crate::libs::peer::{Extensions, PeerIdentity};
    use crate::libs::ws::{WebsocketStates, WsMessage, WsResponse, outbound};
    use crate::model::Field as SchemaField;

    #[derive(Debug)]
    pub(in crate::libs::ws) struct CapturedEvent {
        level: tracing::Level,
        fields: EventFields,
    }

    #[derive(Debug, Default)]
    struct EventFields(BTreeMap<&'static str, String>);

    impl Visit for EventFields {
        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            self.0.insert(field.name(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name(), value.to_owned());
        }
    }

    struct Capture(Arc<Mutex<Vec<CapturedEvent>>>);

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut fields = EventFields::default();
            event.record(&mut fields);
            self.0.lock().unwrap().push(CapturedEvent {
                level: *event.metadata().level(),
                fields,
            });
        }
    }

    pub(in crate::libs::ws) fn capture_events<T>(
        operation: impl FnOnce() -> T,
    ) -> (T, Vec<CapturedEvent>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Capture(Arc::clone(&events)));
        let result = tracing::subscriber::with_default(subscriber, operation);
        let events = std::mem::take(&mut *events.lock().unwrap());
        (result, events)
    }

    pub(in crate::libs::ws) fn assert_sanitized_rejection(
        events: &[CapturedEvent],
        secret: &str,
        message: &str,
        expected_fields: &[(&str, &str)],
    ) {
        for event in events {
            for (name, value) in &event.fields.0 {
                for forbidden in [secret, "invalid type", "expected u32", "expected u64"] {
                    assert!(
                        !name.contains(forbidden) && !value.contains(forbidden),
                        "sensitive content in event: {event:?}"
                    );
                }
            }
        }
        let rejections: Vec<_> = events
            .iter()
            .filter(|event| {
                event
                    .fields
                    .0
                    .get("message")
                    .is_some_and(|value| value == message)
            })
            .collect();
        let [rejection] = rejections.as_slice() else {
            panic!("expected one {message:?} event, got {events:?}");
        };
        assert_eq!(rejection.level, tracing::Level::DEBUG);
        let actual_fields: BTreeMap<_, _> = rejection
            .fields
            .0
            .iter()
            .map(|(&name, value)| (name, value.as_str()))
            .collect();
        let expected_fields: BTreeMap<_, _> = expected_fields
            .iter()
            .copied()
            .chain([("message", message)])
            .collect();
        assert_eq!(actual_fields, expected_fields);
    }

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
    fn direct_endpoint_auth_requires_its_reply_to_be_queued() {
        let mut controller = EndpointAuthController::new();
        controller.add_auth_endpoint(
            EndpointSchema::new(
                "Login",
                TypedAuthRequest::METHOD_ID,
                vec![SchemaField::new("credential", Type::Int64)],
                vec![],
            ),
            TypedAuthHandler {
                calls: Arc::new(AtomicUsize::new(0)),
                accepted_credential: Arc::new(AtomicUsize::new(0)),
            },
        );
        let controller = Arc::new(controller);

        for oneshot in [false, true] {
            let states = WebsocketStates::new();
            let conn = test_connection();
            let (tx, mut rx) = outbound::channel(1);
            tx.try_send(WsMessage::from("occupied")).unwrap();
            states.insert(conn.connection_id, tx, Arc::clone(&conn));
            let state = states.get_state(conn.connection_id).unwrap();
            let toolbox = Toolbox::new();
            toolbox.set_ws_states(states.clone_states(), oneshot, false);

            let auth = || {
                futures::executor::block_on(Arc::clone(&controller).auth(
                    &toolbox,
                    "0login,142".into(),
                    Arc::clone(&conn),
                ))
            };
            assert_eq!(
                auth().unwrap_err().to_string(),
                "authentication reply could not be queued"
            );
            assert_eq!(state.end.is_cancelled(), oneshot);
            assert_eq!(rx.try_recv(), Ok(WsMessage::from("occupied")));
            assert_eq!(rx.try_recv(), Err(outbound::TryRecvError::Empty));

            if !oneshot {
                auth().unwrap();
                let WsMessage::Text(frame) = rx.try_recv().unwrap() else {
                    panic!("expected a queued auth response");
                };
                let reply: serde_json::Value = serde_json::from_str(frame.as_str()).unwrap();
                assert_eq!(reply["method"], json!(TypedAuthRequest::METHOD_ID));
                assert_eq!(reply["params"], json!({"accepted": true}));
            }
        }
    }

    #[test]
    fn direct_erased_auth_reports_queue_failure_and_rejection() {
        let handler: Arc<dyn SubAuthControllerErased> = Arc::new(TypedAuthHandler {
            calls: Arc::new(AtomicUsize::new(0)),
            accepted_credential: Arc::new(AtomicUsize::new(0)),
        });
        let states = WebsocketStates::new();
        let conn = test_connection();
        let (tx, mut rx) = outbound::channel(1);
        tx.try_send(WsMessage::from("occupied")).unwrap();
        states.insert(conn.connection_id, tx, Arc::clone(&conn));
        let toolbox = Toolbox::new();
        toolbox.set_ws_states(states.clone_states(), false, false);
        let mut ctx = RequestContext::from_conn(&conn);
        ctx.method = TypedAuthRequest::METHOD_ID;

        let auth = |param| {
            futures::executor::block_on(Arc::clone(&handler).auth(
                &toolbox,
                param,
                ctx.clone(),
                Arc::clone(&conn),
            ))
        };
        assert_eq!(
            auth(json!({"credential": 42})).unwrap_err().to_string(),
            "authentication reply could not be queued"
        );
        assert_eq!(rx.try_recv(), Ok(WsMessage::from("occupied")));
        assert_eq!(rx.try_recv(), Err(outbound::TryRecvError::Empty));

        let rejection = auth(json!({"credential": null})).unwrap_err();
        assert!(is_auth_rejection(&rejection));
        let WsMessage::Text(frame) = rx.try_recv().unwrap() else {
            panic!("expected a queued auth error");
        };
        let reply: serde_json::Value = serde_json::from_str(frame.as_str()).unwrap();
        assert_eq!(reply["type"], json!("Error"));
        assert_eq!(reply["method"], json!(TypedAuthRequest::METHOD_ID));
        assert_eq!(reply["code"], json!(ErrorCode::BAD_REQUEST.to_u32()));
        assert_eq!(rx.try_recv(), Err(outbound::TryRecvError::Empty));
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

        let secret = "secret-input-must-not-leak";
        let ((), events) = capture_events(|| {
            let reply = futures::executor::block_on(controller.clone().auth_reply(
                &toolbox,
                json!({"credential": secret}),
                ctx.clone(),
                Arc::clone(&conn),
            ));

            assert!(matches!(&reply, AuthReply::Rejected(_)));
            assert!(sent.lock().unwrap().is_empty());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(is_auth_rejection(&reply.finish(&toolbox, 9).unwrap_err()));
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_sanitized_rejection(
            &events,
            secret,
            "Rejected malformed typed authentication request",
            &[
                ("ws_server", "true"),
                ("error_class", "Data"),
                ("request_type", std::any::type_name::<TypedAuthRequest>()),
                ("method", "71"),
                ("connection_id", "9"),
                ("log_id", "123"),
            ],
        );
        {
            let sent = sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(
                serde_json::to_value(&sent[0]).unwrap(),
                json!({
                    "type": "Error",
                    "method": 71,
                    "seq": 4,
                    "log_id": "123",
                    "code": ErrorCode::BAD_REQUEST.to_u32(),
                    "params": {
                        "kind": ErrorCode::BAD_REQUEST.kind(),
                        "message": "authentication failed",
                    },
                }),
            );
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
        let WsResponseValue::Immediate(success) = &sent[1] else {
            panic!("valid authentication input should return a success");
        };
        assert_eq!(success.method, 71);
        assert_eq!(success.seq, 4);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(success.params.get()).unwrap(),
            json!({"accepted": true}),
        );
    }
}
