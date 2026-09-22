use crate::libs::peer::{Extensions, PeerIdentity};
use crate::libs::ws::WsMessage as Message;
use dashmap::DashMap;
use eyre::Result;
use serde::*;
use serde_json::{Map, Value};
use std::cell::Cell;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use tracing::*;

use crate::libs::error_code::ErrorCode;
use crate::libs::handler::HandlerError;
use crate::libs::log::LogLevel;
use crate::libs::signal::Shutdown;
use crate::libs::ws::outbound::{self, Sender};
use crate::libs::ws::{
    ConnectionId, WsConnection, WsLogResponse, WsRequest, WsResponseError, WsResponseValue,
    WsStreamState, WsSuccessResponse, custom_error_to_resp, internal_error_to_resp,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NoResponseError;

impl Display for NoResponseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("NoResp")
    }
}

impl std::error::Error for NoResponseError {}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CustomError {
    pub code: ErrorCode,
    pub params: Value,
}

impl CustomError {
    pub fn new(code: impl Into<ErrorCode>) -> Self {
        let code = code.into();
        Self {
            code,
            params: Value::Object(Map::new()),
        }
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        if let Value::Object(map) = &mut self.params {
            map.insert("message".to_owned(), Value::String(message.into()));
        }
        self
    }

    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        if let Value::Object(map) = &mut self.params {
            map.insert("kind".to_owned(), Value::String(kind.into()));
        }
        self
    }

    pub fn with_details(mut self, details: impl Serialize) -> Self {
        let details = serde_json::to_value(details).unwrap_or(Value::Null);
        let Value::Object(params) = &mut self.params else {
            return self;
        };

        match details {
            Value::Object(details) => {
                for (key, value) in details {
                    if key != "kind" && key != "message" {
                        params.insert(key, value);
                    }
                }
            }
            Value::Null => {}
            value => {
                params.insert("details".to_owned(), value);
            }
        }
        self
    }

    pub fn from_sql_error(err: &str, msg: impl Display) -> Result<Self> {
        let code = u32::from_str_radix(err, 36)?;
        Ok(Self::new(ErrorCode::new(code)).with_message(msg.to_string()))
    }
}

impl Display for CustomError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.params.to_string())
    }
}

impl std::error::Error for CustomError {}

#[derive(Clone)]
pub struct RequestContext {
    pub connection_id: ConnectionId,
    pub user_id: u64,
    pub seq: u32,
    pub method: u32,
    pub log_id: u64,
    pub roles: Arc<Vec<u32>>,
    /// Best-effort IP, kept for compatibility: most consumers only log it.
    ///
    /// Populated from [`PeerIdentity::ip_addr`], so local peers report loopback.
    /// Prefer [`Self::peer`] when the distinction matters.
    pub ip_addr: IpAddr,
    /// Who issued this request. Carries attestation for local transports.
    pub peer: PeerIdentity,
    /// Request-scoped data. `BeforeRequest` hooks attach verified claims here;
    /// handlers read them back.
    pub extensions: Extensions,
}

impl RequestContext {
    pub fn empty() -> Self {
        Self {
            connection_id: 0,
            user_id: 0,
            seq: 0,
            method: 0,
            log_id: 0,
            roles: Arc::new(Vec::new()),
            ip_addr: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            peer: PeerIdentity::Unknown,
            extensions: Extensions::new(),
        }
    }
    pub fn from_conn(conn: &WsConnection) -> Self {
        let roles = conn.roles.read().clone();
        Self {
            connection_id: conn.connection_id,
            user_id: conn.get_user_id(),
            seq: 0,
            method: 0,
            log_id: conn.log_id,
            roles,
            ip_addr: conn.peer.ip_addr(),
            peer: conn.peer.clone(),
            extensions: conn.extensions.clone(),
        }
    }
}

type SendFn = dyn Fn(ConnectionId, WsResponseValue) -> bool + Send + Sync;
type SendFnArc = Arc<SendFn>;
type SendRawFn = dyn Fn(ConnectionId, String) -> bool + Send + Sync;
type SendRawFnArc = Arc<SendRawFn>;

