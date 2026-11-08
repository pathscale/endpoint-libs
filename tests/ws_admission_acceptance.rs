#![cfg(all(feature = "ws", feature = "ws-client"))]

use async_trait::async_trait;
use endpoint_libs::libs::error_code::ErrorCode;
use endpoint_libs::libs::handler::{RequestHandler, Response};
use endpoint_libs::libs::peer::PeerIdentity;
use endpoint_libs::libs::toolbox::{ArcToolbox, CustomError, RequestContext};
use endpoint_libs::libs::ws::{
    AuthController, AuthReply, EndpointAuthController, MessageStream, NagoWssUpgrader,
    OnDisconnect, StreamError, SubAuthControllerErased, WebsocketServer, WebsocketStates,
    WsAuthController, WsConnection, WsMessage, WsRequest, WsResponse, WsResponseError,
    WsResponseValue, WsServerConfig, WsSuccessResponse, WsUpgrader,
};
use endpoint_libs::model::{EndpointSchema, TypeRegistry};
use eyre::Result;
use futures::future::LocalBoxFuture;
use futures::{FutureExt, StreamExt};
use nagoya::reactor::{Addr, Reactor, TcpListener, TcpStream, block_on_with};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct Auth {
    fail: bool,
    pending: bool,
}
impl AuthController for Auth {
    fn auth_reply(
        self: Arc<Self>,
        _: &ArcToolbox,
        _: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<AuthReply>> {
        async move {
            if self.pending {
                futures::future::pending::<()>().await;
            }
            if self.fail {
                eyre::bail!("fixture rejection");
            }
            conn.set_roles(Arc::new(vec![1]));
            Ok(AuthReply::Accepted(None))
        }
        .boxed_local()
    }
}

struct QueueFillingAuth;
impl AuthController for QueueFillingAuth {
    fn auth_reply(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        _: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<AuthReply>> {
        let toolbox = Arc::clone(toolbox);
        async move {
            // The waiting request must be eligible for dispatch if admission succeeds.
            conn.set_roles(Arc::new(vec![1]));
            assert!(toolbox.send(
                conn.connection_id,
                WsResponseValue::Error(Default::default()),
            ));
            Ok(AuthReply::Accepted(Some(WsSuccessResponse {
                method: 1,
                seq: 0,
                params: serde_json::value::to_raw_value(&()).unwrap(),
            })))
        }
        .boxed_local()
    }
}

struct Disconnect(Arc<AtomicUsize>);
#[async_trait(?Send)]
impl OnDisconnect for Disconnect {
    async fn on_disconnect(&self, _: u32, _: &PeerIdentity) {
        nagoya::sleep(Duration::from_millis(2)).await;
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct MemoryStream {
    incoming: futures::channel::mpsc::UnboundedReceiver<WsMessage>,
    outgoing: futures::channel::mpsc::UnboundedSender<WsMessage>,
}
#[async_trait(?Send)]
impl MessageStream for MemoryStream {
    async fn send(&mut self, msg: WsMessage) -> Result<(), StreamError> {
        self.outgoing
            .unbounded_send(msg)
            .map_err(|_| StreamError::Closed)
    }
    async fn recv(&mut self) -> Option<Result<WsMessage, StreamError>> {
        self.incoming.next().await.map(Ok)
    }
}
fn stream() -> (
    Box<dyn MessageStream>,
    futures::channel::mpsc::UnboundedSender<WsMessage>,
    futures::channel::mpsc::UnboundedReceiver<WsMessage>,
) {
    let (tx, incoming) = futures::channel::mpsc::unbounded();
    let (outgoing, rx) = futures::channel::mpsc::unbounded();
    (Box::new(MemoryStream { incoming, outgoing }), tx, rx)
}
fn text(message: WsMessage) -> String {
    match message {
        WsMessage::Text(value) => value.to_string(),
        _ => panic!("expected text response"),
    }
}
fn peer() -> PeerIdentity {
    PeerIdentity::Network("127.0.0.1:1".parse().unwrap())
}
async fn wait_count(count: &AtomicUsize, expected: usize) {
    nagoya::timeout(Duration::from_secs(3), async {
        while count.load(Ordering::SeqCst) != expected {
            nagoya::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("disconnect hook must finish exactly once");
}

#[test]
fn auth_failure_and_cancellation_remove_registered_state_and_dispatch_once() {
    let reactor = Reactor::local().unwrap();
    block_on_with(&reactor, async {
        for pending in [false, true] {
            let count = Arc::new(AtomicUsize::new(0));
            let mut server = WebsocketServer::new(WsServerConfig::default());
            server.set_auth_controller(Auth {
                fail: !pending,
                pending,
            });
            server.add_on_disconnect_hook(Disconnect(count.clone()));
            let states = Arc::new(WebsocketStates::new());
            server
                .toolbox
                .set_ws_states(states.clone_states(), false, false);
            let (stream, _tx, mut rx) = stream();
            let run = Arc::new(server).serve_connection(peer(), states.clone(), stream, None);
            if pending {
                assert!(
                    nagoya::timeout(Duration::from_millis(10), run)
                        .await
                        .is_err()
                );
            } else {
                run.await;
                assert!(
                    rx.next().await.is_some(),
                    "auth refusal must reach the transport"
                );
            }
            assert!(states.clone_states().is_empty(), "registered state leaked");
            wait_count(&count, 1).await;
        }
    });
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WaitRequest {
    pending: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct WaitResponse {
    done: bool,
}
impl WsRequest for WaitRequest {
    type Response = WaitResponse;
    const METHOD_ID: u32 = 1;
    const ROLES: &'static [u32] = &[1];
    const SCHEMA: &'static str = r#"{"name":"Wait","code":1,"parameters":[{"name":"pending","ty":"Boolean"}],"returns":[{"name":"done","ty":"Boolean"}],"roles":[]}"#;
}
impl WsResponse for WaitResponse {
    type Request = WaitRequest;
}
struct MethodWait;
#[async_trait(?Send)]
impl RequestHandler for MethodWait {
    type Request = WaitRequest;
    type Error = CustomError;
    async fn handle(&self, _: RequestContext, req: WaitRequest) -> Response<WaitRequest> {
        if req.pending {
            futures::future::pending::<()>().await;
        }
        Ok(WaitResponse { done: true })
    }
}

struct MethodWaitCountDispatch(Arc<AtomicUsize>);
#[async_trait(?Send)]
impl RequestHandler for MethodWaitCountDispatch {
    type Request = WaitRequest;
    type Error = CustomError;

    async fn handle(&self, _: RequestContext, _: WaitRequest) -> Response<WaitRequest> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(WaitResponse { done: true })
    }
}

#[test]
fn full_auth_queue_rejects_even_when_custom_toolbox_hook_returns_true() {
    let reactor = Reactor::local().unwrap();
    block_on_with(&reactor, async {
        let dispatched = Arc::new(AtomicUsize::new(0));
        let mut server = WebsocketServer::new(WsServerConfig {
            message_buffer_size: 1,
            ..Default::default()
        });
        server.set_auth_controller(QueueFillingAuth);
        server.add_handler(MethodWaitCountDispatch(dispatched.clone()));

        let states = Arc::new(WebsocketStates::new());
        let hook_states = states.clone_states();
        let hook_calls = AtomicUsize::new(0);
        assert!(
            server
                .toolbox
                .send_msg
                .set(Arc::new(move |connection_id, _| {
                    // Fill the queue on the first call and report success on every call.
                    if hook_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        hook_states
                            .get(&connection_id)
                            .unwrap()
                            .message_queue
                            .try_send(WsMessage::Text("occupied".into()))
                            .unwrap();
                    }
                    true
                }))
                .is_ok()
        );

        let (stream, tx, mut rx) = stream();
        tx.unbounded_send(WsMessage::Text(
            r#"{"method":1,"seq":9,"params":{"pending":false}}"#.into(),
        ))
        .unwrap();
        tx.unbounded_send(WsMessage::Close(None)).unwrap();

        Arc::new(server)
            .serve_connection(peer(), states, stream, None)
            .await;

        assert_eq!(
            dispatched.load(Ordering::SeqCst),
            0,
            "a request ran even though the auth reply could not be queued"
        );
        assert_eq!(text(rx.next().await.unwrap()), "occupied");
        assert!(rx.next().await.is_none());
    });
}

const AUTH_METHOD: u32 = 41;

struct ManualAuth {
    reject: bool,
}

fn auth_refusal(method: u32, log_id: u64, reason: &str) -> AuthReply {
    AuthReply::Rejected(Some(WsResponseError {
        method,
        seq: 0,
        log_id: log_id.to_string(),
        code: ErrorCode::FORBIDDEN.to_u32(),
        params: serde_json::json!({ "reason": reason }),
    }))
}

impl SubAuthControllerErased for ManualAuth {
    fn auth_reply(
        self: Arc<Self>,
        _: &ArcToolbox,
        _: serde_json::Value,
        ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, AuthReply> {
        async move {
            if self.reject {
                return auth_refusal(ctx.method, ctx.log_id, "handler");
            }
            conn.set_roles(Arc::new(vec![1]));
            AuthReply::Accepted(Some(WsSuccessResponse {
                method: ctx.method,
                seq: ctx.seq,
                params: serde_json::value::to_raw_value(&()).unwrap(),
            }))
        }
        .boxed_local()
    }
}

struct AuthWrapper {
    inner: Arc<EndpointAuthController>,
    reject: bool,
}

impl AuthController for AuthWrapper {
    fn auth_reply(
        self: Arc<Self>,
        toolbox: &ArcToolbox,
        header: String,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, Result<AuthReply>> {
        let log_id = conn.log_id;
        let inner_reply = Arc::clone(&self.inner).auth_reply(toolbox, header, conn);
        async move {
            match inner_reply.await? {
                AuthReply::Accepted(_) if self.reject => {
                    Ok(auth_refusal(AUTH_METHOD, log_id, "wrapper"))
                }
                reply => Ok(reply),
            }
        }
        .boxed_local()
    }
}

#[test]
fn deferred_auth_wrapper_and_erased_handler_gate_dispatch() {
    let reactor = Reactor::local().unwrap();
    block_on_with(&reactor, async {
        for (handler_rejects, wrapper_rejects, refusal) in [
            (false, false, None),
            (true, false, Some("handler")),
            (false, true, Some("wrapper")),
        ] {
            let mut inner = EndpointAuthController::new();
            inner.auth_endpoints.insert(
                "login".into(),
                WsAuthController {
                    schema: EndpointSchema::new("Login", AUTH_METHOD, vec![], vec![]),
                    handler: Arc::new(ManualAuth {
                        reject: handler_rejects,
                    }),
                },
            );
            let dispatched = Arc::new(AtomicUsize::new(0));
            let mut server = WebsocketServer::new(WsServerConfig {
                message_buffer_size: 1,
                ..Default::default()
            });
            server.set_auth_controller(AuthWrapper {
                inner: Arc::new(inner),
                reject: wrapper_rejects,
            });
            server.add_handler(MethodWaitCountDispatch(Arc::clone(&dispatched)));

            let states = Arc::new(WebsocketStates::new());
            server
                .toolbox
                .set_ws_states(states.clone_states(), false, false);
            let (stream, tx, mut rx) = stream();
            tx.unbounded_send(WsMessage::Text(
                r#"{"method":1,"seq":9,"params":{"pending":false}}"#.into(),
            ))
            .unwrap();

            let run = Arc::new(server).serve_connection(
                peer(),
                Arc::clone(&states),
                stream,
                Some("0login".into()),
            );
            let client = async move {
                let first = text(rx.next().await.expect("auth frame"));
                let auth: serde_json::Value = serde_json::from_str(&first).unwrap();
                assert_eq!(auth["method"], AUTH_METHOD);
                assert_eq!(auth["seq"], 0);
                if let Some(reason) = refusal {
                    assert_eq!(auth["type"], "Error");
                    assert_eq!(auth["code"], ErrorCode::FORBIDDEN.to_u32());
                    assert_eq!(auth["params"]["reason"], reason);
                    assert!(rx.next().await.is_none(), "only the rejection may be sent");
                } else {
                    assert_eq!(auth["type"], "Immediate");
                    let second = text(rx.next().await.expect("request reply"));
                    let reply: serde_json::Value = serde_json::from_str(&second).unwrap();
                    assert_eq!(reply["seq"], 9);
                    assert_eq!(reply["params"]["done"], true);
                }
                drop(tx);
            };
            nagoya::timeout(Duration::from_secs(3), futures::future::join(run, client))
                .await
                .expect("auth exchange must terminate");
            assert_eq!(
                dispatched.load(Ordering::SeqCst),
                refusal.is_none() as usize,
            );
            assert!(states.clone_states().is_empty());
        }
    });
}

#[test]
fn mcp_busy_timeout_and_slot_reuse_keep_the_original_ids() {
    let reactor = Reactor::local().unwrap();
    block_on_with(&reactor, async {
        let mut server = WebsocketServer::new(WsServerConfig {
            max_inflight_requests: Some(1),
            request_timeout: Some(Duration::from_millis(20)),
            ..Default::default()
        });
        server.set_auth_controller(Auth {
            fail: false,
            pending: false,
        });
        server.add_handler(MethodWait);
        let mut registry = TypeRegistry::new();
        registry.add_endpoint(&EndpointSchema::new("Wait", 1, vec![], vec![]));
        server
            .enable_mcp(
                &registry,
                endpoint_libs::libs::ws::mcp::McpServerInfo {
                    name: "acceptance".into(),
                    version: "1".into(),
                },
            )
            .unwrap();
        let states = Arc::new(WebsocketStates::new());
        server
            .toolbox
            .set_ws_states(states.clone_states(), false, false);
        let (stream, tx, mut rx) = stream();
        let run = Arc::new(server).serve_connection(peer(), states.clone(), stream, None);
        let body = async {
            tx.unbounded_send(WsMessage::Text(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"wait","arguments":{"pending":true}}}"#.into())).unwrap();
            tx.unbounded_send(WsMessage::Text(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"wait","arguments":{"pending":false}}}"#.into())).unwrap();
            let busy = text(rx.next().await.unwrap());
            assert!(busy.contains("\"id\":2"));
            assert!(format!("{busy:?}").contains("-32000"));
            let timeout = text(rx.next().await.unwrap());
            assert!(timeout.contains("\"id\":1"));
            assert!(format!("{timeout:?}").contains("-32001"));
            tx.unbounded_send(WsMessage::Text(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"wait","arguments":{"pending":false}}}"#.into())).unwrap();
            let success = text(rx.next().await.unwrap());
            assert!(success.contains("\"id\":3"));
            assert!(format!("{success:?}").contains("result"));
            tx.unbounded_send(WsMessage::Text(r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"wait","arguments":{"pending":false}}}"#.into())).unwrap();
            assert!(
                nagoya::timeout(Duration::from_millis(10), rx.next())
                    .await
                    .is_err()
            );
        };
        futures::pin_mut!(run, body);
        assert!(matches!(
            nagoya::timeout(Duration::from_secs(2), futures::future::select(body, run))
                .await
                .unwrap(),
            futures::future::Either::Left(_)
        ));
    });
}

#[test]
fn real_loopback_partial_http_upgrade_expires() {
    let reactor = Reactor::local().unwrap();
    let handle = reactor.handle();
    let listener = TcpListener::bind(Addr::localhost(0), &handle).unwrap();
    let addr = listener.local_addr().unwrap();
    block_on_with(&reactor, async {
        let server = async {
            let (stream, _) = listener.accept().await.unwrap();
            let config = WsServerConfig {
                handshake_timeout: Some(Duration::from_millis(15)),
                ..Default::default()
            };
            assert!(
                NagoWssUpgrader
                    .upgrade_stream(
                        Box::new(stream),
                        format!("127.0.0.1:{}", addr.port()).parse().unwrap(),
                        &config
                    )
                    .await
                    .is_err()
            );
        };
        let client = async {
            let mut stream = TcpStream::connect(addr, &handle).await.unwrap();
            stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        };
        nagoya::timeout(
            Duration::from_secs(2),
            futures::future::join(server, client),
        )
        .await
        .unwrap();
    });
}

#[test]
fn idle_session_expires_and_removes_state() {
    let reactor = Reactor::local().unwrap();
    block_on_with(&reactor, async {
        let mut server = WebsocketServer::new(WsServerConfig {
            idle_timeout: Some(Duration::from_millis(10)),
            ..Default::default()
        });
        server.set_auth_controller(Auth {
            fail: false,
            pending: false,
        });
        let states = Arc::new(WebsocketStates::new());
        server
            .toolbox
            .set_ws_states(states.clone_states(), false, false);
        let (stream, _tx, _rx) = stream();
        nagoya::timeout(
            Duration::from_secs(1),
            Arc::new(server).serve_connection(peer(), states.clone(), stream, None),
        )
        .await
        .unwrap();
        assert!(states.clone_states().is_empty());
    });
}

struct FailingListener(Arc<AtomicUsize>);
#[async_trait]
impl endpoint_libs::libs::ws::SessionListener for FailingListener {
    async fn accept(&self) -> Result<(Box<dyn MessageStream>, PeerIdentity)> {
        self.0.fetch_add(1, Ordering::SeqCst);
        eyre::bail!("fixture accept failure")
    }
}
#[test]
fn accept_errors_back_off_and_cancellation_stays_prompt() {
    let reactor = Reactor::local().unwrap();
    block_on_with(&reactor, async {
        let count = Arc::new(AtomicUsize::new(0));
        let server = WebsocketServer::new(WsServerConfig::default());
        assert!(
            nagoya::timeout(
                Duration::from_millis(100),
                server.serve_with(FailingListener(count.clone()))
            )
            .await
            .is_err()
        );
        assert!(
            (3..=5).contains(&count.load(Ordering::SeqCst)),
            "accept loop spun or stopped retrying"
        );
    });
}

struct SlowBefore;
#[async_trait(?Send)]
impl endpoint_libs::libs::ws::BeforeRequest for SlowBefore {
    async fn before(
        &self,
        _: &mut RequestContext,
        _: &EndpointSchema,
        _: &serde_json::Value,
    ) -> Result<(), CustomError> {
        futures::future::pending().await
    }
}
#[test]
fn legacy_before_hook_is_inside_the_request_deadline() {
    let reactor = Reactor::local().unwrap();
    block_on_with(&reactor, async {
        let mut server = WebsocketServer::new(WsServerConfig {
            request_timeout: Some(Duration::from_millis(10)),
            ..Default::default()
        });
        server.set_auth_controller(Auth {
            fail: false,
            pending: false,
        });
        server.add_handler(MethodWait);
        server.add_before_hook(SlowBefore);
        let states = Arc::new(WebsocketStates::new());
        server
            .toolbox
            .set_ws_states(states.clone_states(), false, false);
        let (stream, tx, mut rx) = stream();
        let run = Arc::new(server).serve_connection(peer(), states, stream, None);
        let body = async {
            tx.unbounded_send(WsMessage::Text(
                r#"{"method":1,"seq":9,"params":{"pending":false}}"#.into(),
            ))
            .unwrap();
            let reply = rx.next().await.unwrap();
            assert!(format!("{reply:?}").contains("100408"));
        };
        futures::pin_mut!(run, body);
        assert!(matches!(
            nagoya::timeout(Duration::from_secs(1), futures::future::select(body, run))
                .await
                .unwrap(),
            futures::future::Either::Left(_)
        ));
    });
}

#[test]
fn thousand_refused_auth_sessions_leave_no_registered_states() {
    let reactor = Reactor::local().unwrap();
    block_on_with(&reactor, async {
        let count = Arc::new(AtomicUsize::new(0));
        let mut server = WebsocketServer::new(WsServerConfig::default());
        server.set_auth_controller(Auth {
            fail: true,
            pending: false,
        });
        server.add_on_disconnect_hook(Disconnect(count.clone()));
        let states = Arc::new(WebsocketStates::new());
        server
            .toolbox
            .set_ws_states(states.clone_states(), false, false);
        let server = Arc::new(server);
        for _ in 0..1000 {
            let (stream, _tx, mut rx) = stream();
            server
                .clone()
                .serve_connection(peer(), states.clone(), stream, None)
                .await;
            assert!(states.clone_states().is_empty());
            assert!(rx.next().await.is_some());
        }
        wait_count(&count, 1000).await;
        nagoya::sleep(Duration::from_millis(10)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1000);
    });
}
