use std::net::SocketAddr;

use async_trait::async_trait;
use eyre::{Result, eyre};
use httparse::Request as HttpRequest;
use tokio::io::AsyncReadExt;
use tracing::*;
use wtx::{
    collection::Vector,
    http::{Headers, HttpRecvParams, Protocol},
    http2::{Http2, Http2Buffer, WebSocketOverStream},
    rng::Xorshift64,
    stream::Stream,
    web_socket::{Frame, OpCode, WebSocket, WebSocketBuffer, WebSocketPayloadOrigin},
};

use super::stream::TokioStreamAdapter;
use crate::libs::ws::{
    WsMessage as Message, WsServerConfig,
    traits::{BoxedStream, StreamError, WsStream, WsUpgrader},
};

const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

pub struct WtxUpgrader;

#[async_trait]
impl WsUpgrader for WtxUpgrader {
    async fn upgrade(
        &self,
        stream: BoxedStream,
        addr: SocketAddr,
        config: &WsServerConfig,
        cached_date: &str,
    ) -> Result<(Box<dyn WsStream>, String)> {
        // Try H2 first by attempting an H2 preface read. If the first bytes
        // match the H2 preface, route to the H2 upgrader. Otherwise replay
        // the peeked bytes and fall through to the H1 (WebSocketAcceptor) path.
        let mut peek_buf = [0u8; 24];
        let mut stream = stream;
        let n = stream
            .read(&mut peek_buf)
            .await
            .map_err(|e| eyre!("failed to read for protocol detection: {e}"))?;

        if n == 0 {
            return Err(eyre!("connection closed before protocol detection"));
        }

        let is_h2 = n == 24
            && peek_buf[0..4] == [b'P', b'R', b'I', b' ']
            && peek_buf[4..16] == *b"* HTTP/2.0\r\n"
            && peek_buf[16..18] == [b'\r', b'\n']
            && peek_buf[18..20] == [b'S', b'M']
            && peek_buf[20..22] == [b'\r', b'\n']
            && peek_buf[22..24] == [b'\r', b'\n'];

        if is_h2 {
            debug!(ws_server = true, ?addr, "H2 connection detected");
            // Http2::accept() reads the preface itself — replay the consumed bytes.
            let replayed: BoxedStream =
                Box::new(TokioStreamAdapter::with_prefix(stream, &peek_buf[..n]));
            let (read_half, write_half) = tokio::io::split(replayed);
            upgrade_h2(read_half, write_half, addr).await
        } else {
            debug!(ws_server = true, ?addr, "H1 connection detected");
            upgrade_h1(TokioStreamAdapter::with_prefix(stream, &peek_buf[..n]), addr, config, cached_date).await
        }
    }
}

// -- H1 path --

