use eyre::{Context, Result, bail, eyre};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::*;

// --- nago-wss/nagoya: only for the TCP(/TLS) constructors -------------------
//
// The tokio half of this file is gone. `tokio_tungstenite`, `tokio::net`,
// `tokio_rustls` and the hyper HTTP/2 path have been replaced by nago-wss over
// a nagoya reactor, which is the same substitution `server.rs` already made.
// What that costs the caller is documented on [`WsClient::new`]: a nagoya
// socket is bound to one reactor at birth and only makes progress while that
// reactor is polled, so the reactor is now a parameter rather than something
// an ambient `#[tokio::main]` supplied invisibly.
#[cfg(feature = "ws-client")]
use nago_wss::conn::Connection;
#[cfg(feature = "ws-client")]
use nago_wss::proto::message::{CloseFrame as NagoCloseFrame, Limits, Message as NagoMessage};
#[cfg(feature = "ws-client")]
use nago_wss::stream::{ByteStream, StreamExt};
#[cfg(feature = "ws-client")]
use nagoya::reactor::{Addr, Handle, TcpStream, connect_any, resolve};
#[cfg(feature = "ws-client")]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::libs::log::LogLevel;
use crate::libs::ws::WireMessage as Message;
#[cfg(feature = "ws-client")]
use crate::libs::ws::{CloseFrame, Utf8Bytes};
use crate::libs::ws::{WsLogResponse, WsRequest, WsRequestGeneric, WsResponseGeneric};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which HTTP version to use when connecting.
///
/// # There is only one answer now
///
/// HTTP/2 extended CONNECT (RFC 8441) has been removed from this client. It was
/// never negotiable without ALPN, ALPN is a TLS feature, and the server side of
/// this crate no longer terminates TLS: the fleet runs plain `ws://` behind a
/// fly.io edge that terminates for it. An h2 attempt against that topology can
/// only fail and fall back, so the fallback is all that is left.
///
/// The enum survives because two fleet repositories name it
/// (`honey_id-types`, `auth.honey.id-backend`) and deleting it would break them
/// at the `use` line rather than at the one place the behaviour actually
/// changed. Every variant now selects the HTTP/1.1 upgrade handshake; the two
/// that used to mean something else are deprecated rather than silently
/// honoured, so a caller asking for h2 is told, at compile time, that it is not
/// getting it.
#[derive(Debug, Clone, Copy, Default)]
#[cfg(feature = "ws-client")]
pub enum WsVersionMode {
    /// HTTP/1.1 upgrade handshake. The only behaviour.
    #[default]
    Http1Only,
    /// Formerly HTTP/2 extended CONNECT. Now an alias for [`Self::Http1Only`].
    #[deprecated(note = "HTTP/2 extended CONNECT was removed; this connects over HTTP/1.1")]
    Http2Only,
    /// Formerly "h2 first, fall back to HTTP/1.1". Now an alias for [`Self::Http1Only`].
    #[deprecated(note = "HTTP/2 extended CONNECT was removed; this connects over HTTP/1.1")]
    Auto,
}

/// Response metadata returned by [`WsClientBuilder::build`].
///
/// `headers` is no longer the server's whole response head. nago-wss's
/// handshake consumes the head, validates the `Sec-WebSocket-Accept` value
/// against the key it sent and reports only the negotiated subprotocol, so the
/// rest is gone by the time this is built. Reporting the one header it does
/// know is honest; inventing the others would not be. Callers that need more
/// of the head need it surfaced from nago-wss first.
#[cfg(feature = "ws-client")]
pub struct WsConnectResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The subprotocol the server selected, if any.
    pub protocol: Option<String>,
}

// ---------------------------------------------------------------------------
// Internal stream abstraction
// ---------------------------------------------------------------------------

