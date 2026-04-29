use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use eyre::{Context, Result, bail, ensure, eyre};
use futures::SinkExt;
use futures::StreamExt;
use http_body_util::Empty;
use hyper::StatusCode;
use hyper::client::conn::http2;
use hyper::header::HeaderValue;
use hyper_util::rt::{TokioExecutor, TokioIo};
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
use crate::libs::ws::{WsLogResponse, WsRequest, WsRequestGeneric, WsResponseGeneric};

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
    ) -> Result<(
        Self,
        tokio_tungstenite::tungstenite::http::Response<std::option::Option<Vec<u8>>>,
    )> {
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
        Ok((
            Self {
                stream: WsStream::H1(ws_stream),
                seq: 0,
            },
            response,
        ))
    }

    // --- Private stream helpers -------------------------------------------

    async fn stream_send(&mut self, msg: Message) -> Result<()> {
        match &mut self.stream {
            WsStream::H1(s) => s.send(msg).await?,
            WsStream::H2(s) => s.send(msg).await?,
        }
        Ok(())
    }

    async fn stream_next(
        &mut self,
    ) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
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

    // --- Public API --------------------------------

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

    pub async fn recv_raw(&mut self) -> Result<serde_json::Value> {
        let msg = self
            .stream_next()
            .await
            .ok_or(eyre!("Connection closed"))??;
        let text = match msg {
            Message::Text(text) => text,
            other => bail!("Expected text message, got: {:?}", other),
        };
        debug!("recv raw: {}", text);
        let resp: serde_json::Value = serde_json::from_str(&text)?;
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
    danger_accept_invalid_certs: bool,
}

impl WsClientBuilder {
    pub fn new() -> Self {
        Self {
            mode: WsVersionMode::Http1Only,
            protocol_header: String::new(),
            headers: Vec::new(),
            danger_accept_invalid_certs: false,
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

    pub fn danger_accept_invalid_certs(mut self) -> Self {
        self.danger_accept_invalid_certs = true;
        self
    }

    pub async fn build(self, connect_addr: &str) -> Result<(WsClient, WsConnectResponse)> {
        let danger = self.danger_accept_invalid_certs;
        match self.mode {
            WsVersionMode::Http1Only => {
                connect_h1(connect_addr, &self.protocol_header, &self.headers, danger).await
            }
            WsVersionMode::Http2Only => {
                connect_h2(connect_addr, &self.protocol_header, &self.headers, danger).await
            }
            WsVersionMode::Auto => {
                match connect_h2(connect_addr, &self.protocol_header, &self.headers, danger).await {
                    Ok(result) => Ok(result),
                    Err(h2_err) => {
                        debug!(
                            "H2 connection failed ({}), falling back to HTTP/1.1",
                            h2_err
                        );
                        connect_h1(connect_addr, &self.protocol_header, &self.headers, danger).await
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

    Ok(ParsedUrl {
        tls,
        host,
        port,
        path,
    })
}

async fn connect_h1(
    addr: &str,
    protocol_header: &str,
    headers: &[(&'static str, &'static str)],
    danger_accept_invalid_certs: bool,
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

    let (ws_stream, response) = if danger_accept_invalid_certs {
        let connector = tokio_tungstenite::Connector::Rustls(Arc::new(make_dangerous_tls_config()));
        tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(connector))
            .await
            .context("Failed to connect to endpoint")?
    } else {
        connect_async(req)
            .await
            .context("Failed to connect to endpoint")?
    };

    let conn_resp = WsConnectResponse {
        status: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
    };

    Ok((
        WsClient {
            stream: WsStream::H1(ws_stream),
            seq: 0,
        },
        conn_resp,
    ))
}

async fn connect_h2(
    addr: &str,
    protocol_header: &str,
    headers: &[(&'static str, &'static str)],
    danger_accept_invalid_certs: bool,
) -> Result<(WsClient, WsConnectResponse)> {
    let ParsedUrl {
        tls,
        host,
        port,
        path,
    } = parse_ws_url(addr)?;

    debug!(host, port, path, tls, "H2: resolving host");
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(format!("{}:{}", host, port))
        .await
        .context("DNS resolution failed")?
        .collect();

    if addrs.is_empty() {
        return Err(eyre!("No addresses returned for {}:{}", host, port));
    }
    debug!(?addrs, "H2: resolved addresses");

    let mut tcp = None;
    let mut last_err = None;
    for addr in &addrs {
        debug!(%addr, "H2: attempting TCP connect");
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                debug!(%addr, "H2: TCP connected");
                tcp = Some(stream);
                break;
            }
            Err(e) => {
                debug!(%addr, err=%e, "H2: TCP connect failed, trying next address");
                last_err = Some(e);
            }
        }
    }
    let tcp = tcp.ok_or_else(|| {
        eyre!(
            "TCP connect failed for all addresses {:?}: {}",
            addrs,
            last_err.unwrap()
        )
    })?;
    tcp.set_nodelay(true)?;

    if tls {
        debug!(host, "H2: starting TLS handshake");
        let tls_stream = make_tls_stream(tcp, &host, danger_accept_invalid_certs).await?;
        debug!(host, "H2: TLS handshake complete, starting H2 upgrade");
        h2_upgrade(
            TokioIo::new(tls_stream),
            &host,
            &path,
            tls,
            protocol_header,
            headers,
        )
        .await
    } else {
        debug!(host, "H2: plain TCP, starting H2 upgrade (h2c)");
        h2_upgrade(
            TokioIo::new(tcp),
            &host,
            &path,
            tls,
            protocol_header,
            headers,
        )
        .await
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
    debug!(host, "H2: HTTP/2 connection handshake complete");
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!("H2 connection driver exited: {}", e);
        }
    });

    let scheme = if tls { "https" } else { "http" };
    let uri = format!("{}://{}{}", scheme, host, path);
    debug!(uri, protocol_header, "H2: sending CONNECT upgrade request");
    let mut builder = hyper::Request::builder()
        .method(hyper::Method::CONNECT)
        .uri(&uri)
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

    let mut response = sender
        .send_request(request)
        .await
        .context("Failed to send H2 upgrade request")?;

    debug!(status=%response.status(), "H2: received upgrade response");
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

    // For H2 CONNECT the tunnel is the response stream itself — upgrade::on must be
    // called on the response (not the request) to obtain the bidirectional tunnel.
    let upgraded = hyper::upgrade::on(&mut response)
        .await
        .context("H2 upgrade failed")?;
    debug!(host, "H2: upgrade completed, WebSocket stream ready");
    let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, None).await;

    Ok((
        WsClient {
            stream: WsStream::H2(ws),
            seq: 0,
        },
        conn_resp,
    ))
}

async fn make_tls_stream(
    tcp: TcpStream,
    host: &str,
    danger_accept_invalid_certs: bool,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut tls_config = if danger_accept_invalid_certs {
        make_dangerous_tls_config()
    } else {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    };
    tls_config.alpn_protocols = vec![b"h2".to_vec()];

    let connector = TlsConnector::from(Arc::new(tls_config));
    let server_name = ServerName::try_from(host.to_owned()).context("Invalid TLS server name")?;
    connector
        .connect(server_name, tcp)
        .await
        .context("TLS handshake failed")
}

fn make_dangerous_tls_config() -> rustls::ClientConfig {
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAllVerifier))
        .with_no_client_auth()
}

#[derive(Debug)]
struct AcceptAllVerifier;

impl rustls::client::danger::ServerCertVerifier for AcceptAllVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}
