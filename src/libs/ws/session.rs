use eyre::Result;
use futures::StreamExt;
use futures::future::{FutureExt, LocalBoxFuture};
use futures::stream::FuturesUnordered;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::*;

use crate::libs::signal::Shutdown;

/// What the session loop does after one inbound frame.
///
/// Handler bodies are polled in place on this task (`Task`) so a slow hook
/// cannot stall the read loop, without `spawn_local` and the LocalSet it
/// needs. `serve_with` already drives connections the same way.
enum Dispatch {
    Close,
    Keep,
    Task(LocalBoxFuture<'static, ()>),
}

use crate::libs::ws::WsMessage as Message;

use crate::libs::error_code::ErrorCode;
use crate::libs::toolbox::{RequestContext, TOOLBOX};

use super::mcp::{
    self, JsonRpcError, JsonRpcId, JsonRpcRequest, McpAction, McpCallCtx, McpState,
    encode_tool_error, jsonrpc_error, jsonrpc_result,
};
use super::{
    MessageStream, RequestOutcome, StreamError, WebsocketServer, WsConnection, WsRequestValue,
    WsResponseError, WsResponseValue,
};

pub struct WsClientSession {
    conn_info: Arc<WsConnection>,
    conn: Box<dyn MessageStream>,
    rx: mpsc::Receiver<Message>,
    server: Arc<WebsocketServer>,
    /// Policy close, shared with [`crate::libs::ws::WsStreamState::end`].
    /// A session built with [`WsClientSession::new`] and never [`WsClientSession::bind_end`]
    /// holds a flag nobody else can set.
    end: Arc<Shutdown>,
}

impl WsClientSession {
    pub fn new(
        conn_info: Arc<WsConnection>,
        conn: Box<dyn MessageStream>,
        rx: mpsc::Receiver<Message>,
        server: Arc<WebsocketServer>,
    ) -> Self {
        Self {
            conn_info,
            conn,
            rx,
            server,
            end: Arc::new(Shutdown::new()),
        }
    }

    /// Use the connection's policy-close flag. [`WebsocketServer`] does this
    /// after `WebsocketStates::insert` has created the flag.
    pub fn bind_end(&mut self, end: Arc<Shutdown>) {
        self.end = end;
    }

    pub fn conn(&self) -> &dyn MessageStream {
        self.conn.as_ref()
    }

    pub async fn run(mut self) {
        let addr = self.conn_info.peer.display();
        let conn_id = self.conn_info.connection_id;
        if let Err(err) = self.run_loop().await {
            error!(
                ws_server = true,
                ?err,
                ?addr,
                ?conn_id,
                "Failed to run websocket session"
            );
        }
    }