enum WsStream {
    #[cfg(feature = "ws-client")]
    Plain(Box<Connection<TcpStream>>),
    /// A `wss://` connection. See [`WsTarget::resolve`] for why this is behind
    /// a feature of its own rather than compiled unconditionally.
    #[cfg(all(feature = "ws-client", feature = "ws-client-tls"))]
    Secure(Box<Connection<nago_wss::tls::TlsStream<TcpStream>>>),
    /// Any transport-agnostic message channel — a framed Unix socket, a named pipe,
    /// an XPC connection. Added in 2.0 alongside [`WsClient::from_stream`].
    Message(Box<dyn crate::libs::ws::MessageStream>),
}

// ---------------------------------------------------------------------------
// WsClient
// ---------------------------------------------------------------------------

pub struct WsClient {
    stream: WsStream,
    seq: u32,
}

impl WsClient {
    /// Connect over TCP and perform the HTTP/1.1 upgrade handshake.
    ///
    /// # The reactor is a parameter now
    ///
    /// **Breaking change: this takes a [`Handle`], and its second return value
    /// is a [`WsConnectResponse`] rather than a tungstenite `http::Response`.**
    ///
    /// tokio supplied an ambient runtime, so a client could be built from
    /// anywhere inside `#[tokio::main]` and the sockets it opened found a
    /// driver by themselves. nagoya has no ambient anything: a descriptor is
    /// registered with one specific reactor when it is created, and only that
    /// reactor will ever report its readiness. A connection opened against a
    /// reactor nobody polls does not connect slowly, it never completes at all.
    /// Making the handle an argument is the only way that constraint is visible
    /// at the call site, and it matches what
    /// [`WebsocketServer::listen`](crate::libs::ws::WebsocketServer::listen)
    /// already does on the other side of the wire.
    ///
    /// The caller owns the reactor, exactly as the server does:
    ///
    /// ```ignore
    /// let reactor = nagoya::reactor::Reactor::local()?;
    /// let handle = reactor.handle();
    /// // Resolve first: see `WsTarget::resolve`.
    /// let target = WsTarget::resolve("ws://127.0.0.1:8443/")?;
    /// nagoya::reactor::block_on_with(&reactor, async {
    ///     let (client, _) = WsClientBuilder::new().connect(&target, &handle).await?;
    ///     // ... use the client while this future runs ...
    ///     Ok::<_, eyre::Report>(())
    /// })?;
    /// ```
    ///
    /// This convenience form resolves the name itself, which blocks — see
    /// [`WsTarget::resolve`]. Use [`WsTarget`] plus
    /// [`WsClientBuilder::connect`] when a reactor is already running.
    #[cfg(feature = "ws-client")]
    pub async fn new(
        connect_addr: &str,
        protocol_header: &str,
        headers: Option<Vec<(&'static str, &'static str)>>,
        handle: &Handle,
    ) -> Result<(Self, WsConnectResponse)> {
        let mut builder = WsClientBuilder::new().protocol_header(protocol_header);
        if let Some(headers) = headers {
            builder = builder.headers(headers);
        }
        builder.build(connect_addr, handle).await
    }

    /// Build a client over any [`MessageStream`], bypassing TCP/TLS entirely.
    ///
    /// The transport-agnostic counterpart to [`Self::new`], and the client-side mirror
    /// of [`WebsocketServer::serve_connection`](crate::libs::ws::WebsocketServer::serve_connection).
    /// All the request/reply machinery — sequence correlation, response routing, MCP
    /// framing — is shared with the WebSocket path; only the byte plumbing differs.
    ///
    /// Use with `framed_json_neutral` over a Unix socket or inherited
    /// socketpair, or with a platform transport's own `MessageStream`
    /// implementation.
    ///
    /// `MessageStream`'s futures are not `Send`, so this must be polled on the
    /// thread that owns the stream. `nagoya::reactor::TaskSet` driven by
    /// `block_on_with` is the non-`Send` task runner here; a bare
    /// `nagoya::block_on` does for one connection.
    pub fn from_stream(stream: Box<dyn crate::libs::ws::MessageStream>) -> Self {
        Self {
            stream: WsStream::Message(stream),
            seq: 0,
        }
    }

    // --- Private stream helpers -------------------------------------------

