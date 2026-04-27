use eyre::Result;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::*;

use crate::libs::ws::WsMessage as Message;

use crate::libs::error_code::ErrorCode;
use crate::libs::toolbox::{RequestContext, TOOLBOX};

use super::{
    StreamError, WebsocketServer, WsConnection, WsRequestValue, WsStream, request_error_to_resp,
};

pub struct WsClientSession {
    conn_info: Arc<WsConnection>,
    conn: Box<dyn WsStream>,
    rx: mpsc::Receiver<Message>,
    server: Arc<WebsocketServer>,
}

impl WsClientSession {
    pub fn new(
        conn_info: Arc<WsConnection>,
        conn: Box<dyn WsStream>,
        rx: mpsc::Receiver<Message>,
        server: Arc<WebsocketServer>,
    ) -> Self {
        Self {
            conn_info,
            conn,
            rx,
            server,
        }
    }

    pub fn conn(&self) -> &dyn WsStream {
        self.conn.as_ref()
    }

    pub async fn run(mut self) {
        let addr = self.conn_info.address;
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

    fn handle_message(&mut self, msg: Message) -> Result<bool> {
        let addr = &self.conn_info.address;
        let mut context = RequestContext::from_conn(&self.conn_info);

        #[allow(unreachable_patterns)]
        let obj: Result<WsRequestValue, _> = match msg {
            Message::Text(t) => {
                debug!(ws_server = true, ?addr, "Handling request {}", t);
                serde_json::from_str(&t)
            }
            Message::Binary(b) => {
                debug!(ws_server = true, ?addr, "Handling request <BIN>");
                serde_json::from_slice(&b)
            }
            Message::Ping(_) => {
                return Ok(true);
            }
            Message::Pong(_) => {
                return Ok(true);
            }
            Message::Close(_) => {
                debug!(ws_server = true, ?addr, "Receive side terminated");
                return Ok(false);
            }
            _ => {
                warn!(ws_server = true, ?addr, "Strange pattern {:?}", msg);
                return Ok(true);
            }
        };
        let req = match obj {
            Ok(req) => req,
            Err(err) => {
                self.server.toolbox.send(
                    context.connection_id,
                    request_error_to_resp(&context, ErrorCode::BAD_REQUEST, err.to_string()),
                );
                return Ok(true);
            }
        };
        context.seq = req.seq;
        context.method = req.method;
        context.user_id = self.conn_info.get_user_id();
        context.roles = self.conn_info.get_roles();

        let Some(endpoint) = self.server.handlers.get(&req.method) else {
            self.server.toolbox.send(
                context.connection_id,
                request_error_to_resp(&context, ErrorCode::NOT_IMPLEMENTED, Value::Null),
            );
            return Ok(true);
        };

        if !check_roles(&context.roles, &endpoint.allowed_roles) {
            self.server.toolbox.send(
                context.connection_id,
                request_error_to_resp(&context, ErrorCode::FORBIDDEN, "Forbidden"),
            );
            return Ok(true);
        }

        let handler = endpoint.handler.clone();
        let toolbox = self.server.toolbox.clone();
        tokio::task::spawn_local(async move {
            TOOLBOX
                .scope(
                    toolbox.clone(),
                    handler.handle(&toolbox, context, req.params),
                )
                .await;
        });

        Ok(true)
    }

    async fn run_loop(&mut self) -> Result<()> {
        let conn_id = self.conn_info.connection_id;
        loop {
            while let Ok(msg) = self.rx.try_recv() {
                if !self.send_message(msg).await {
                    return Ok(());
                }
                if self.server.config.header_only {
                    return Ok(());
                }
            }

            tokio::select! {
                msg = self.rx.recv() => {
                    if let Some(msg) = msg {
                        if !self.send_message(msg).await {
                            break;
                        }
                        if self.server.config.header_only {
                            break;
                        }
                    } else {
                        debug!(ws_server = true, ?conn_id, "Outbound channel closed");
                        break;
                    }
                }
                msg = self.conn.recv() => {
                    if let Some(msg_result) = msg {
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
                                warn!(ws_server = true, ?conn_id, "WS write buffer full on receive");
                                break;
                            }
                            Err(StreamError::Other(e)) => {
                                error!(ws_server = true, ?conn_id, err=%e, "WS receive error");
                                break;
                            }
                        };
                        if !self.handle_message(msg)? {
                            break;
                        }
                    } else {
                        debug!(ws_server = true, ?conn_id, "Inbound stream ended");
                        break;
                    }
                }
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
}