    fn handle_message(&mut self, msg: Message) -> Result<Dispatch> {
        let addr = self.conn_info.peer.display();
        let mut context = RequestContext::from_conn(&self.conn_info);

        // MCP: route JSON-RPC 2.0 frames to the MCP adapter when enabled.
        // Detection is unambiguous: legacy frames ({method: u32, seq, params})
        // can never carry a top-level "jsonrpc": "2.0" member.
        if let Some(mcp) = &self.server.mcp {
            let payload = match &msg {
                Message::Text(t) => Some(t.as_str()),
                Message::Binary(b) => std::str::from_utf8(b).ok(),
                _ => None,
            };
            if let Some(frame) = payload.and_then(mcp::try_parse_jsonrpc) {
                let mcp = Arc::clone(mcp);
                if let Some(task) = self.handle_mcp_frame(mcp, frame, context) {
                    return Ok(Dispatch::Task(task));
                }
                return Ok(Dispatch::Keep);
            }
        }

        if self.server.config.mcp_only {
            self.server.toolbox.send_raw(
                context.connection_id,
                jsonrpc_error(
                    &None,
                    JsonRpcError::new(
                        mcp::INVALID_REQUEST,
                        "This WebSocket accepts MCP JSON-RPC 2.0 frames only",
                    ),
                )
                .to_string(),
            );
            return Ok(Dispatch::Keep);
        }

        #[allow(unreachable_patterns)]
        let obj: Result<WsRequestValue, _> = match msg {
            Message::Text(t) => {
                debug!(
                    ws_server = true,
                    ?addr,
                    bytes = t.len(),
                    "Handling text request"
                );
                serde_json::from_str(&t)
            }
            Message::Binary(b) => {
                debug!(
                    ws_server = true,
                    ?addr,
                    bytes = b.len(),
                    "Handling binary request"
                );
                serde_json::from_slice(&b)
            }
            Message::Ping(_) => {
                return Ok(Dispatch::Keep);
            }
            Message::Pong(_) => {
                return Ok(Dispatch::Keep);
            }
            Message::Close(_) => {
                debug!(ws_server = true, ?addr, "Receive side terminated");
                return Ok(Dispatch::Close);
            }
            _ => {
                warn!(
                    ws_server = true,
                    ?addr,
                    "Ignoring unsupported WebSocket frame"
                );
                return Ok(Dispatch::Keep);
            }
        };
        let req = match obj {
            Ok(req) => req,
            Err(err) => {
                self.server.toolbox.send(
                    context.connection_id,
                    WsResponseValue::Error(WsResponseError {
                        method: context.method,
                        code: ErrorCode::BAD_REQUEST.to_u32(),
                        seq: context.seq,
                        log_id: context.log_id.to_string(),
                        params: serde_json::json!({
                            "kind": ErrorCode::BAD_REQUEST.kind(),
                            "message": err.to_string(),
                        }),
                    }),
                );
                return Ok(Dispatch::Keep);
            }
        };
        debug!(
            ws_server = true,
            ?addr,
            method = req.method,
            seq = req.seq,
            "Parsed WebSocket request"
        );
        context.seq = req.seq;
        context.method = req.method;
        context.user_id = self.conn_info.get_user_id();
        context.roles = self.conn_info.get_roles();

        let Some(endpoint) = self.server.handlers.get(&req.method) else {
            self.server.toolbox.send(
                context.connection_id,
                WsResponseValue::Error(WsResponseError {
                    method: context.method,
                    code: ErrorCode::NOT_IMPLEMENTED.to_u32(),
                    seq: context.seq,
                    log_id: context.log_id.to_string(),
                    params: serde_json::json!({
                        "kind": ErrorCode::NOT_IMPLEMENTED.kind(),
                        "message": "Method not implemented",
                    }),
                }),
            );
            return Ok(Dispatch::Keep);
        };

        if !check_roles(&context.roles, &endpoint.allowed_roles) {
            self.server.toolbox.send(
                context.connection_id,
                WsResponseValue::Error(WsResponseError {
                    method: context.method,
                    code: ErrorCode::FORBIDDEN.to_u32(),
                    seq: context.seq,
                    log_id: context.log_id.to_string(),
                    params: serde_json::json!({
                        "kind": ErrorCode::FORBIDDEN.kind(),
                        "message": "Forbidden",
                    }),
                }),
            );
            return Ok(Dispatch::Keep);
        }

        let handler = endpoint.handler.clone();
        let toolbox = self.server.toolbox.clone();
        let hooks = self.server.hooks.clone();
        let schema = endpoint.schema.clone();
        Ok(Dispatch::Task(
            async move {
                let mut context = context;
                // Polled alongside the session read loop so a slow hook cannot
                // stall it, and after check_roles so they only see calls that
                // were already allowed to reach this endpoint.
                if let Err(custom) = hooks.run_before(&mut context, &schema, &req.params).await {
                    let code = custom.code.to_u32();
                    toolbox.send(
                        context.connection_id,
                        WsResponseValue::Error(WsResponseError {
                            method: context.method,
                            code,
                            seq: context.seq,
                            log_id: context.log_id.to_string(),
                            params: custom.params.clone(),
                        }),
                    );
                    hooks
                        .run_after(&context, &schema, &RequestOutcome::PublicErr { code })
                        .await;
                    return;
                }

                TOOLBOX
                    .scope(
                        toolbox.clone(),
                        handler.handle(&toolbox, context.clone(), req.params),
                    )
                    .await;

                // The erased handler reports its own outcome through the toolbox, so
                // AfterRequest observes completion rather than the specific result here.
                hooks
                    .run_after(&context, &schema, &RequestOutcome::Ok)
                    .await;
            }
            .boxed_local(),
        ))
    }

