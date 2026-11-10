//! Minimal runtime-neutral MCP client (`McpClient`) over `MessageStream`.

use std::collections::VecDeque;
use std::time::Duration;

use eyre::eyre;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::mcp_wire::{
    JsonRpcError, JsonRpcId, JsonRpcMessage, JsonRpcRequest, MCP_PROTOCOL_VERSION, parse,
};
use super::message::WireMessage;
use super::traits::{MessageStream, StreamError};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerCapabilities {
    pub protocol_version: String,
    pub capabilities: Value,
    pub server_info: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolDesc {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolCallResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

#[derive(Debug)]
pub enum McpClientError {
    Stream(StreamError),
    Rpc(JsonRpcError),
    Protocol(String),
    Timeout,
}

pub struct McpClient<S> {
    stream: S,
    next_id: i64,
    notifications: VecDeque<JsonRpcRequest>,
}

impl<S: MessageStream> McpClient<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            next_id: 1,
            notifications: VecDeque::new(),
        }
    }

    pub fn take_notifications(&mut self) -> Vec<JsonRpcRequest> {
        self.notifications.drain(..).collect()
    }

    async fn send_request(&mut self, req: &JsonRpcRequest) -> Result<(), McpClientError> {
        let encoded = serde_json::to_string(req)
            .map_err(|e| McpClientError::Protocol(format!("serialize request: {e}")))?;
        self.stream
            .send(WireMessage::Text(encoded.into()))
            .await
            .map_err(McpClientError::Stream)
    }

    async fn recv_matching(&mut self, wanted_id: &JsonRpcId) -> Result<Value, McpClientError> {
        loop {
            let Some(frame) = self.stream.recv().await else {
                return Err(McpClientError::Stream(StreamError::Other(eyre!(
                    "MCP stream closed before response"
                ))));
            };
            let msg = frame.map_err(McpClientError::Stream)?;
            let WireMessage::Text(text) = msg else {
                continue;
            };
            let parsed = parse(&text).map_err(McpClientError::Rpc)?;
            match parsed {
                JsonRpcMessage::Request(notif) if notif.is_notification() => {
                    self.notifications.push_back(notif);
                }
                JsonRpcMessage::Request(_) => {}
                JsonRpcMessage::Response(resp) => {
                    if resp.id.as_ref() != Some(wanted_id) {
                        return Err(McpClientError::Protocol(format!(
                            "mismatched response id: expected {wanted_id:?}, got {:?}",
                            resp.id
                        )));
                    }
                    if let Some(err) = resp.error {
                        return Err(McpClientError::Rpc(err));
                    }
                    return resp.result.ok_or_else(|| {
                        McpClientError::Protocol("response missing both result and error".into())
                    });
                }
            }
        }
    }

    pub async fn initialize(&mut self) -> Result<McpServerCapabilities, McpClientError> {
        let id = JsonRpcId::Number(self.next_id);
        self.next_id += 1;
        let req = JsonRpcRequest::call(
            id.clone(),
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "endpoint-libs-mcp-client", "version": "1.0.0" }
            }),
        );
        self.send_request(&req).await?;
        let res = self.recv_matching(&id).await?;
        let protocol_version = res
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(MCP_PROTOCOL_VERSION)
            .to_string();
        let capabilities = res.get("capabilities").cloned().unwrap_or_else(|| json!({}));
        let server_info = res.get("serverInfo").cloned();

        let initialized = JsonRpcRequest::notification("notifications/initialized", json!({}));
        self.send_request(&initialized).await?;

        Ok(McpServerCapabilities {
            protocol_version,
            capabilities,
            server_info,
        })
    }

    pub async fn tools_list(&mut self) -> Result<Vec<McpToolDesc>, McpClientError> {
        let id = JsonRpcId::Number(self.next_id);
        self.next_id += 1;
        let req = JsonRpcRequest::call(id.clone(), "tools/list", json!({}));
        self.send_request(&req).await?;
        let res = self.recv_matching(&id).await?;
        let tools_val = res
            .get("tools")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        serde_json::from_value(tools_val)
            .map_err(|e| McpClientError::Protocol(format!("invalid tools list: {e}")))
    }

    pub async fn tools_call(
        &mut self,
        name: &str,
        arguments: Value,
        deadline: Duration,
    ) -> Result<McpToolCallResult, McpClientError> {
        let id = JsonRpcId::Number(self.next_id);
        self.next_id += 1;
        if deadline.is_zero() {
            let cancel = JsonRpcRequest::notification(
                "notifications/cancelled",
                json!({ "requestId": id, "reason": "deadline expired" }),
            );
            let _ = self.send_request(&cancel).await;
            return Err(McpClientError::Timeout);
        }
        let req = JsonRpcRequest::call(
            id.clone(),
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        self.send_request(&req).await?;
        let res = self.recv_matching(&id).await?;
        serde_json::from_value(res)
            .map_err(|e| McpClientError::Protocol(format!("invalid tool call result: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    struct ScriptedStream {
        sent: Vec<String>,
        inbound: VecDeque<String>,
    }

    #[async_trait::async_trait(?Send)]
    impl MessageStream for ScriptedStream {
        async fn send(&mut self, msg: WireMessage) -> Result<(), StreamError> {
            if let WireMessage::Text(t) = msg {
                self.sent.push(t.to_string());
            }
            Ok(())
        }

        async fn recv(&mut self) -> Option<Result<WireMessage, StreamError>> {
            self.inbound.pop_front().map(|s| Ok(WireMessage::Text(s.into())))
        }
    }

    #[test]
    fn mcp_client_initialize_list_call_and_cancel() {
        block_on(async {
            let mut inbound = VecDeque::new();
            inbound.push_back(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mock", "version": "0.1" }
                    }
                })
                .to_string(),
            );
            inbound.push_back(
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": { "step": 1 }
                })
                .to_string(),
            );
            inbound.push_back(
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "result": {
                        "tools": [{ "name": "echo", "description": "Echo tool" }]
                    }
                })
                .to_string(),
            );
            inbound.push_back(
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "result": {
                        "content": [{ "type": "text", "text": "hello" }],
                        "isError": false
                    }
                })
                .to_string(),
            );

            let stream = ScriptedStream {
                sent: Vec::new(),
                inbound,
            };
            let mut client = McpClient::new(stream);
            let caps = client.initialize().await.unwrap();
            assert_eq!(caps.protocol_version, MCP_PROTOCOL_VERSION);

            let tools = client.tools_list().await.unwrap();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "echo");
            assert_eq!(client.take_notifications().len(), 1);

            let call_res = client
                .tools_call("echo", json!({"msg": "hello"}), Duration::from_secs(5))
                .await
                .unwrap();
            assert!(!call_res.is_error);

            let err = client
                .tools_call("slow", json!({}), Duration::ZERO)
                .await
                .unwrap_err();
            assert!(matches!(err, McpClientError::Timeout));
            assert!(
                client
                    .stream
                    .sent
                    .last()
                    .unwrap()
                    .contains("notifications/cancelled")
            );
        });
    }
}