pub struct Toolbox {
    pub send_msg: OnceLock<SendFnArc>,
    pub send_raw_msg: OnceLock<SendRawFnArc>,
}
pub type ArcToolbox = Arc<Toolbox>;
impl Toolbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            send_msg: OnceLock::new(),
            send_raw_msg: OnceLock::new(),
        })
    }

    pub fn set_ws_states(
        &self,
        states: Arc<DashMap<ConnectionId, Arc<WsStreamState>>>,
        oneshot: bool,
        drop_on_buffer_full: bool,
    ) {
        let raw_states = Arc::clone(&states);
        let send_fn: SendFnArc = Arc::new(move |conn_id, msg| {
            let state = if let Some(state) = states.get(&conn_id) {
                state
            } else {
                return false;
            };
            let serialized = match serde_json::to_string(&msg) {
                Ok(serialized) => serialized,
                Err(e) => {
                    error!(ws_server = true, conn_id, err=%e, "Failed to serialize WS response — dropping message");
                    return true;
                }
            };
            Self::enqueue(
                &state.message_queue,
                Some(&state.end),
                serialized,
                oneshot,
                conn_id,
                drop_on_buffer_full,
            );
            true
        });
        if self.send_msg.set(send_fn).is_err() {
            warn!(
                ws_server = true,
                "set_ws_states called twice — ignoring second call"
            );
        }
        // Pre-serialized frames (JSON-RPC/MCP responses) bypass WsResponseValue.
        let send_raw_fn: SendRawFnArc = Arc::new(move |conn_id, serialized| {
            let state = if let Some(state) = raw_states.get(&conn_id) {
                state
            } else {
                return false;
            };
            Self::enqueue(
                &state.message_queue,
                Some(&state.end),
                serialized,
                oneshot,
                conn_id,
                drop_on_buffer_full,
            );
            true
        });
        let _ = self.send_raw_msg.set(send_raw_fn);
    }

    pub fn send_ws_msg(
        sender: &Sender<Message>,
        resp: WsResponseValue,
        oneshot: bool,
        conn_id: ConnectionId,
        drop_on_full: bool,
    ) {
        let serialized = match serde_json::to_string(&resp) {
            Ok(s) => s,
            Err(e) => {
                error!(ws_server = true, conn_id, err=%e, "Failed to serialize WS response — dropping message");
                return;
            }
        };
        Self::enqueue(sender, None, serialized, oneshot, conn_id, drop_on_full)
    }

    pub fn send_serialized_ws_msg(
        sender: &Sender<Message>,
        serialized: String,
        oneshot: bool,
        conn_id: ConnectionId,
        drop_on_full: bool,
    ) {
        Self::enqueue(sender, None, serialized, oneshot, conn_id, drop_on_full)
    }

    /// Queue a frame, and on a policy close signal `end` as well.
    ///
    /// The `Close` frame stays ordered behind whatever is already queued.
    /// `end` is what still fires when that `try_send` cannot take a slot.
    /// Callers that pass `None` keep the old queue-only behaviour.
    fn enqueue(
        sender: &Sender<Message>,
        end: Option<&Shutdown>,
        serialized: String,
        oneshot: bool,
        conn_id: ConnectionId,
        drop_on_full: bool,
    ) {
        match sender.try_send(serialized.into()) {
            Ok(()) => {}
            Err(outbound::TrySendError::Full(_)) => {
                error!(
                    ws_server = true,
                    conn_id, "Send buffer full — client too slow or disconnected"
                );
                if drop_on_full {
                    Self::close_for_policy(sender, end);
                }
            }
            Err(outbound::TrySendError::Closed(_)) => {
                debug!(
                    ws_server = true,
                    conn_id, "Send channel closed — client already disconnected"
                );
            }
        }
        if oneshot {
            Self::close_for_policy(sender, end);
        }
    }

    fn close_for_policy(sender: &Sender<Message>, end: Option<&Shutdown>) {
        let _ = sender.try_send(Message::Close(None));
        if let Some(end) = end {
            end.cancel();
        }
    }
    pub fn send(&self, conn_id: ConnectionId, resp: WsResponseValue) -> bool {
        match self.send_msg.get() {
            Some(f) => f(conn_id, resp),
            None => false,
        }
    }
    /// Sends a pre-serialized frame (e.g. a JSON-RPC/MCP response) as-is.
    pub fn send_raw(&self, conn_id: ConnectionId, serialized: String) -> bool {
        match self.send_raw_msg.get() {
            Some(f) => f(conn_id, serialized),
            None => false,
        }
    }
    pub fn send_response(&self, ctx: &RequestContext, resp: impl Serialize) {
        let params = match serde_json::value::to_raw_value(&resp) {
            Ok(p) => p,
            Err(e) => {
                error!(ws_server = true, conn_id=ctx.connection_id, err=%e, "Failed to serialize response — sending error to client");
                self.send(
                    ctx.connection_id,
                    WsResponseValue::Error(WsResponseError {
                        method: ctx.method,
                        code: ErrorCode::INTERNAL_ERROR.to_u32(),
                        seq: ctx.seq,
                        log_id: ctx.log_id.to_string(),
                        params: serde_json::json!({
                            "kind": ErrorCode::INTERNAL_ERROR.kind(),
                            "message": "Failed to serialize response",
                        }),
                    }),
                );
                return;
            }
        };
        self.send(
            ctx.connection_id,
            WsResponseValue::Immediate(WsSuccessResponse {
                method: ctx.method,
                seq: ctx.seq,
                params,
            }),
        );
    }
    pub fn send_internal_error(&self, ctx: &RequestContext, code: ErrorCode, err: eyre::Error) {
        self.send(ctx.connection_id, internal_error_to_resp(ctx, code, err));
    }
    pub fn send_request_error(&self, ctx: &RequestContext, code: ErrorCode, err: impl Display) {
        self.send(
            ctx.connection_id,
            WsResponseValue::Error(WsResponseError {
                method: ctx.method,
                code: code.to_u32(),
                seq: ctx.seq,
                log_id: ctx.log_id.to_string(),
                params: serde_json::json!({
                    "kind": code.kind(),
                    "message": err.to_string(),
                }),
            }),
        );
    }
    pub fn send_log(&self, ctx: &RequestContext, level: LogLevel, msg: impl Into<String>) {
        self.send(
            ctx.connection_id,
            WsResponseValue::Log(WsLogResponse {
                seq: ctx.seq,
                log_id: ctx.log_id,
                level,
                message: msg.into(),
            }),
        );
    }
    pub fn encode_ws_response<Resp: Serialize>(
        ctx: RequestContext,
        resp: Result<Resp>,
    ) -> Option<WsResponseValue> {
        #[allow(unused_variables)]
        let RequestContext {
            connection_id,
            user_id,
            seq,
            method,
            log_id,
            ..
        } = ctx;
        let resp = match resp {
            Ok(ok) => match serde_json::value::to_raw_value(&ok) {
                Ok(params) => WsResponseValue::Immediate(WsSuccessResponse {
                    method,
                    seq,
                    params,
                }),
                Err(e) => {
                    error!(ws_server = true, connection_id, err=%e, "Failed to serialize response — sending error to client");
                    WsResponseValue::Error(WsResponseError {
                        method,
                        code: ErrorCode::INTERNAL_ERROR.to_u32(),
                        seq,
                        log_id: log_id.to_string(),
                        params: serde_json::json!({
                            "kind": ErrorCode::INTERNAL_ERROR.kind(),
                            "message": "Failed to serialize response",
                        }),
                    })
                }
            },
            Err(err) if err.is::<NoResponseError>() => {
                return None;
            }
            Err(err) => match err.downcast::<CustomError>() {
                Ok(err) => custom_error_to_resp(&ctx, err),
                Err(err) => internal_error_to_resp(&ctx, ErrorCode::INTERNAL_ERROR, err),
            },
        };
        Some(resp)
    }

    pub fn encode_handler_response<Req, Err>(
        ctx: RequestContext,
        resp: crate::libs::handler::Response<Req, Err>,
    ) -> Option<WsResponseValue>
    where
        Req: WsRequest,
        Err: Into<CustomError>,
    {
        #[allow(unused_variables)]
        let RequestContext {
            connection_id,
            user_id,
            seq,
            method,
            log_id,
            ..
        } = ctx;
        let resp = match resp {
            Ok(ok) => match serde_json::value::to_raw_value(&ok) {
                Ok(params) => WsResponseValue::Immediate(WsSuccessResponse {
                    method,
                    seq,
                    params,
                }),
                Err(e) => {
                    error!(ws_server = true, connection_id, err=%e, "Failed to serialize response — sending error to client");
                    WsResponseValue::Error(WsResponseError {
                        method,
                        code: ErrorCode::INTERNAL_ERROR.to_u32(),
                        seq,
                        log_id: log_id.to_string(),
                        params: serde_json::json!({
                            "kind": ErrorCode::INTERNAL_ERROR.kind(),
                            "message": "Failed to serialize response",
                        }),
                    })
                }
            },
            Err(HandlerError::Public(err)) => {
                let err = err.into();
                custom_error_to_resp(&ctx, err)
            }
            Err(HandlerError::Internal(err)) => {
                internal_error_to_resp(&ctx, ErrorCode::INTERNAL_ERROR, err)
            }
            Err(HandlerError::NoResponse) => return None,
        };
        Some(resp)
    }
}
thread_local! {
    static TOOLBOX_SLOT: Cell<Option<ArcToolbox>> = const { Cell::new(None) };
}

