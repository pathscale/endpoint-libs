use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use eyre::{Context, Result, bail, ensure, eyre};
use futures::SinkExt;
use futures::StreamExt;
use http_body_util::Empty;
use hyper::StatusCode;
use hyper::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use reqwest::header::HeaderValue;
use rustls::pki_types::ServerName;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Role;
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
    const ROLES: &'static [u32];
}
pub trait WsResponse: Serialize + DeserializeOwned + Send + Sync + Clone {
    type Request: WsRequest;
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which HTTP version to use when connecting.
#[derive(Debug, Clone, Copy, Default)]
pub enum WsVersionMode {
    /// HTTP/1.1 upgrade handshake (existing behaviour).
    #[default]
    Http1Only,
    /// HTTP/2 Extended CONNECT (RFC 8441): h2 for `wss://`, h2c for `ws://`.
    Http2Only,
    /// Try HTTP/2 first; fall back to HTTP/1.1 on any error.
    Auto,
}

/// Response metadata returned by [`WsClientBuilder::build`].
pub struct WsConnectResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Internal stream abstraction
// ---------------------------------------------------------------------------

enum WsStream {
    H1(WebSocketStream<MaybeTlsStream<TcpStream>>),
    H2(WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>),
}

// ---------------------------------------------------------------------------
// WsClient
// ---------------------------------------------------------------------------

pub struct WsClient {
    stream: WsStream,
    seq: u32,
}

impl WsClient {
    // Existing HTTP/1.1 constructor — unchanged externally.
    pub async fn new(
        connect_addr: &str,
        protocol_header: &str,
        headers: Option<Vec<(&'static str, &'static str)>>,
    ) -> Result<(Self, tokio_tungstenite::tungstenite::http::Response<std::option::Option<Vec<u8>>>)> {
        let mut req = <&str as IntoClientRequest>::into_client_request(connect_addr)?;
        if !protocol_header.is_empty() {
            req.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_str(protocol_header)?,
            );
        }

        if let Some(headers) = headers {
            for header in headers {
                req.headers_mut()
                    .insert(header.0, HeaderValue::from_str(header.1)?);
            }
        }

        let (ws_stream, response) = connect_async(req)
            .await
            .context("Failed to connect to endpoint")?;
        Ok((Self {
            stream: WsStream::H1(ws_stream),
            seq: 0,
        }, response))
    }

    // --- Private stream helpers -------------------------------------------

    async fn stream_send(&mut self, msg: Message) -> Result<()> {
        match &mut self.stream {
            WsStream::H1(s) => s.send(msg).await?,
            WsStream::H2(s) => s.send(msg).await?,
        }
        Ok(())
    }

    async fn stream_next(&mut self) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
        match &mut self.stream {
            WsStream::H1(s) => s.next().await,
            WsStream::H2(s) => s.next().await,
        }
    }

    async fn stream_close(&mut self) -> Result<()> {
        match &mut self.stream {
            WsStream::H1(s) => s.close(None).await?,
            WsStream::H2(s) => s.close(None).await?,
        }
        Ok(())
    }

    // --- Public API (signatures unchanged) --------------------------------

    pub async fn send_req(&mut self, method: u32, params: impl Serialize) -> Result<()> {
        self.seq += 1;
        let req = serde_json::to_string(&WsRequestGeneric {
            method,
            seq: self.seq,
            params,
        })?;
        debug!("send req: {}", req);
        self.stream_send(Message::Text(req.into())).await
    }

    /// Send a fully pre-serialized request message.
    pub async fn send_raw(&mut self, request_bytes: &[u8]) -> Result<()> {
        let text = std::str::from_utf8(request_bytes).context("Invalid UTF-8 in request bytes")?;
        self.stream_send(Message::Text(text.into())).await
    }

    pub async fn recv_raw(&mut self) -> Result<WsResponseValue> {
        let msg = self
            .stream_next()
            .await
            .ok_or(eyre!("Connection closed"))??;
        let resp: WsResponseValue = serde_json::from_str(&msg.to_string())?;
        Ok(resp)
    }

    pub async fn recv_resp<T: DeserializeOwned>(&mut self) -> Result<T> {
        loop {
            let msg = self
                .stream_next()
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
                    self.stream_close().await?;
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
        self.stream_close().await
    }
}

// ---------------------------------------------------------------------------
// WsClientBuilder
// ---------------------------------------------------------------------------

pub struct WsClientBuilder {
    mode: WsVersionMode,
    protocol_header: String,
    headers: Vec<(&'static str, &'static str)>,
}

impl WsClientBuilder {
    pub fn new() -> Self {
        Self {
            mode: WsVersionMode::Http1Only,
            protocol_header: String::new(),
            headers: Vec::new(),
        }
    }