    /// Handles one parsed JSON-RPC frame: lifecycle methods are answered
    /// inline from [`McpState`]; `tools/call` dispatches to the endpoint's
    /// request handler via [`RequestHandlerErased::handle_mcp`].
    ///
    /// [`RequestHandlerErased::handle_mcp`]: crate::libs::handler::RequestHandlerErased::handle_mcp
    fn handle_mcp_frame(
        &mut self,
        mcp: Arc<McpState>,
        frame: Result<JsonRpcRequest, serde_json::Value>,
        mut context: RequestContext,
    ) -> Option<LocalBoxFuture<'static, ()>> {
        let conn_id = context.connection_id;
        let req = match frame {
            Ok(req) => req,
            Err(error_frame) => {
                self.server
                    .toolbox
                    .send_raw(conn_id, error_frame.to_string());
                return None;
            }
        };

        context.user_id = self.conn_info.get_user_id();
        context.roles = self.conn_info.get_roles();

        match mcp.route(req, &context.roles) {
            McpAction::Respond(frame) => {
                self.server.toolbox.send_raw(conn_id, frame.to_string());
                None
            }
            McpAction::Ignore => None,
            McpAction::ToolCall {
                id,
                method_code,
                arguments,
            } => {
                context.method = method_code;
                // Numeric ids that fit u32 double as the legacy seq for logging.
                if let Some(JsonRpcId::Num(n)) = &id
                    && let Ok(seq) = u32::try_from(*n)
                {
                    context.seq = seq;
                }

                let Some(endpoint) = self.server.handlers.get(&method_code) else {
                    // Unreachable in practice: McpState is built from handlers.
                    self.server.toolbox.send_raw(
                        conn_id,
                        jsonrpc_error(
                            &id,
                            JsonRpcError::new(mcp::METHOD_NOT_FOUND, "Method not found"),
                        )
                        .to_string(),
                    );
                    return None;
                };

                let handler = endpoint.handler.clone();
                let toolbox = self.server.toolbox.clone();
                let hooks = self.server.hooks.clone();
                let schema = endpoint.schema.clone();
                Some(
                    async move {
                        let mut context = context;
                        // Same placement as the legacy path, but the rejection has to go
                        // back in the MCP envelope — a tool error, not a WsResponseError.
                        if let Err(custom) =
                            hooks.run_before(&mut context, &schema, &arguments).await
                        {
                            let code = custom.code.to_u32();
                            toolbox.send_raw(
                                conn_id,
                                jsonrpc_result(&id, encode_tool_error(custom.code, &custom.params))
                                    .to_string(),
                            );
                            hooks
                                .run_after(&context, &schema, &RequestOutcome::PublicErr { code })
                                .await;
                            return;
                        }

                        TOOLBOX
                            .scope(
                                toolbox.clone(),
                                handler.handle_mcp(
                                    &toolbox,
                                    context.clone(),
                                    McpCallCtx { id },
                                    arguments,
                                ),
                            )
                            .await;

                        hooks
                            .run_after(&context, &schema, &RequestOutcome::Ok)
                            .await;
                    }
                    .boxed_local(),
                )
            }
        }
    }