/// The toolbox for the poll that is running.
///
/// Set for the duration of one poll and restored on the way out, including
/// when the inner future panics or the scope is dropped. The value lives on
/// the scope future, so an `.await` inside it sees the same toolbox on the
/// next poll. This is a `thread_local`, not `tokio::task_local`: two polls
/// never overlap on one thread, and the slot is empty between them.
pub struct ToolboxKey;

/// `TOOLBOX.scope(value, future)` installs `value` while `future` is polled.
pub static TOOLBOX: ToolboxKey = ToolboxKey;

/// The slot was empty. `with` panics with this; `try_with` returns it.
#[derive(Debug)]
pub struct ToolboxAccessError;

impl Display for ToolboxAccessError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("TOOLBOX is not set for this poll")
    }
}

impl std::error::Error for ToolboxAccessError {}

impl ToolboxKey {
    /// Poll `future` with `value` installed. The previous slot is restored
    /// when the poll returns.
    pub fn scope<F>(&self, value: ArcToolbox, future: F) -> ToolboxScope<F> {
        ToolboxScope { value, future }
    }

    /// Read the toolbox installed by the current poll.
    pub fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&ArcToolbox) -> R,
    {
        self.try_with(f).expect("TOOLBOX is not set for this poll")
    }

    /// Read the toolbox, or [`ToolboxAccessError`] when no scope is polling.
    pub fn try_with<F, R>(&self, f: F) -> Result<R, ToolboxAccessError>
    where
        F: FnOnce(&ArcToolbox) -> R,
    {
        TOOLBOX_SLOT.with(|slot| {
            let current = slot.take();
            // Put it back before `f`, so a nested `try_with` still sees it,
            // and so a panic in `f` does not clear the slot.
            slot.set(current.clone());
            match current.as_ref() {
                Some(value) => Ok(f(value)),
                None => Err(ToolboxAccessError),
            }
        })
    }
}