    async fn stream_send(&mut self, msg: Message) -> Result<()> {
        // Backend edge: the client speaks WireMessage; nago-wss's message type
        // exists only inside these helpers and the two conversions below.
        match &mut self.stream {
            #[cfg(feature = "ws-client")]
            WsStream::Plain(c) => send_to(c, msg).await?,
            #[cfg(all(feature = "ws-client", feature = "ws-client-tls"))]
            WsStream::Secure(c) => send_to(c, msg).await?,
            WsStream::Message(s) => s
                .send(msg)
                .await
                .map_err(|err| eyre!("message stream send failed: {err}"))?,
        }
        Ok(())
    }

    async fn stream_next(&mut self) -> Option<Result<Message>> {
        match &mut self.stream {
            #[cfg(feature = "ws-client")]
            WsStream::Plain(c) => next_from(c).await,
            #[cfg(all(feature = "ws-client", feature = "ws-client-tls"))]
            WsStream::Secure(c) => next_from(c).await,
            WsStream::Message(s) => s
                .recv()
                .await
                .map(|res| res.map_err(|err| eyre!("message stream recv failed: {err}"))),
        }
    }

    async fn stream_close(&mut self) -> Result<()> {
        match &mut self.stream {
            #[cfg(feature = "ws-client")]
            WsStream::Plain(c) => close_to(c).await?,
            #[cfg(all(feature = "ws-client", feature = "ws-client-tls"))]
            WsStream::Secure(c) => close_to(c).await?,
            WsStream::Message(s) => {
                // No protocol-level close handshake on a plain message channel:
                // send the Close frame and let the transport tear down.
                let _ = s.send(Message::Close(None)).await;
            }
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
        debug!(
            method,
            seq = self.seq,
            bytes = req.len(),
            "sending WebSocket request"
        );
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
            other => {
                #[allow(unreachable_patterns)]
                let kind = match &other {
                    Message::Text(_) => "text",
                    Message::Binary(_) => "binary",
                    Message::Ping(_) => "ping",
                    Message::Pong(_) => "pong",
                    Message::Close(_) => "close",
                    _ => "unknown",
                };
                bail!("Expected text message, got {kind}")
            }
        };
        debug!(bytes = text.len(), "received raw WebSocket response");
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
                    debug!(bytes = text.len(), "received WebSocket response");
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
// The nago-wss edge
// ---------------------------------------------------------------------------
//
// Three helpers, generic over the byte stream, so the plain and TLS variants of
// `WsStream` share one implementation instead of two that drift. The match arms
// above are dispatch and nothing else.

#[cfg(feature = "ws-client")]
async fn send_to<S: ByteStream + StreamExt>(
    conn: &mut Connection<S>,
    message: Message,
) -> Result<()> {
    conn.write(into_nago(message))
        .await
        .map_err(|err| eyre!("websocket write failed: {err}"))
}

#[cfg(feature = "ws-client")]
async fn next_from<S: ByteStream + StreamExt>(conn: &mut Connection<S>) -> Option<Result<Message>> {
    match conn.read().await {
        // A clean close handshake. `None` is "the stream ended", which is what
        // every caller above already treats as a closed connection.
        Ok(None) => None,
        // tokio-tungstenite answered pings inside its own `Stream` impl and
        // still yielded the Ping to the caller; nago-wss does neither, because
        // it will not write behind its caller's back. Answering here keeps the
        // observable behaviour identical rather than leaving a client that
        // looks alive to itself and dead to any server with a ping timeout.
        // §5.5.2: the payload must be echoed exactly, so it is cloned rather
        // than rebuilt — `Bytes` makes that a refcount bump.
        Ok(Some(NagoMessage::Ping(payload))) => {
            if let Err(err) = conn.pong(payload.clone()).await {
                return Some(Err(eyre!("failed to answer ping: {err}")));
            }
            Some(Ok(Message::Ping(payload)))
        }
        Ok(Some(other)) => Some(Ok(from_nago(other))),
        Err(err) => Some(Err(eyre!("websocket read failed: {err}"))),
    }
}

#[cfg(feature = "ws-client")]
async fn close_to<S: ByteStream + StreamExt>(conn: &mut Connection<S>) -> Result<()> {
    // Sending a second close is a no-op inside nago-wss, so this stays safe to
    // call from both `close()` and the Close branch of `recv_resp`.
    conn.close(None)
        .await
        .map_err(|err| eyre!("websocket close failed: {err}"))
}

/// nago-wss message -> [`WireMessage`](crate::libs::ws::WireMessage).
///
/// Both types carry `Bytes`, so this moves refcounts rather than payloads: the
/// buffer the reactor read into reaches the caller without being copied.
#[cfg(feature = "ws-client")]
fn from_nago(message: NagoMessage) -> Message {
    match message {
        // SAFETY: nago-wss validates a text payload during reassembly (over the
        // joined fragments, so a multi-byte character split across a boundary is
        // handled) and rejects the frame otherwise. Re-scanning here would
        // repeat that work on every message for no additional guarantee.
        NagoMessage::Text(payload) => {
            Message::Text(unsafe { Utf8Bytes::from_bytes_unchecked(payload) })
        }
        NagoMessage::Binary(payload) => Message::Binary(payload),
        NagoMessage::Ping(payload) => Message::Ping(payload),
        NagoMessage::Pong(payload) => Message::Pong(payload),
        NagoMessage::Close(frame) => Message::Close(frame.map(|frame| CloseFrame {
            code: frame.code.0,
            // SAFETY: as above — a close reason is checked for UTF-8 when the
            // close body is parsed, and a frame that fails never gets here.
            reason: unsafe { Utf8Bytes::from_bytes_unchecked(frame.reason) },
        })),
    }
}

/// [`WireMessage`](crate::libs::ws::WireMessage) -> nago-wss message.
#[cfg(feature = "ws-client")]
fn into_nago(message: Message) -> NagoMessage {
    match message {
        Message::Text(text) => NagoMessage::Text(text.into_bytes()),
        Message::Binary(payload) => NagoMessage::Binary(payload),
        Message::Ping(payload) => NagoMessage::Ping(payload),
        Message::Pong(payload) => NagoMessage::Pong(payload),
        Message::Close(frame) => NagoMessage::Close(frame.map(|frame| NagoCloseFrame {
            code: nago_wss::CloseCode(frame.code),
            reason: frame.reason.into_bytes(),
        })),
    }
}

// ---------------------------------------------------------------------------
// WsTarget: the blocking half of connecting
// ---------------------------------------------------------------------------

/// A parsed and resolved WebSocket URL, ready to connect to.
///
/// This exists to separate the one blocking step from the asynchronous ones.
/// See [`Self::resolve`].
#[cfg(feature = "ws-client")]
pub struct WsTarget {
    secure: bool,
    /// The host as written, which is what SNI and certificate validation use.
    host: String,
    /// The `Host` header value: the same host, with the port when it is not the
    /// scheme's default.
    host_header: String,
    path: String,
    addrs: Vec<Addr>,
}

#[cfg(feature = "ws-client")]
impl WsTarget {
    /// Parse a `ws://` or `wss://` URL and resolve its host.
    ///
    /// # Why this is a separate, synchronous step
    ///
    /// [`resolve`] is `getaddrinfo`. It blocks the calling thread for as long
    /// as the platform resolver takes, which on a DNS timeout is seconds, and
    /// nagoya has no `spawn_blocking` to put it anywhere else. The arrangement
    /// this crate uses is the one where the polling thread *is* the reactor
    /// thread, so resolving from inside a running reactor stalls every other
    /// socket on it for the whole lookup — not just this client's.
    ///
    /// So the name is resolved here, before any reactor is driven, and
    /// [`WsClientBuilder::connect`] takes the result. That is the same order
    /// [`WebsocketServer::listen`](crate::libs::ws::WebsocketServer::listen)
    /// follows on the server side: resolve, create the reactor, then poll.
    ///
    /// `nago_wss::client::connect_plain` would do both in one call and is
    /// deliberately not used for that reason: it resolves inside the future,
    /// which is precisely the stall nagoya's documentation warns about.
    ///
    /// Every address the name has is kept, not the first. A host with both an A
    /// and an AAAA record is ordinary, and taking one family fails on any
    /// machine where the listener is on the other.
    pub fn resolve(url: &str) -> Result<Self> {
        let url = nago_wss::client::Url::parse(url)
            .map_err(|err| eyre!("invalid WebSocket URL {url}: {err}"))?;

        #[cfg(not(feature = "ws-client-tls"))]
        if url.secure {
            bail!(
                "wss:// needs the `ws-client-tls` feature; this build speaks plain ws:// only \
                 (the fleet terminates TLS at the edge)"
            );
        }

        let host_header = url.host_header();
        debug!(host = %url.host, port = url.port, secure = url.secure, "resolving WebSocket host");
        let addrs = resolve(&url.host, url.port)
            .map_err(|err| eyre!("DNS resolution failed for {}: {err}", url.host))?;
        debug!(?addrs, "resolved WebSocket host");

        Ok(Self {
            secure: url.secure,
            host: url.host,
            host_header,
            path: url.path,
            addrs,
        })
    }

    /// Whether this target is `wss://`.
    pub fn is_secure(&self) -> bool {
        self.secure
    }
}

// ---------------------------------------------------------------------------
// WsClientBuilder
// ---------------------------------------------------------------------------

#[cfg(feature = "ws-client")]
pub struct WsClientBuilder {
    protocol_header: String,
    headers: Vec<(&'static str, &'static str)>,
    /// Only the TLS path reads this, and the TLS path is optional. Without the
    /// feature the field is still set — the builder method stays callable so a
    /// caller does not have to feature-gate its own code — and never read.
    #[cfg_attr(not(feature = "ws-client-tls"), allow(dead_code))]
    danger_accept_invalid_certs: bool,
}

#[cfg(feature = "ws-client")]
impl WsClientBuilder {
    pub fn new() -> Self {
        Self {
            protocol_header: String::new(),
            headers: Vec::new(),
            danger_accept_invalid_certs: false,
        }
    }

    /// Select the HTTP version. Retained for source compatibility; ignored.
    ///
    /// Every [`WsVersionMode`] now means HTTP/1.1. The call is kept so the two
    /// fleet repositories that make it keep compiling, and warns at runtime
    /// when it is given a mode that used to mean something else, because a
    /// silently downgraded connection is exactly the kind of thing that gets
    /// diagnosed twice.
    #[allow(deprecated)]
    pub fn mode(self, mode: WsVersionMode) -> Self {
        if !matches!(mode, WsVersionMode::Http1Only) {
            warn!(
                ?mode,
                "HTTP/2 extended CONNECT was removed from this client; connecting over HTTP/1.1"
            );
        }
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

    /// Accept any server certificate. `wss://` only, and a development tool.
    pub fn danger_accept_invalid_certs(mut self) -> Self {
        self.danger_accept_invalid_certs = true;
        self
    }

    /// Resolve `connect_addr` and connect, in one call.
    ///
    /// Convenience for a caller that has not entered a reactor yet — a test, a
    /// CLI, the first connection a process makes. **It resolves inside the
    /// returned future**, so a reactor that is already serving other sockets
    /// stalls for the length of the lookup. Anything long-lived should call
    /// [`WsTarget::resolve`] before entering the reactor and then
    /// [`Self::connect`]; that is the whole reason the two are separable.
    pub async fn build(
        self,
        connect_addr: &str,
        handle: &Handle,
    ) -> Result<(WsClient, WsConnectResponse)> {
        let target = WsTarget::resolve(connect_addr)?;
        self.connect(&target, handle).await
    }

    /// Connect to an already resolved target on the reactor `handle` names.
    ///
    /// The socket this opens belongs to that reactor and makes progress only
    /// while it is polled. See [`WsClient::new`].
    pub async fn connect(
        self,
        target: &WsTarget,
        handle: &Handle,
    ) -> Result<(WsClient, WsConnectResponse)> {
        // The subprotocol goes out as one header value, exactly as it was
        // written. It is not a list this crate composes: the fleet's auth
        // scheme puts `0<method>, 1<key>` in that field, and splitting it on
        // the comma and rejoining it would be a round trip through a meaning
        // the string does not have.
        let protocols: Vec<&str> = if self.protocol_header.is_empty() {
            Vec::new()
        } else {
            vec![self.protocol_header.as_str()]
        };
        let headers: Vec<(&str, &str)> = self.headers.iter().map(|(k, v)| (*k, *v)).collect();

        debug!(
            host = %target.host,
            secure = target.secure,
            has_protocol_header = !protocols.is_empty(),
            additional_header_count = headers.len(),
            "connecting WebSocket client"
        );

        if target.secure {
            #[cfg(feature = "ws-client-tls")]
            {
                return connect_secure(
                    target,
                    handle,
                    &protocols,
                    &headers,
                    self.danger_accept_invalid_certs,
                )
                .await;
            }
            #[cfg(not(feature = "ws-client-tls"))]
            {
                // Unreachable through `WsTarget::resolve`, which refuses a
                // `wss://` URL in this build. Kept so the branch is not a
                // silent plaintext connection if a target is ever built
                // another way.
                bail!("wss:// needs the `ws-client-tls` feature");
            }
        }

        let stream = connect_any(&target.addrs, handle)
            .await
            .map_err(|err| eyre!("TCP connect failed for {:?}: {err}", target.addrs))?;

        let (connection, protocol) = nago_wss::upgrade::connect(
            stream,
            &target.path,
            &target.host_header,
            &protocols,
            &headers,
            handshake_entropy(),
            Limits::default(),
        )
        .await
        .map_err(|err| eyre!("WebSocket upgrade failed: {err}"))?;

        Ok((
            WsClient {
                stream: WsStream::Plain(Box::new(connection)),
                seq: 0,
            },
            connect_response(protocol),
        ))
    }
}

#[cfg(feature = "ws-client")]
impl Default for WsClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// A handshake succeeded; this is what the caller is told about it.
///
/// The status is 101 by construction: nago-wss's `check_response` returns an
/// error for anything else, so a value here means the upgrade happened.
#[cfg(feature = "ws-client")]
fn connect_response(protocol: Option<String>) -> WsConnectResponse {
    let headers = match &protocol {
        Some(protocol) => vec![("sec-websocket-protocol".to_string(), protocol.clone())],
        None => Vec::new(),
    };
    WsConnectResponse {
        status: 101,
        headers,
        protocol,
    }
}

/// Sixteen bytes for the `Sec-WebSocket-Key`.
///
/// §4.1 wants a value a cache or a proxy cannot predict, so it cannot replay a
/// 101 it liked the look of. It is sent in cleartext in the request and is not
/// a secret, which is why this is the clock, a counter and an address rather
/// than a CSPRNG this crate would otherwise not depend on.
///
/// The counter is what makes two connections opened in the same nanosecond
/// differ, which is ordinary for a client that opens several at once; the
/// address varies between processes with ASLR, which the clock alone does not
/// give on a machine where two processes start together. nago-wss's own default
/// is a fixed constant it documents as the thing to avoid, so the parameter is
/// always supplied rather than defaulted.
#[cfg(feature = "ws-client")]
fn handshake_entropy() -> [u8; 16] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| value.as_nanos() as u64);
    let counted = COUNTER.fetch_add(1, Ordering::Relaxed);
    let local = 0u8;
    let address = core::ptr::addr_of!(local) as u64;

    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&nanos.to_ne_bytes());
    out[8..].copy_from_slice(&(counted ^ address).to_ne_bytes());
    out
}