    async fn run_loop(&mut self) -> Result<()> {
        let conn_id = self.conn_info.connection_id;
        let mut handlers = FuturesUnordered::new();
        // Owned so the recv futures can drop before the arms borrow `self` again.
        // `pin_mut` holds those borrows to the end of its scope.
        enum Ready {
            Outbound(Outbound),
            Inbound(Option<std::result::Result<Message, StreamError>>),
            Handler,
            End,
        }
        loop {
            while let Ok(msg) = self.rx.try_recv() {
                if !self.send_message(msg).await {
                    return Ok(());
                }
                if self.server.config.header_only {
                    return Ok(());
                }
            }
            // After the queue is drained. A payload already queued is sent
            // above; the flag only wins once there is nothing left to send.
            if self.end.is_cancelled() {
                debug!(ws_server = true, ?conn_id, "Session end flagged");
                return Ok(());
            }

            // Outbound, then inbound, then a finished handler, then the policy
            // flag. The handler arm stays disabled while the set is empty.
            // The flag is last so a frame that is already ready is not dropped
            // on the floor when the flag trips in the same poll.
            let ready = {
                let outbound = self.rx.recv();
                futures::pin_mut!(outbound);
                let inbound = self.conn.recv();
                futures::pin_mut!(inbound);
                let end = self.end.cancelled();
                futures::pin_mut!(end);
                if handlers.is_empty() {
                    match futures::future::select(outbound, futures::future::select(inbound, end))
                        .await
                    {
                        futures::future::Either::Left((msg, _)) => {
                            Ready::Outbound(classify_outbound(msg))
                        }
                        futures::future::Either::Right((inner, _)) => match inner {
                            futures::future::Either::Left((msg, _)) => Ready::Inbound(msg),
                            futures::future::Either::Right(_) => Ready::End,
                        },
                    }
                } else {
                    let handler = handlers.next();
                    futures::pin_mut!(handler);
                    match futures::future::select(
                        outbound,
                        futures::future::select(inbound, futures::future::select(handler, end)),
                    )
                    .await
                    {
                        futures::future::Either::Left((msg, _)) => {
                            Ready::Outbound(classify_outbound(msg))
                        }
                        futures::future::Either::Right((inner, _)) => match inner {
                            futures::future::Either::Left((msg, _)) => Ready::Inbound(msg),
                            futures::future::Either::Right((handler_or_end, _)) => {
                                match handler_or_end {
                                    futures::future::Either::Left(_) => Ready::Handler,
                                    futures::future::Either::Right(_) => Ready::End,
                                }
                            }
                        },
                    }
                }
            };
            match ready {
                Ready::Outbound(Outbound::Frame(msg)) => {
                    if !self.send_message(msg).await {
                        break;
                    }
                    if self.server.config.header_only {
                        break;
                    }
                }
                Ready::Outbound(Outbound::Closed) => {
                    debug!(ws_server = true, ?conn_id, "Outbound channel closed");
                    break;
                }
                Ready::End => {
                    debug!(ws_server = true, ?conn_id, "Session end flagged");
                    break;
                }
                Ready::Inbound(Some(msg_result)) => {
                    let msg = match msg_result {
                        Ok(m) => m,
                        Err(StreamError::Closed) => {
                            debug!(ws_server = true, ?conn_id, "WS receive: connection closed");
                            break;
                        }
                        Err(StreamError::Protocol(e)) => {
                            warn!(ws_server = true, ?conn_id, err=%e, "WS protocol error on receive");
                            break;
                        }
                        Err(StreamError::WriteBufferFull) => {
                            warn!(
                                ws_server = true,
                                ?conn_id,
                                "WS write buffer full on receive"
                            );
                            break;
                        }
                        Err(StreamError::Other(e)) => {
                            error!(ws_server = true, ?conn_id, err=%e, "WS receive error");
                            break;
                        }
                    };
                    match self.handle_message(msg)? {
                        Dispatch::Close => break,
                        Dispatch::Keep => {}
                        Dispatch::Task(task) => handlers.push(task),
                    }
                }
                Ready::Inbound(None) => {
                    debug!(ws_server = true, ?conn_id, "Inbound stream ended");
                    break;
                }
                Ready::Handler => {}
            }
        }

        Ok(())
    }

    async fn send_message(&mut self, msg: Message) -> bool {
        let conn_id = self.conn_info.connection_id;
        match self.conn.send(msg).await {
            Ok(()) => true,
            Err(StreamError::Closed) => {
                debug!(ws_server = true, ?conn_id, "WS send: connection closed");
                false
            }
            Err(StreamError::WriteBufferFull) => {
                warn!(ws_server = true, ?conn_id, "WS send: write buffer full");
                false
            }
            Err(StreamError::Protocol(e)) => {
                warn!(ws_server = true, ?conn_id, err=%e, "WS send: protocol error");
                false
            }
            Err(StreamError::Other(e)) => {
                error!(ws_server = true, ?conn_id, err=%e, "WS send error");
                false
            }
        }
    }
}

/// What an outbound `recv` means. `Closed` is the teardown edge: every sender
/// is gone. It is not a frame and it ends the session.
pub(crate) enum Outbound {
    Frame(Message),
    Closed,
}