async fn upgrade_h1(
    mut stream: TokioStreamAdapter,
    addr: SocketAddr,
    _config: &WsServerConfig,
    _cached_date: &str,
) -> Result<(Box<dyn WsStream>, String)> {
    use base64::Engine as _;
    use sha1::{Digest, Sha1};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Read raw HTTP request until \r\n\r\n, parsing only bytes actually received
    // (avoids wtx's PartitionedFilledBuffer bug where nb.following() returns all
    // zero-initialized capacity bytes, causing httparse to hit null bytes in headers)
    const MAX_HTTP_HEADER_LEN: usize = 16 * 1024;
    let mut buf = Vec::with_capacity(4096);
    let header_end = loop {
        let mut tmp = [0u8; 1024];
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| eyre!("read during H1 upgrade on {addr}: {e}"))?;
        if n == 0 {
            return Err(eyre!("not a websocket upgrade: connection closed during headers"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_HTTP_HEADER_LEN {
            return Err(eyre!("not a websocket upgrade: headers too large on {addr}"));
        }
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let mut hbuf = [httparse::EMPTY_HEADER; 64];
    let mut req = HttpRequest::new(&mut hbuf);
    match req.parse(&buf[..header_end]) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => {
            return Err(eyre!("not a websocket upgrade: incomplete HTTP request on {addr}"));
        }
        Err(e) => {
            return Err(eyre!("not a websocket upgrade: HTTP parse error on {addr}: {e}"));
        }
    }

    if !req.method.map(|m| m.eq_ignore_ascii_case("GET")).unwrap_or(false) {
        return Err(eyre!("not a websocket upgrade: expected GET on {addr}"));
    }

    let mut ws_key: Option<String> = None;
    let mut protocol = String::new();
    let mut has_upgrade = false;
    let mut has_connection_upgrade = false;
    let mut ws_version_ok = false;

    for h in req.headers.iter() {
        let value = core::str::from_utf8(h.value).unwrap_or("").trim();
        debug!(ws_server = true, ?addr, header_name = %h.name, header_value = %value, "H1: parsed header");
        if h.name.eq_ignore_ascii_case("Upgrade") {
            has_upgrade = value.eq_ignore_ascii_case("websocket");
        } else if h.name.eq_ignore_ascii_case("Connection") {
            has_connection_upgrade =
                value.split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade"));
        } else if h.name.eq_ignore_ascii_case("Sec-WebSocket-Version") {
            ws_version_ok = value == "13";
        } else if h.name.eq_ignore_ascii_case("Sec-WebSocket-Key") {
            ws_key = Some(value.to_string());
        } else if h.name.eq_ignore_ascii_case("Sec-WebSocket-Protocol") {
            protocol = value.trim().to_string();
        }
    }

    if !has_upgrade || !has_connection_upgrade || !ws_version_ok {
        return Err(eyre!("not a websocket upgrade: missing upgrade headers on {addr}"));
    }
    let key =
        ws_key.ok_or_else(|| eyre!("not a websocket upgrade: missing Sec-WebSocket-Key on {addr}"))?;

    // RFC 6455 §4.2.2: accept key = base64(SHA1(client_key + WS_GUID))
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let accept = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());

    let selected_protocol = protocol.split(',').next().unwrap_or("").trim().to_string();
    let protocol_line = if !selected_protocol.is_empty() {
        format!("Sec-WebSocket-Protocol: {selected_protocol}\r\n")
    } else {
        String::new()
    };
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n{protocol_line}\r\n"
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| eyre!("write 101 on {addr}: {e}"))?;

    debug!(ws_server = true, ?addr, response = %response.lines().next().unwrap_or(""), "H1: sent 101 response");

    // Re-queue any bytes received after headers (shouldn't happen for WS upgrades)
    if buf.len() > header_end {
        stream.prepend_bytes(buf[header_end..].to_vec());
    }

    debug!(ws_server = true, ?addr, protocol = %protocol, "H1 WTX upgrade successful");

    debug!(ws_server = true, ?addr, protocol = %protocol, "H1: returning protocol to upgrader");

    // Construct WebSocket framer directly — handshake already done above
    let wsb = WebSocketBuffer::new();
    let rng = Xorshift64::new(wtx::rng::simple_seed());
    let mut ws = WebSocket::new((), false, rng, stream, wsb);
    ws.set_max_payload_len(MAX_FRAME_SIZE);

    Ok((
        Box::new(GenericWsStream { ws, read_buffer: Vector::new() }) as Box<dyn WsStream + 'static>,
        protocol,
    ))
}

// -- H2 path --

async fn upgrade_h2(
    reader: tokio::io::ReadHalf<BoxedStream>,
    writer: tokio::io::WriteHalf<BoxedStream>,
    addr: SocketAddr,
) -> Result<(Box<dyn WsStream>, String)> {
    let hp = HttpRecvParams::with_optioned_params().set_enable_connect_protocol(true);
    let hb = Http2Buffer::default();

    let (frame_reader, http2) = Http2::accept(hb, hp, (reader, writer))
        .await
        .map_err(|e| eyre!("H2 accept failed: {e}"))?;

    tokio::task::spawn_local(frame_reader);

    let (mut server_stream, protocol) = http2
        .stream(|req, _protocol| {
            let proto = req.rrd.headers.get_by_name(b"sec-websocket-protocol")
                .map(|h| h.value.to_string())
                .unwrap_or_default();
            proto
        })
        .await
        .map_err(|e| eyre!("H2 stream error: {e}"))?
        .ok_or_else(|| eyre!("H2 connection closed before any stream"))?;

    if server_stream.protocol() != Some(Protocol::WebSocket) {
        return Err(eyre!("not a websocket upgrade: H2 stream is not a WS upgrade"));
    }

    // recv_req() is NOT called here — http2.stream() already delivered the
    // CONNECT HEADERS frame; calling recv_req() again causes UnknownStreamId.

    let headers = Headers::new();
    let ws = WebSocketOverStream::new(
        &headers,
        false,
        Xorshift64::new(wtx::rng::simple_seed()),
        server_stream,
    )
    .await
    .map_err(|e| eyre!("WebSocketOverStream::new failed: {e}"))?;

    Ok((
        Box::new(H2WsStream {
            ws,
            read_buffer: Vector::new(),
            _http2: http2,
        }) as Box<dyn WsStream + 'static>,
        protocol,
    ))
}