// ---------------------------------------------------------------------------
// TLS, for a client that still dials an external wss://
// ---------------------------------------------------------------------------
//
// Behind `ws-client-tls`, which is not in the default set and must forward
// `nago-wss/tls` (and `nago-wss/webpki-roots` for the bundled trust anchors).
// Off by default on purpose: the fleet's own services are plain `ws://` with
// TLS terminated at fly.io, and nago-rustls drags `std` and a certificate stack
// into a graph the internal services are trying to keep no_std-friendly. A
// build that never dials an external `wss://` should never compile any of this.

#[cfg(all(feature = "ws-client", feature = "ws-client-tls"))]
async fn connect_secure(
    target: &WsTarget,
    handle: &Handle,
    protocols: &[&str],
    headers: &[(&str, &str)],
    danger_accept_invalid_certs: bool,
) -> Result<(WsClient, WsConnectResponse)> {
    // Through nago-wss's re-export, so this crate never names a rustls version
    // of its own and cannot end up linking a second, incompatible one.
    use nago_wss::tls::rustls_pki_types::ServerName;
    use nago_wss::tls::{TlsStream, rustls};
    use std::sync::Arc;

    let stream = connect_any(&target.addrs, handle)
        .await
        .map_err(|err| eyre!("TCP connect failed for {:?}: {err}", target.addrs))?;

    let config: Arc<rustls::ClientConfig> = if danger_accept_invalid_certs {
        Arc::new(make_dangerous_tls_config())
    } else {
        nago_wss::tls::default_client_config()
    };

    // SNI and certificate validation both key off the name from the URL, not
    // the address that answered.
    let name = ServerName::try_from(target.host.clone())
        .map_err(|_| eyre!("invalid TLS server name: {}", target.host))?;
    let session = rustls::ClientConnection::new(config, name)
        .map_err(|err| eyre!("TLS session setup failed: {err}"))?;

    let mut tls = TlsStream::client(stream, session);
    // Driven explicitly so a certificate failure surfaces here, naming TLS,
    // rather than in the middle of the WebSocket handshake that follows.
    tls.handshake()
        .await
        .map_err(|err| eyre!("TLS handshake failed: {err}"))?;

    let (connection, protocol) = nago_wss::upgrade::connect(
        tls,
        &target.path,
        &target.host_header,
        protocols,
        headers,
        handshake_entropy(),
        Limits::default(),
    )
    .await
    .map_err(|err| eyre!("WebSocket upgrade failed: {err}"))?;

    Ok((
        WsClient {
            stream: WsStream::Secure(Box::new(connection)),
            seq: 0,
        },
        connect_response(protocol),
    ))
}

