use dashmap::DashMap;
use std::sync::Arc;

use super::WsMessage as Message;

use super::{ConnectionId, WsConnection};
use crate::libs::signal::Shutdown;

#[derive(Default)]
pub struct WebsocketStates {
    states: Arc<DashMap<ConnectionId, Arc<WsStreamState>>>,
}

impl WebsocketStates {
    pub fn new() -> Self {
        WebsocketStates::default()
    }
    pub fn remove(&self, connection_id: u32) {
        self.states.remove(&connection_id);
    }

    pub fn get_state(&self, connection_id: u32) -> Option<Arc<WsStreamState>> {
        self.states.get(&connection_id).map(|x| x.value().clone())
    }
    pub fn clone_states(&self) -> Arc<DashMap<u32, Arc<WsStreamState>>> {
        Arc::clone(&self.states)
    }
    pub fn insert(
        &self,
        connection_id: u32,
        message_queue: tokio::sync::mpsc::Sender<Message>,
        conn: Arc<WsConnection>,
    ) {
        self.states.insert(
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
    pub message_queue: tokio::sync::mpsc::Sender<Message>,
    /// Policy close for this connection.
    ///
    /// `Shutdown::cancel` sticks, so the session still observes it after the
    /// fact. A `Close` frame queued behind the payload does not: `try_send`
    /// fails when the buffer is already full, which is the case
    /// `drop_conn_on_buffer_full` exists for.
    pub end: Arc<Shutdown>,
}