// -- Generic WsStream wrapper (works with any wtx Stream type) --

struct GenericWsStream<S: Stream> {
    ws: WebSocket<(), Xorshift64, S, WebSocketBuffer, false>,
    read_buffer: Vector<u8>,
}

#[async_trait(?Send)]
impl<S: Stream + Unpin + Send + 'static> WsStream for GenericWsStream<S> {
    async fn send(&mut self, msg: Message) -> Result<(), StreamError> {
        #[allow(unreachable_patterns)]
        let (op_code, payload) = message_to_payload(msg);
        let payload = Vector::from(payload);
        let mut frame = Frame::new_fin(op_code, payload);
        self.ws.write_frame(&mut frame).await.map_err(map_wtx_err)
    }

    async fn recv(&mut self) -> Option<Result<Message, StreamError>> {
        self.read_buffer.clear();
        let frame = match self
            .ws
            .read_frame(&mut self.read_buffer, WebSocketPayloadOrigin::Adaptive)
            .await
        {
            Ok(f) => f,
            Err(e) => return Some(Err(map_wtx_err(e))),
        };
        Some(Ok(payload_to_message(frame.op_code(), frame.payload().to_vec())?))
    }
}

// -- H2 WsStream wrapper --

type H2ServerStream = wtx::http2::ServerStream<Http2Buffer, tokio::io::WriteHalf<BoxedStream>>;

struct H2WsStream {
    ws: WebSocketOverStream<H2ServerStream>,
    read_buffer: Vector<u8>,
    _http2: Http2<Http2Buffer, tokio::io::WriteHalf<BoxedStream>, false>,
}

#[async_trait(?Send)]
impl WsStream for H2WsStream {
    async fn send(&mut self, msg: Message) -> Result<(), StreamError> {
        #[allow(unreachable_patterns)]
        let (op_code, payload) = message_to_payload(msg);
        let payload = Vector::from(payload);
        let mut frame = Frame::new_fin(op_code, payload);
        self.ws.write_frame(&mut frame).await.map_err(map_wtx_err)
    }

    async fn recv(&mut self) -> Option<Result<Message, StreamError>> {
        self.read_buffer.clear();
        let frame = match self.ws.read_frame(&mut self.read_buffer).await {
            Ok(f) => f,
            Err(e) => return Some(Err(map_wtx_err(e))),
        };
        Some(Ok(payload_to_message(frame.op_code(), frame.payload().to_vec())?))
    }
}

// -- Shared message conversion helpers --

fn message_to_payload(msg: Message) -> (OpCode, Vec<u8>) {
    match msg {
        Message::Text(t) => (OpCode::Text, t.as_str().as_bytes().to_vec()),
        Message::Binary(b) => (OpCode::Binary, b.to_vec()),
        Message::Ping(d) => (OpCode::Ping, d.to_vec()),
        Message::Pong(d) => (OpCode::Pong, d.to_vec()),
        Message::Close(_) => (OpCode::Close, Vec::new()),
    }
}

fn payload_to_message(op_code: OpCode, payload: Vec<u8>) -> Option<Message> {
    match op_code {
        OpCode::Text => Some(Message::Text(String::from_utf8(payload).ok()?.into())),
        OpCode::Binary => Some(Message::Binary(payload.into())),
        OpCode::Ping => Some(Message::Ping(payload.into())),
        OpCode::Pong => Some(Message::Pong(payload.into())),
        OpCode::Close => Some(Message::Close(None)),
        _ => None,
    }
}

// -- Error mapping --

fn map_wtx_err(e: wtx::Error) -> StreamError {
    match &e {
        wtx::Error::ClosedWebSocketConnection
        | wtx::Error::UnexpectedStreamReadEOF
        | wtx::Error::ClosedConnection
        | wtx::Error::ClosedHttpConnection => StreamError::Closed,
        wtx::Error::WebSocketError(_) => StreamError::Protocol(e.to_string()),
        _ => StreamError::Other(eyre!(e)),
    }
}