/// Future returned by [`ToolboxKey::scope`].
pub struct ToolboxScope<F> {
    value: ArcToolbox,
    future: F,
}

impl<F: Future> Future for ToolboxScope<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `future` stays where it is for the life of this pin. Only `value`
        // is cloned. The guard restores the previous slot on Ready, Pending,
        // and panic.
        let this = unsafe { self.get_unchecked_mut() };
        let previous = TOOLBOX_SLOT.with(|slot| slot.replace(Some(this.value.clone())));
        let _restore = Restore(previous);
        let future = unsafe { Pin::new_unchecked(&mut this.future) };
        future.poll(cx)
    }
}

struct Restore(Option<ArcToolbox>);

impl Drop for Restore {
    fn drop(&mut self) {
        TOOLBOX_SLOT.with(|slot| slot.set(self.0.take()));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use parking_lot::RwLock;

    use super::Toolbox;
    use crate::libs::peer::{Extensions, PeerIdentity};
    use crate::libs::ws::outbound;
    use crate::libs::ws::{WebsocketStates, WsConnection, WsMessage as Message};

    fn conn(id: u32) -> Arc<WsConnection> {
        Arc::new(WsConnection {
            connection_id: id,
            user_id: AtomicU64::new(0),
            roles: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            peer: PeerIdentity::Unknown,
            extensions: Extensions::new(),
            log_id: 0,
        })
    }

    #[test]
    fn buffer_full_policy_cancels_without_a_free_slot() {
        let states = WebsocketStates::new();
        let (tx, mut rx) = outbound::channel(1);
        tx.try_send(Message::from("queued")).unwrap();
        states.insert(7, tx, conn(7));
        let toolbox = Toolbox::new();
        toolbox.set_ws_states(states.clone_states(), false, true);

        assert!(toolbox.send_raw(7, "next".into()));

        assert!(states.get_state(7).unwrap().end.is_cancelled());
        assert!(matches!(rx.try_recv().unwrap(), Message::Text(_)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn header_only_cancels_even_when_the_frame_fits() {
        let states = WebsocketStates::new();
        let (tx, mut rx) = outbound::channel(4);
        states.insert(7, tx, conn(7));
        let toolbox = Toolbox::new();
        toolbox.set_ws_states(states.clone_states(), true, false);

        assert!(toolbox.send_raw(7, "one".into()));

        assert!(states.get_state(7).unwrap().end.is_cancelled());
        assert!(matches!(rx.try_recv().unwrap(), Message::Text(_)));
        assert!(matches!(rx.try_recv().unwrap(), Message::Close(None)));
    }

    #[test]
    fn scope_sees_the_toolbox_on_every_poll_and_clears_it_after() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll};

        use super::TOOLBOX;

        struct See {
            polls: Arc<AtomicUsize>,
        }

        impl Future for See {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                let n = self.polls.fetch_add(1, Ordering::AcqRel);
                assert!(TOOLBOX.try_with(|_| ()).is_ok());
                if n == 0 {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            }
        }

        let toolbox = Toolbox::new();
        let polls = Arc::new(AtomicUsize::new(0));
        let mut scoped = Box::pin(TOOLBOX.scope(
            toolbox,
            See {
                polls: Arc::clone(&polls),
            },
        ));
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(TOOLBOX.try_with(|_| ()).is_err());
        assert!(scoped.as_mut().poll(&mut cx).is_pending());
        assert!(TOOLBOX.try_with(|_| ()).is_err());
        assert!(scoped.as_mut().poll(&mut cx).is_ready());
        assert!(TOOLBOX.try_with(|_| ()).is_err());
        assert_eq!(polls.load(Ordering::Acquire), 2);
    }

    #[test]
    fn dropping_a_pending_scope_leaves_the_slot_empty() {
        use std::future::Future;
        use std::task::Context;

        use super::TOOLBOX;

        let mut scoped = Box::pin(TOOLBOX.scope(Toolbox::new(), std::future::pending::<()>()));
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(Future::poll(scoped.as_mut(), &mut cx).is_pending());
        drop(scoped);
        assert!(TOOLBOX.try_with(|_| ()).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scope_is_the_same_toolbox_after_an_await() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use super::TOOLBOX;

        let toolbox = Toolbox::new();
        let expected = toolbox.clone();
        let saw = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&saw);
        TOOLBOX
            .scope(toolbox, async move {
                assert!(
                    TOOLBOX
                        .try_with(|current| Arc::ptr_eq(current, &expected))
                        .unwrap()
                );
                tokio::task::yield_now().await;
                assert!(
                    TOOLBOX
                        .try_with(|current| Arc::ptr_eq(current, &expected))
                        .unwrap()
                );
                flag.store(true, Ordering::Release);
            })
            .await;
        assert!(saw.load(Ordering::Acquire));
        assert!(TOOLBOX.try_with(|_| ()).is_err());
    }

    #[test]
    fn a_cloned_sender_does_not_move_the_full_depth() {
        let states = WebsocketStates::new();
        let (tx, mut rx) = outbound::channel(1);
        let _extra = tx.clone();
        tx.try_send(Message::from("queued")).unwrap();
        states.insert(7, tx, conn(7));
        let toolbox = Toolbox::new();
        toolbox.set_ws_states(states.clone_states(), false, true);

        assert!(toolbox.send_raw(7, "next".into()));

        // One queued frame fills a capacity of 1. The extra sender did not
        // open another slot, so the policy still fires and `next` is not queued.
        assert!(states.get_state(7).unwrap().end.is_cancelled());
        assert!(matches!(rx.try_recv().unwrap(), Message::Text(_)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_full_buffer_without_the_policy_does_not_cancel() {
        let states = WebsocketStates::new();
        let (tx, _rx) = outbound::channel(1);
        tx.try_send(Message::from("queued")).unwrap();
        states.insert(7, tx, conn(7));
        let toolbox = Toolbox::new();
        toolbox.set_ws_states(states.clone_states(), false, false);

        assert!(toolbox.send_raw(7, "next".into()));

        assert!(!states.get_state(7).unwrap().end.is_cancelled());
    }
}
