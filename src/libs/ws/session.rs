use eyre::Result;
use futures::StreamExt;
use futures::{Sink, SinkExt, Stream};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::*;

use crate::libs::error_code::ErrorCode;
use crate::libs::toolbox::{RequestContext, TOOLBOX};

use super::{request_error_to_resp, WebsocketServer, WsConnection, WsRequestValue};
pub struct WsClientSession<WS> {
    conn_info: Arc<WsConnection>,
    conn: WS,
    rx: mpsc::Receiver<Message>,
    server: Arc<WebsocketServer>,
}
impl<
        WS: Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
            + Stream<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    > WsClientSession<WS>
{
    pub fn new(
        conn_info: Arc<WsConnection>,
        conn: WS,
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

    pub fn conn(&self) -> &WS {
        &self.conn
    }
    pub async fn run(mut self) {
        let addr = self.conn_info.address;
        let conn_id = self.conn_info.connection_id;
        if let Err(err) = self.run_loop().await {
            error!(?err, ?addr, ?conn_id, "Failed to run websocket session");
        }

        // if let Err(err) = self.handler.handle_drop().await {
        //     error!(
        //         ?err,
        //         ?addr,
        //         ?conn_id,
        //         "Failed to handle websocket session drop"
        //     );
        // }
    }
    // if continue, returns true
    fn handle_message(&mut self, msg: Message) -> Result<bool> {
        let addr = &self.conn_info.address;
        let mut context = RequestContext::from_conn(&self.conn_info);

        let obj: Result<WsRequestValue, _> = match msg {
            Message::Text(t) => {
                debug!(?addr, "Handling request {}", t);

                serde_json::from_str(&t)
            }
            Message::Binary(b) => {
                debug!(?addr, "Handling request <BIN>");
                serde_json::from_slice(&b)
            }
            Message::Ping(_) => {
                return Ok(true);
            }
            Message::Pong(_) => {
                return Ok(true);
            }
            Message::Close(_) => {
                info!(?addr, "Receive side terminated");
                return Ok(false);
            }
            _ => {
                warn!(?addr, "Strange pattern {:?}", msg);
                return Ok(true);
            }
        };
        let req = match obj {
            Ok(req) => req,
            Err(err) => {
                self.server.toolbox.send(
                    context.connection_id,
                    request_error_to_resp(
                        &context,
                        ErrorCode::new(100400), // BadRequest
                        err.to_string(),
                    ),
                );
                return Ok(true);
            }
        };
        context.seq = req.seq;
        context.method = req.method;
        context.user_id = self.conn_info.get_user_id();
        context.role = self.conn_info.get_role();

        // Check roles
        let Some(allowed_roles) = self.server.allowed_roles.get(&req.method) else {
            return Ok(true);
        };

        let allowed = check_roles(context.role, allowed_roles);
        if !allowed {
            self.server.toolbox.send(
                context.connection_id,
                request_error_to_resp(
                    &context,
                    ErrorCode::new(100403), // Forbidden
                    "Forbidden",
                ),
            );
            return Ok(true);
        }

        let handler = self.server.handlers.get(&req.method);
        let handler = match handler {
            Some(handler) => handler,
            None => {
                self.server.toolbox.send(
                    context.connection_id,
                    request_error_to_resp(
                        &context,
                        ErrorCode::new(100501), // Not Implemented
                        Value::Null,
                    ),
                );
                return Ok(true);
            }
        };
        let handler = handler.handler.clone();
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
            tokio::select! {
                msg = self.rx.recv() => {
                    // info!(?conn_id, ?msg, "Received message to send");
                    if let Some(msg) = msg {
                        self.send_message(msg).await?;
                        if self.server.config.header_only {
                            break;
                        }
                    } else {
                        info!(?conn_id, "Receive side terminated");
                        break;
                    }
                }
                msg = self.conn.next() => {
                    if let Some(msg) = msg {
                        let msg = msg?;
                        // info!(?conn_id, ?msg, "Received message");
                        if !self.handle_message(msg)? {
                            break;
                        }
                    } else {
                        info!(?conn_id, "Send side terminated");
                        break;
                    }
                }
            }
        }

        Ok(())
    }
    async fn send_message(&mut self, msg: Message) -> Result<()> {
        // info!(?msg, "Sending message");
        self.conn.send(msg).await?;
        Ok(())
    }
}

fn check_roles(role: u32, allowed_roles: &Option<HashSet<u32>>) -> bool {
    if let Some(allowed_roles) = allowed_roles {
        return allowed_roles.contains(&role);
    }
    true // If roles are None, allow all
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_roles_allowed() {
        use super::check_roles;
        use std::collections::HashSet;

        let allowed_roles: HashSet<u32> = [1, 2, 3].iter().cloned().collect();
        assert!(check_roles(1, &Some(allowed_roles.clone())));
        assert!(check_roles(2, &Some(allowed_roles.clone())));
        assert!(check_roles(3, &Some(allowed_roles.clone())));
        assert!(!check_roles(4, &Some(allowed_roles)));
    }

    #[test]
    fn check_roles_none() {
        use super::check_roles;

        assert!(check_roles(1, &None)); // If roles are None, allow all
    }

    #[test]
    fn check_roles_empty() {
        use super::check_roles;
        use std::collections::HashSet;

        let allowed_roles: HashSet<u32> = HashSet::new();
        assert!(!check_roles(1, &Some(allowed_roles))); // Empty roles means no roles are allowed
    }
}