#[cfg(all(feature = "ws-client", feature = "ws-client-tls"))]
fn make_dangerous_tls_config() -> nago_wss::tls::rustls::ClientConfig {
    use nago_wss::tls::rustls;
    use std::sync::Arc;

    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAllVerifier))
        .with_no_client_auth()
}

#[derive(Debug)]
#[cfg(all(feature = "ws-client", feature = "ws-client-tls"))]
struct AcceptAllVerifier;

#[cfg(all(feature = "ws-client", feature = "ws-client-tls"))]
impl nago_wss::tls::rustls::client::danger::ServerCertVerifier for AcceptAllVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &nago_wss::tls::rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[nago_wss::tls::rustls_pki_types::CertificateDer<'_>],
        _server_name: &nago_wss::tls::rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: nago_wss::tls::rustls_pki_types::UnixTime,
    ) -> std::result::Result<
        nago_wss::tls::rustls::client::danger::ServerCertVerified,
        nago_wss::tls::rustls::Error,
    > {
        Ok(nago_wss::tls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &nago_wss::tls::rustls_pki_types::CertificateDer<'_>,
        _dss: &nago_wss::tls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        nago_wss::tls::rustls::client::danger::HandshakeSignatureValid,
        nago_wss::tls::rustls::Error,
    > {
        Ok(nago_wss::tls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &nago_wss::tls::rustls_pki_types::CertificateDer<'_>,
        _dss: &nago_wss::tls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        nago_wss::tls::rustls::client::danger::HandshakeSignatureValid,
        nago_wss::tls::rustls::Error,
    > {
        Ok(nago_wss::tls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<nago_wss::tls::rustls::SignatureScheme> {
        use nago_wss::tls::rustls::SignatureScheme;
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}
