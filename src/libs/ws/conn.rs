use alloc::collections::BTreeMap as HashMap;
use alloc::sync::Arc;
use eko::thread::RwLock;

use super::WsMessage as Message;
use super::outbound::Sender;

use super::{ConnectionId, WsConnection};
use crate::libs::signal::Shutdown;

pub type SharedWsStates = Arc<RwLock<HashMap<ConnectionId, Arc<WsStreamState>>>>;

#[derive(Default)]
pub struct WebsocketStates {
    states: SharedWsStates,
}

impl WebsocketStates {
    pub fn new() -> Self {
        WebsocketStates::default()
    }
    pub fn remove(&self, connection_id: u32) {
        self.states.write().remove(&connection_id);
    }

    pub fn get_state(&self, connection_id: u32) -> Option<Arc<WsStreamState>> {
        self.states.read().get(&connection_id).cloned()
    }
    pub fn clone_states(&self) -> SharedWsStates {
        Arc::clone(&self.states)
    }
    pub fn insert(
        &self,
        connection_id: u32,
        message_queue: Sender<Message>,
        conn: Arc<WsConnection>,
    ) {
        self.states.write().insert(
            connection_id,
            Arc::new(WsStreamState {
                conn,
                message_queue,
                end: Arc::new(Shutdown::new()),
            }),
        );
    }
}

pub struct WsStreamState {
    pub conn: Arc<WsConnection>,
    /// Per-connection outbound queue. The bound is the number of queued
    /// messages; see [`super::outbound`].
    pub message_queue: Sender<Message>,
    /// Policy close for this connection.
    ///
    /// `Shutdown::cancel` sticks, so the session still observes it after the
    /// fact. A `Close` frame queued behind the payload does not: `try_send`
    /// fails when the buffer is already full, which is the case
    /// `drop_conn_on_buffer_full` exists for.
    pub end: Arc<Shutdown>,
}