    pub fn mode(mut self, mode: WsVersionMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn protocol_header(mut self, protocol: impl Into<String>) -> Self {
        self.protocol_header = protocol.into();
        self
    }

    pub fn header(mut self, key: &'static str, value: &'static str) -> Self {
        self.headers.push((key, value));
        self
    }

    pub fn headers(mut self, headers: Vec<(&'static str, &'static str)>) -> Self {
        self.headers.extend(headers);
        self
    }

    pub async fn build(self, connect_addr: &str) -> Result<(WsClient, WsConnectResponse)> {
        match self.mode {
            WsVersionMode::Http1Only => {
                connect_h1(connect_addr, &self.protocol_header, &self.headers).await
            }
            WsVersionMode::Http2Only => {
                connect_h2(connect_addr, &self.protocol_header, &self.headers).await
            }
            WsVersionMode::Auto => {
                match connect_h2(connect_addr, &self.protocol_header, &self.headers).await {
                    Ok(result) => Ok(result),
                    Err(h2_err) => {
                        debug!("H2 connection failed ({}), falling back to HTTP/1.1", h2_err);
                        connect_h1(connect_addr, &self.protocol_header, &self.headers).await
                    }
                }
            }
        }
    }
}

impl Default for WsClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

struct ParsedUrl {
    tls: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_ws_url(url: &str) -> Result<ParsedUrl> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        bail!("URL must start with ws:// or wss://: {}", url)
    };

    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_owned()),
        None => (rest, "/".to_owned()),
    };

    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let h = authority[..i].to_owned();
            let p: u16 = authority[i + 1..].parse().context("Invalid port in URL")?;
            (h, p)
        }
        None => (authority.to_owned(), if tls { 443 } else { 80 }),
    };

    Ok(ParsedUrl { tls, host, port, path })
}

async fn connect_h1(
    addr: &str,
    protocol_header: &str,
    headers: &[(&'static str, &'static str)],
) -> Result<(WsClient, WsConnectResponse)> {
    let mut req = <&str as IntoClientRequest>::into_client_request(addr)?;
    if !protocol_header.is_empty() {
        req.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(protocol_header)?,
        );
    }
    for (k, v) in headers {
        req.headers_mut().insert(*k, HeaderValue::from_str(v)?);
    }

    let (ws_stream, response) = connect_async(req)
        .await
        .context("Failed to connect to endpoint")?;

    let conn_resp = WsConnectResponse {
        status: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
    };

    Ok((WsClient { stream: WsStream::H1(ws_stream), seq: 0 }, conn_resp))
}

async fn connect_h2(
    addr: &str,
    protocol_header: &str,
    headers: &[(&'static str, &'static str)],
) -> Result<(WsClient, WsConnectResponse)> {
    let ParsedUrl { tls, host, port, path } = parse_ws_url(addr)?;

    let sock_addr: SocketAddr = tokio::net::lookup_host(format!("{}:{}", host, port))
        .await
        .context("DNS resolution failed")?
        .next()
        .ok_or_else(|| eyre!("No addresses returned for {}:{}", host, port))?;

    let tcp = TcpStream::connect(sock_addr)
        .await
        .context("TCP connect failed")?;
    tcp.set_nodelay(true)?;

    if tls {
        let tls_stream = make_tls_stream(tcp, &host).await?;
        h2_upgrade(TokioIo::new(tls_stream), &host, &path, tls, protocol_header, headers).await
    } else {
        h2_upgrade(TokioIo::new(tcp), &host, &path, tls, protocol_header, headers).await
    }
}

async fn h2_upgrade<T>(
    io: T,
    host: &str,
    path: &str,
    tls: bool,
    protocol_header: &str,
    headers: &[(&'static str, &'static str)],
) -> Result<(WsClient, WsConnectResponse)>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, conn) = http2::Builder::new(TokioExecutor::new())
        .handshake(io)
        .await
        .context("HTTP/2 handshake failed")?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!("H2 connection driver exited: {}", e);
        }
    });

    let scheme = if tls { "https" } else { "http" };
    let mut builder = hyper::Request::builder()
        .method(hyper::Method::CONNECT)
        .uri(format!("{}://{}{}", scheme, host, path))
        .header("sec-websocket-version", "13");
    if !protocol_header.is_empty() {
        builder = builder.header("sec-websocket-protocol", protocol_header);
    }
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let mut request = builder
        .body(Empty::<Bytes>::new())
        .context("Failed to build H2 upgrade request")?;

    // :protocol pseudo-header must be set as an extension, not a raw header
    request
        .extensions_mut()
        .insert(hyper::ext::Protocol::from_static("websocket"));

    // Capture the upgrade future BEFORE sending (stores the oneshot sender in request extensions)
    let on_upgrade = hyper::upgrade::on(&mut request);

    let response = sender
        .send_request(request)
        .await
        .context("Failed to send H2 upgrade request")?;

    ensure!(
        response.status() == StatusCode::OK,
        "H2 WebSocket upgrade rejected: {}",
        response.status()
    );

    let conn_resp = WsConnectResponse {
        status: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
    };

    let upgraded = on_upgrade.await.context("H2 upgrade failed")?;
    let ws =
        WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, None).await;

    Ok((WsClient { stream: WsStream::H2(ws), seq: 0 }, conn_resp))
}

async fn make_tls_stream(
    tcp: TcpStream,
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"h2".to_vec()];

    let connector = TlsConnector::from(Arc::new(tls_config));
    let server_name =
        ServerName::try_from(host.to_owned()).context("Invalid TLS server name")?;
    connector
        .connect(server_name, tcp)
        .await
        .context("TLS handshake failed")
}
