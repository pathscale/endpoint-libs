use eyre::{bail, eyre, Context, Result};
use futures::SinkExt;
use futures::StreamExt;
use reqwest::header::HeaderValue;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::net::TcpStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tracing::*;

use crate::libs::log::LogLevel;
use crate::libs::ws::WsLogResponse;
use crate::libs::ws::WsRequestGeneric;
use crate::libs::ws::WsResponseGeneric;

use super::WsResponseValue;

pub trait WsRequest: Serialize + DeserializeOwned + Send + Sync + Clone {
    type Response: WsResponse;
    const METHOD_ID: u32;
    const SCHEMA: &'static str;
    const ROLES: Option<&'static [u32]>;
}
pub trait WsResponse: Serialize + DeserializeOwned + Send + Sync + Clone {
    type Request: WsRequest;
}
pub struct WsClient {
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    seq: u32,
}
impl WsClient {
    pub async fn new(
        connect_addr: &str,
        protocol_header: &str,
        headers: Option<Vec<(&'static str, &'static str)>>,
    ) -> Result<Self> {
        let mut req = <&str as IntoClientRequest>::into_client_request(connect_addr)?;
        req.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(protocol_header)?,
        );

        if let Some(headers) = headers {
            for header in headers {
                req.headers_mut()
                    .insert(header.0, HeaderValue::from_str(header.1)?);
            }
        }

        let (ws_stream, _) = connect_async(req)
            .await
            .context("Failed to connect to endpoint")?;
        Ok(Self {
            stream: ws_stream,
            seq: 0,
        })
    }
    pub async fn send_req(&mut self, method: u32, params: impl Serialize) -> Result<()> {
        self.seq += 1;
        let req = serde_json::to_string(&WsRequestGeneric {
            method,
            seq: self.seq,
            params,
        })?;
        debug!("send req: {}", req);
        self.stream.send(Message::Text(req)).await?;
        Ok(())
    }
    pub async fn recv_raw(&mut self) -> Result<WsResponseValue> {
        let msg = self
            .stream
            .next()
            .await
            .ok_or(eyre!("Connection closed"))??;
        let resp: WsResponseValue = serde_json::from_str(&msg.to_string())?;
        Ok(resp)
    }
    pub async fn recv_resp<T: DeserializeOwned>(&mut self) -> Result<T> {
        loop {
            let msg = self
                .stream
                .next()
                .await
                .ok_or(eyre!("Connection closed"))??;
            match msg {
                Message::Text(text) => {
                    debug!("recv resp: {}", text);
                    let resp: WsResponseGeneric<T> = serde_json::from_str(&text)?;
                    match resp {
                        WsResponseGeneric::Immediate(resp) if resp.seq == self.seq => {
                            return Ok(resp.params);
                        }
                        WsResponseGeneric::Immediate(resp) => {
                            bail!("Seq mismatch this: {} got: {}", self.seq, resp.seq)
                        }
                        WsResponseGeneric::Stream(_) => {
                            debug!("expect immediate response, got stream")
                        }
                        WsResponseGeneric::Forwarded(_) => {
                            debug!("expect immediate response, got forwarded")
                        }
                        WsResponseGeneric::Close => {
                            bail!("unreachable")
                        }
                        WsResponseGeneric::Log(WsLogResponse {
                            log_id,
                            level,
                            message,
                            ..
                        }) => match level {
                            LogLevel::Error => error!(?log_id, "{}", message),
                            LogLevel::Warn => warn!(?log_id, "{}", message),
                            LogLevel::Info => info!(?log_id, "{}", message),
                            LogLevel::Debug => debug!(?log_id, "{}", message),
                            LogLevel::Trace => trace!(?log_id, "{}", message),
                            LogLevel::Detail => trace!(?log_id, "{}", message),
                            LogLevel::Off => {}
                        },
                        WsResponseGeneric::Error(err) => {
                            bail!("Error: {} {:?}", err.code, err.params)
                        }
                    }
                }
                Message::Close(_) => {
                    self.stream.close(None).await?;
                    bail!("Connection closed")
                }
                _ => {}
            }
        }
    }
    pub async fn request<T: WsRequest>(&mut self, params: T) -> Result<T::Response> {
        self.send_req(T::METHOD_ID, params).await?;
        self.recv_resp().await
    }
    pub async fn close(mut self) -> Result<()> {
        self.stream.close(None).await?;
        Ok(())
    }
}