pub(crate) fn classify_outbound(msg: Option<Message>) -> Outbound {
    match msg {
        Some(frame) => Outbound::Frame(frame),
        None => Outbound::Closed,
    }
}

fn check_roles(actual_roles: &[u32], allowed_roles: &HashSet<u32>) -> bool {
    if allowed_roles.is_empty() || actual_roles.is_empty() {
        return false;
    }
    actual_roles.iter().any(|role| allowed_roles.contains(role))
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_roles_allowed() {
        use super::check_roles;
        use std::collections::HashSet;

        let allowed_roles: HashSet<u32> = [1, 2, 3].iter().cloned().collect();
        assert!(check_roles(&[1], &allowed_roles.clone()));
        assert!(check_roles(&[2], &allowed_roles.clone()));
        assert!(check_roles(&[1, 2], &allowed_roles.clone()));
        assert!(check_roles(&[4, 2], &allowed_roles.clone()));

        assert!(!check_roles(&[4], &allowed_roles.clone()));
    }

    #[test]
    fn check_roles_empty() {
        use super::check_roles;
        use std::collections::HashSet;

        let allowed_roles: HashSet<u32> = HashSet::new();
        assert!(!check_roles(&[1], &allowed_roles));
    }

    #[test]
    fn closed_outbound_is_not_a_frame() {
        use super::{Outbound, classify_outbound};
        use crate::libs::ws::WsMessage as Message;

        assert!(matches!(classify_outbound(None), Outbound::Closed));
        assert!(matches!(
            classify_outbound(Some(Message::from("hi"))),
            Outbound::Frame(_)
        ));
    }

    struct PendingStream {
        parked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait(?Send)]
    impl super::MessageStream for PendingStream {
        async fn send(
            &mut self,
            _msg: super::Message,
        ) -> std::result::Result<(), super::StreamError> {
            Ok(())
        }

        async fn recv(
            &mut self,
        ) -> Option<std::result::Result<super::Message, super::StreamError>> {
            self.parked
                .store(true, std::sync::atomic::Ordering::Release);
            std::future::pending().await
        }
    }

    fn test_conn() -> std::sync::Arc<super::WsConnection> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        use parking_lot::RwLock;

        use crate::libs::peer::{Extensions, PeerIdentity};

        Arc::new(super::WsConnection {
            connection_id: 1,
            user_id: AtomicU64::new(0),
            roles: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            peer: PeerIdentity::Unknown,
            extensions: Extensions::new(),
            log_id: 0,
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_the_outbound_sender_ends_the_session() {
        use std::time::Duration;

        use tokio::sync::mpsc;

        use super::WsClientSession;
        use crate::libs::ws::{WebsocketServer, WsServerConfig};

        let (tx, rx) = mpsc::channel(1);
        drop(tx);
        let session = WsClientSession::new(
            test_conn(),
            Box::new(PendingStream {
                parked: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
            rx,
            std::sync::Arc::new(WebsocketServer::new(WsServerConfig::default())),
        );
        let finished = tokio::time::timeout(Duration::from_secs(1), session.run()).await;
        assert!(
            finished.is_ok(),
            "session kept running after every outbound sender was dropped"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn policy_flag_ends_a_session_blocked_on_recv() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        use tokio::sync::mpsc;

        use super::WsClientSession;
        use crate::libs::signal::Shutdown;
        use crate::libs::ws::{WebsocketServer, WsServerConfig};

        let (_tx, rx) = mpsc::channel(1);
        let parked = Arc::new(AtomicBool::new(false));
        let end = Arc::new(Shutdown::new());
        let mut session = WsClientSession::new(
            test_conn(),
            Box::new(PendingStream {
                parked: Arc::clone(&parked),
            }),
            rx,
            Arc::new(WebsocketServer::new(WsServerConfig::default())),
        );
        session.bind_end(Arc::clone(&end));

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let running = tokio::task::spawn_local(async move { session.run().await });
                tokio::time::timeout(Duration::from_secs(1), async {
                    while !parked.load(Ordering::Acquire) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("session never waited on the connection");
                end.cancel();
                let finished = tokio::time::timeout(Duration::from_secs(1), running).await;
                assert!(
                    finished.is_ok(),
                    "session kept running after the policy flag was set"
                );
            })
            .await;
    }
}
