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
type SendOutcomeFn = dyn Fn(ConnectionId, WsResponseValue) -> EnqueueOutcome + Send + Sync;
type SendRawFn = dyn Fn(ConnectionId, String) -> bool + Send + Sync;
type SendRawFnArc = Arc<SendRawFn>;
type SendRawBatchFn = dyn Fn(ConnectionId, Vec<String>) -> SendRawBatchOutcome + Send + Sync;
type SendRawBatchFnArc = Arc<SendRawBatchFn>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnqueueOutcome {
    Queued,
    Full,
    Disconnected,
}

/// Result of atomically admitting pre-serialized frames to a connection queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendRawBatchOutcome {
    /// Every frame was queued in order.
    Queued,
    /// The frames fit an empty queue but not the available capacity.
    Full,
    /// The frame count exceeds the queue's total capacity.
    TooLarge,
    /// The connection is unregistered or its receiver has been dropped.
    Closed,
    /// Batch admission is unavailable in header-only mode or with a custom raw sender.
    Unsupported,
}

pub struct Toolbox {
    /// Typed sender. The built-in callback returns whether it queued the frame.
    pub send_msg: OnceLock<SendFnArc>,
    /// Raw sender. The built-in callback returns whether it queued the frame.
    pub send_raw_msg: OnceLock<SendRawFnArc>,
    built_in_send: OnceLock<(SendFnArc, Arc<SendOutcomeFn>)>,
    send_raw_batch_msg: OnceLock<Option<SendRawBatchFnArc>>,
}
pub type ArcToolbox = Arc<Toolbox>;
impl Toolbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            send_msg: OnceLock::new(),
            send_raw_msg: OnceLock::new(),
            built_in_send: OnceLock::new(),
            send_raw_batch_msg: OnceLock::new(),
        })
    }

    pub fn set_ws_states(
        &self,
        states: Arc<DashMap<ConnectionId, Arc<WsStreamState>>>,
        oneshot: bool,
        drop_on_buffer_full: bool,
    ) {
        let mut initialized = false;
        // One initializer installs all senders from the same connection table and mode.
        let _ = self.send_raw_batch_msg.get_or_init(|| {
            initialized = true;
            self.install_ws_senders(states, oneshot, drop_on_buffer_full)
        });
        if !initialized {
            warn!(
                ws_server = true,
                "set_ws_states called twice — ignoring second call"
            );
        }
    }

    fn install_ws_senders(
        &self,
        states: Arc<DashMap<ConnectionId, Arc<WsStreamState>>>,
        oneshot: bool,
        drop_on_buffer_full: bool,
    ) -> Option<SendRawBatchFnArc> {
        let raw_states = Arc::clone(&states);
        let batch_states = Arc::clone(&states);
        let send_outcome: Arc<SendOutcomeFn> = Arc::new(move |conn_id, msg| {
            let state = if let Some(state) = states.get(&conn_id) {
                state
            } else {
                return EnqueueOutcome::Disconnected;
            };
            let serialized = match serde_json::to_string(&msg) {
                Ok(serialized) => serialized,
                Err(e) => {
                    error!(ws_server = true, conn_id, err=%e, "Failed to serialize WS response — dropping message");
                    return EnqueueOutcome::Disconnected;
                }
            };
            Self::enqueue(
                &state.message_queue,
                Some(&state.end),
                serialized,
                oneshot,
                conn_id,
                drop_on_buffer_full,
            )
        });
        let mut installed = false;
        self.send_msg.get_or_init(|| {
            let outcome = Arc::clone(&send_outcome);
            let send: SendFnArc =
                Arc::new(move |conn_id, msg| outcome(conn_id, msg) == EnqueueOutcome::Queued);
            // Associate the queue outcome with this callback before publishing it.
            let _ = self.built_in_send.set((Arc::clone(&send), send_outcome));
            installed = true;
            send
        });
        if !installed {
            warn!(
                ws_server = true,
                "Keeping the existing typed WebSocket sender"
            );
        }

        // Pre-serialized frames (JSON-RPC/MCP responses) bypass WsResponseValue.
        let send_raw_fn: SendRawFnArc = Arc::new(move |conn_id, serialized| {
            let Some(state) = raw_states.get(&conn_id) else {
                return false;
            };
            Self::enqueue(
                &state.message_queue,
                Some(&state.end),
                serialized,
                oneshot,
                conn_id,
                drop_on_buffer_full,
            ) == EnqueueOutcome::Queued
        });
        // A manually installed raw sender may use a different route.
        if self.send_raw_msg.set(send_raw_fn).is_err() {
            return None;
        }

        Some(Arc::new(move |conn_id, batch| {
            if oneshot {
                return SendRawBatchOutcome::Unsupported;
            }
            let state = batch_states
                .get(&conn_id)
                .map(|state| Arc::clone(state.value()));
            let Some(state) = state else {
                return SendRawBatchOutcome::Closed;
            };
            let batch = batch.into_iter().map(Message::from).collect();
            match state.message_queue.try_send_batch(batch) {
                Ok(()) => SendRawBatchOutcome::Queued,
                Err(outbound::TrySendBatchError::Full(_)) => SendRawBatchOutcome::Full,
                Err(outbound::TrySendBatchError::TooLarge(_)) => SendRawBatchOutcome::TooLarge,
                Err(outbound::TrySendBatchError::Closed(_)) => SendRawBatchOutcome::Closed,
            }
        }))
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
        let _ = Self::enqueue(sender, None, serialized, oneshot, conn_id, drop_on_full);
    }

    pub fn send_serialized_ws_msg(
        sender: &Sender<Message>,
        serialized: String,
        oneshot: bool,
        conn_id: ConnectionId,
        drop_on_full: bool,
    ) {
        let _ = Self::enqueue(sender, None, serialized, oneshot, conn_id, drop_on_full);
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
    ) -> EnqueueOutcome {
        let outcome = match sender.try_send(serialized.into()) {
            Ok(()) => EnqueueOutcome::Queued,
            Err(outbound::TrySendError::Full(_)) => {
                error!(
                    ws_server = true,
                    conn_id, "Send buffer full — client too slow or disconnected"
                );
                if drop_on_full {
                    Self::close_for_policy(sender, end);
                    EnqueueOutcome::Disconnected
                } else {
                    EnqueueOutcome::Full
                }
            }
            Err(outbound::TrySendError::Closed(_)) => {
                debug!(
                    ws_server = true,
                    conn_id, "Send channel closed — client already disconnected"
                );
                EnqueueOutcome::Disconnected
            }
        };
        if oneshot {
            Self::close_for_policy(sender, end);
        }
        outcome
    }

    fn close_for_policy(sender: &Sender<Message>, end: Option<&Shutdown>) {
        let _ = sender.try_send(Message::Close(None));
        if let Some(end) = end {
            end.cancel();
        }
    }
    /// Send a typed response. The built-in sender returns `true` only when
    /// the frame enters the outbound queue. Custom sender results pass through.
    pub fn send(&self, conn_id: ConnectionId, resp: WsResponseValue) -> bool {
        match self.send_msg.get() {
            Some(f) => f(conn_id, resp),
            None => false,
        }
    }

    /// Send a subscription frame and report whether to keep the subscriber.
    pub(super) fn send_to_subscriber(&self, conn_id: ConnectionId, resp: WsResponseValue) -> bool {
        let Some(send) = self.send_msg.get() else {
            return false;
        };
        match self.built_in_send.get() {
            Some((built_in, outcome)) if Arc::ptr_eq(built_in, send) => {
                outcome(conn_id, resp) != EnqueueOutcome::Disconnected
            }
            _ => send(conn_id, resp),
        }
    }

    pub(super) fn enqueue_response(
        state: &WsStreamState,
        resp: WsResponseValue,
        oneshot: bool,
        drop_on_full: bool,
    ) -> bool {
        let Ok(serialized) = serde_json::to_string(&resp) else {
            return false;
        };
        Self::enqueue(
            &state.message_queue,
            Some(&state.end),
            serialized,
            oneshot,
            state.conn.connection_id,
            drop_on_full,
        ) == EnqueueOutcome::Queued
    }
    /// Send a pre-serialized frame as-is. The built-in sender returns `true`
    /// only when the frame enters the outbound queue. Custom sender results pass through.
    pub fn send_raw(&self, conn_id: ConnectionId, serialized: String) -> bool {
        match self.send_raw_msg.get() {
            Some(f) => f(conn_id, serialized),
            None => false,
        }
    }

    /// Queue pre-serialized text frames together in FIFO order.
    ///
    /// A failed admission leaves the queue and connection untouched. See
    /// [`SendRawBatchOutcome`] for the possible results.
    pub fn try_send_raw_batch(
        &self,
        conn_id: ConnectionId,
        batch: Vec<String>,
    ) -> SendRawBatchOutcome {
        match self.send_raw_batch_msg.get() {
            Some(Some(f)) => f(conn_id, batch),
            Some(None) => SendRawBatchOutcome::Unsupported,
            None if self.send_raw_msg.get().is_some() => SendRawBatchOutcome::Unsupported,
            None => SendRawBatchOutcome::Closed,
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

    use super::{RequestContext, SendRawBatchOutcome, Toolbox};
    use crate::libs::peer::{Extensions, PeerIdentity};
    use crate::libs::ws::outbound;
    use crate::libs::ws::{
        SubscriptionManager, WebsocketStates, WsConnection, WsMessage as Message,
    };

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

        assert!(!toolbox.send_raw(7, "next".into()));

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
    fn raw_batch_shares_the_fifo_and_rejects_without_policy_close() {
        let states = WebsocketStates::new();
        let (tx, mut rx) = outbound::channel(3);
        states.insert(7, tx, conn(7));
        let toolbox = Toolbox::new();
        toolbox.set_ws_states(states.clone_states(), false, true);
        let state = states.get_state(7).unwrap();

        assert!(toolbox.send_raw(7, "queued".into()));
        assert_eq!(
            toolbox.try_send_raw_batch(7, vec!["first".into(), "second".into(), "third".into()]),
            SendRawBatchOutcome::Full
        );
        assert_eq!(
            toolbox.try_send_raw_batch(
                7,
                vec![
                    "first".into(),
                    "second".into(),
                    "third".into(),
                    "fourth".into()
                ],
            ),
            SendRawBatchOutcome::TooLarge
        );
        assert!(!state.end.is_cancelled());

        assert_eq!(
            toolbox.try_send_raw_batch(7, vec!["first".into(), "second".into()]),
            SendRawBatchOutcome::Queued
        );
        for expected in ["queued", "first", "second"] {
            assert_eq!(rx.try_recv(), Ok(Message::from(expected)));
        }
        assert_eq!(rx.try_recv(), Err(outbound::TryRecvError::Empty));
    }

    #[test]
    fn raw_batch_uses_the_same_setup_as_raw_during_concurrent_registration() {
        let first = WebsocketStates::new();
        let second = WebsocketStates::new();
        let (first_tx, mut first_rx) = outbound::channel(2);
        let (second_tx, mut second_rx) = outbound::channel(2);
        first.insert(7, first_tx, conn(7));
        second.insert(7, second_tx, conn(7));

        let toolbox = Toolbox::new();
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                toolbox.set_ws_states(first.clone_states(), false, false);
            });
            start.wait();
            toolbox.set_ws_states(second.clone_states(), false, false);
        });

        assert!(toolbox.send_raw(7, "raw".into()));
        assert_eq!(
            toolbox.try_send_raw_batch(7, vec!["batch".into()]),
            SendRawBatchOutcome::Queued
        );
        let (selected, other) = match first_rx.try_recv() {
            Ok(frame) => {
                assert_eq!(frame, Message::from("raw"));
                (&mut first_rx, &mut second_rx)
            }
            Err(outbound::TryRecvError::Empty) => {
                assert_eq!(second_rx.try_recv(), Ok(Message::from("raw")));
                (&mut second_rx, &mut first_rx)
            }
            Err(error) => panic!("unexpected queue error: {error:?}"),
        };
        assert_eq!(selected.try_recv(), Ok(Message::from("batch")));
        assert_eq!(other.try_recv(), Err(outbound::TryRecvError::Empty));
    }

    #[test]
    fn raw_batch_is_unsupported_with_a_custom_raw_sender() {
        let toolbox = Toolbox::new();
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(
            toolbox
                .send_raw_msg
                .set(Arc::new(move |id, frame| tx.send((id, frame)).is_ok()))
                .is_ok()
        );

        assert!(toolbox.send_raw(7, "before".into()));
        assert_eq!(
            toolbox.try_send_raw_batch(7, vec!["batch".into()]),
            SendRawBatchOutcome::Unsupported
        );

        let states = WebsocketStates::new();
        let (queue_tx, mut queue_rx) = outbound::channel(1);
        states.insert(7, queue_tx, conn(7));
        toolbox.set_ws_states(states.clone_states(), false, false);

        assert!(toolbox.send_raw(7, "after".into()));
        assert_eq!(
            toolbox.try_send_raw_batch(7, vec!["batch".into()]),
            SendRawBatchOutcome::Unsupported
        );
        assert_eq!(rx.try_recv().unwrap(), (7, String::from("before")));
        assert_eq!(rx.try_recv().unwrap(), (7, String::from("after")));
        assert_eq!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        assert_eq!(queue_rx.try_recv(), Err(outbound::TryRecvError::Empty));
    }

    #[test]
    fn raw_batch_is_unsupported_in_header_only_mode() {
        let states = WebsocketStates::new();
        let (tx, mut rx) = outbound::channel(2);
        states.insert(7, tx, conn(7));
        let toolbox = Toolbox::new();
        toolbox.set_ws_states(states.clone_states(), true, false);

        assert_eq!(
            toolbox.try_send_raw_batch(7, vec!["first".into(), "second".into()]),
            SendRawBatchOutcome::Unsupported
        );
        assert_eq!(rx.try_recv(), Err(outbound::TryRecvError::Empty));
        assert!(!states.get_state(7).unwrap().end.is_cancelled());
    }

    #[test]
    fn raw_batch_reports_missing_connections_and_dropped_receivers_as_closed() {
        let states = WebsocketStates::new();
        let (tx, rx) = outbound::channel(1);
        states.insert(7, tx, conn(7));
        let toolbox = Toolbox::new();
        toolbox.set_ws_states(states.clone_states(), false, true);

        assert_eq!(
            toolbox.try_send_raw_batch(8, vec!["missing".into()]),
            SendRawBatchOutcome::Closed
        );

        drop(rx);
        assert_eq!(
            toolbox.try_send_raw_batch(7, vec!["closed".into(), "oversized".into()]),
            SendRawBatchOutcome::Closed
        );
        assert!(!states.get_state(7).unwrap().end.is_cancelled());
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

    /// The slot survives a suspension, which is the whole point of restoring it
    /// on every poll rather than once at entry.
    ///
    /// The two tests above poll by hand because one poll is all they need.
    /// This one needs the future to come back from `Pending` and find the same
    /// toolbox, so it needs something that polls twice: `nagoya::block_on`,
    /// which drives the future on the calling thread. That is load bearing
    /// here and not a detail -- `TOOLBOX` is a `thread_local`, so a runtime
    /// free to resume the future on another thread would be testing a
    /// different question.
    #[test]
    fn scope_is_the_same_toolbox_after_an_await() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use super::TOOLBOX;

        let toolbox = Toolbox::new();
        let expected = toolbox.clone();
        let saw = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&saw);
        nagoya::block_on(TOOLBOX.scope(toolbox, async move {
            assert!(
                TOOLBOX
                    .try_with(|current| Arc::ptr_eq(current, &expected))
                    .unwrap()
            );
            nagoya::yield_now().await;
            assert!(
                TOOLBOX
                    .try_with(|current| Arc::ptr_eq(current, &expected))
                    .unwrap()
            );
            flag.store(true, Ordering::Release);
        }));
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

        assert!(!toolbox.send_raw(7, "next".into()));

        // One queued frame fills a capacity of 1. The extra sender did not
        // open another slot, so the policy still fires and `next` is not queued.
        assert!(states.get_state(7).unwrap().end.is_cancelled());
        assert!(matches!(rx.try_recv().unwrap(), Message::Text(_)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_full_buffer_without_the_policy_does_not_cancel() {
        let states = WebsocketStates::new();
        let (tx, mut rx) = outbound::channel(1);
        tx.try_send(Message::from("queued")).unwrap();
        states.insert(7, tx, conn(7));
        let toolbox = Toolbox::new();
        toolbox.set_ws_states(states.clone_states(), false, false);

        assert!(!toolbox.send_raw(7, "next".into()));
        let reply = || {
            crate::libs::ws::WsResponseValue::Immediate(crate::libs::ws::WsSuccessResponse {
                method: 1,
                seq: 1,
                params: serde_json::value::to_raw_value(&serde_json::json!({})).unwrap(),
            })
        };
        assert!(!toolbox.send(7, reply()));
        assert!(!Toolbox::enqueue_response(
            &states.get_state(7).unwrap(),
            reply(),
            false,
            false
        ));

        assert!(!states.get_state(7).unwrap().end.is_cancelled());
        assert!(matches!(rx.try_recv().unwrap(), Message::Text(_)));
        assert!(rx.try_recv().is_err());
        assert!(Toolbox::enqueue_response(
            &states.get_state(7).unwrap(),
            reply(),
            false,
            false
        ));
        assert!(matches!(rx.try_recv().unwrap(), Message::Text(_)));
    }

    #[test]
    fn subscription_publishing_retains_live_connections_when_the_queue_is_full() {
        let states = WebsocketStates::new();
        let (tx, mut rx) = outbound::channel(1);
        tx.try_send(Message::from("occupied")).unwrap();
        let connection = conn(7);
        let ctx = RequestContext::from_conn(&connection);
        states.insert(7, tx, connection);
        let toolbox = Toolbox::new();
        toolbox.set_ws_states(states.clone_states(), false, false);

        let mut subscriptions = SubscriptionManager::new(1);
        subscriptions.subscribe_with_keys(ctx, vec!["topic"], (), |_| {});
        subscriptions.publish_to(&toolbox, 7, &"first");
        subscriptions.publish_to_all(&toolbox, &"second");
        assert!(!states.get_state(7).unwrap().end.is_cancelled());
        assert_eq!(rx.try_recv(), Ok(Message::from("occupied")));
        assert_eq!(rx.try_recv(), Err(outbound::TryRecvError::Empty));

        subscriptions.publish_to_key(&toolbox, &"topic", &"third");
        assert!(matches!(rx.try_recv().unwrap(), Message::Text(_)));
        drop(rx);
        subscriptions.publish_to_all(&toolbox, &"fourth");
        assert!(!subscriptions.subscribes.contains_key(&7));
    }

    #[test]
    fn subscription_publishing_respects_a_custom_typed_sender() {
        for replace_built_in in [false, true] {
            let states = WebsocketStates::new();
            let (tx, mut rx) = outbound::channel(1);
            let connection = conn(7);
            let ctx = RequestContext::from_conn(&connection);
            states.insert(7, tx, connection);
            let mut toolbox = Toolbox::new();
            if replace_built_in {
                toolbox.set_ws_states(states.clone_states(), false, false);
                let toolbox = Arc::get_mut(&mut toolbox).unwrap();
                assert!(toolbox.send_msg.take().is_some());
            }
            let (sent, received) = std::sync::mpsc::channel();
            assert!(
                toolbox
                    .send_msg
                    .set(Arc::new(move |id, _| sent.send(id).is_ok()))
                    .is_ok()
            );
            if !replace_built_in {
                toolbox.set_ws_states(states.clone_states(), false, false);
            }

            let mut subscriptions: SubscriptionManager<()> = SubscriptionManager::new(1);
            subscriptions.subscribe(ctx, (), |_| {});
            subscriptions.publish_to(&toolbox, 7, &"first");
            assert_eq!(received.try_recv(), Ok(7));
            assert!(subscriptions.subscribes.contains_key(&7));
            assert_eq!(rx.try_recv(), Err(outbound::TryRecvError::Empty));

            drop(received);
            subscriptions.publish_to(&toolbox, 7, &"second");
            assert!(!subscriptions.subscribes.contains_key(&7));
        }
    }
}
