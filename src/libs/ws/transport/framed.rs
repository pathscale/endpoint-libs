//! Length-delimited framing of [`WireMessage`] over any byte stream.
//!
//! This is what carries the ordinary JSON protocol over a Unix socket, a Windows
//! named pipe, or an inherited socketpair — anywhere there is no WebSocket to provide
//! message boundaries.
//!
//! # Wire format
//!
//! Each message is one length-delimited frame:
//!
//! ```text
//! +------------------+--------+------------------------+
//! | u32 BE length    | u8 kind| payload (length-1 bytes)|
//! +------------------+--------+------------------------+
//! ```
//!
//! * `length` counts the kind byte plus the payload — i.e. the whole rest of the frame.
//! * `kind` is `0 = Text`, `1 = Binary`, `2 = Ping`, `3 = Pong`, `4 = Close`.
//! * `Text` payloads are UTF-8. `Close` payloads are either empty (no close frame) or
//!   `u16 BE code` followed by a UTF-8 reason.
//!
//! The default maximum frame length is 16 MiB; see [`framed_json_with_max_frame`].
//!
//! **This format is normative for any non-Rust peer.** It is deliberately trivial to
//! implement: a 4-byte length prefix, one tag byte, and a payload. It is also recorded
//! in the AsyncAPI document emitted in 2.1, which is the machine-readable copy.
//!
//! Note the implementation uses `tokio_util`'s `LengthDelimitedCodec` for the length
//! prefix but *not* `tokio_serde`: the kind byte means the payload is not a bare serde
//! value, so the serde codec layer would buy nothing.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::io::{AsyncRead as FuturesRead, AsyncWrite as FuturesWrite};
use futures::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use super::super::message::{CloseFrame, WireMessage};
use super::Transport;

/// Default maximum frame length: 16 MiB.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

const KIND_TEXT: u8 = 0;
const KIND_BINARY: u8 = 1;
const KIND_PING: u8 = 2;
const KIND_PONG: u8 = 3;
const KIND_CLOSE: u8 = 4;

/// Errors from the framed transport.
#[derive(Debug)]
pub enum FramedError {
    Io(io::Error),
    /// A frame arrived with a `kind` byte this version does not know.
    UnknownKind(u8),
    /// A frame was empty (not even a kind byte).
    EmptyFrame,
    /// A `Text` frame's payload was not valid UTF-8.
    InvalidUtf8,
    /// A `Close` frame's payload was malformed (1 byte, or a bad reason).
    MalformedClose,
}

impl std::fmt::Display for FramedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::UnknownKind(kind) => write!(f, "unknown frame kind: {kind}"),
            Self::EmptyFrame => f.write_str("empty frame"),
            Self::InvalidUtf8 => f.write_str("text frame was not valid UTF-8"),
            Self::MalformedClose => f.write_str("malformed close frame"),
        }
    }
}

impl std::error::Error for FramedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for FramedError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

fn encode(msg: WireMessage) -> Bytes {
    let payload_len = msg.as_bytes().len();
    let close_code_len = matches!(msg, WireMessage::Close(Some(_))) as usize * 2;
    let mut buf = BytesMut::with_capacity(1 + close_code_len + payload_len);
    match msg {
        WireMessage::Text(text) => {
            buf.put_u8(KIND_TEXT);
            buf.put_slice(text.as_bytes());
        }
        WireMessage::Binary(data) => {
            buf.put_u8(KIND_BINARY);
            buf.put_slice(&data);
        }
        WireMessage::Ping(data) => {
            buf.put_u8(KIND_PING);
            buf.put_slice(&data);
        }
        WireMessage::Pong(data) => {
            buf.put_u8(KIND_PONG);
            buf.put_slice(&data);
        }
        WireMessage::Close(frame) => {
            buf.put_u8(KIND_CLOSE);
            if let Some(frame) = frame {
                buf.put_u16(frame.code);
                buf.put_slice(frame.reason.as_bytes());
            }
        }
    }
    buf.freeze()
}

fn decode(mut frame: BytesMut) -> Result<WireMessage, FramedError> {
    if frame.is_empty() {
        return Err(FramedError::EmptyFrame);
    }
    let kind = frame.get_u8();
    let payload = frame.freeze();
    Ok(match kind {
        KIND_TEXT => WireMessage::Text(payload.try_into().map_err(|_| FramedError::InvalidUtf8)?),
        KIND_BINARY => WireMessage::Binary(payload),
        KIND_PING => WireMessage::Ping(payload),
        KIND_PONG => WireMessage::Pong(payload),
        KIND_CLOSE => {
            if payload.is_empty() {
                WireMessage::Close(None)
            } else {
                if payload.len() < 2 {
                    return Err(FramedError::MalformedClose);
                }
                let mut payload = payload;
                let code = payload.get_u16();
                let reason = payload
                    .try_into()
                    .map_err(|_| FramedError::MalformedClose)?;
                WireMessage::Close(Some(CloseFrame { code, reason }))
            }
        }
        other => return Err(FramedError::UnknownKind(other)),
    })
}

/// Wrap a byte stream in the framing described in this module's docs.
///
/// The result is a [`Transport`] of [`WireMessage`], which
/// [`TransportStream`](super::TransportStream) turns into a
/// [`MessageStream`](super::super::traits::MessageStream) for the session loop.
pub fn framed_json<S>(
    io: S,
) -> impl Transport<WireMessage, WireMessage, TransportError = FramedError> + Unpin + Send
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    framed_json_with_max_frame(io, DEFAULT_MAX_FRAME_BYTES)
}

/// [`framed_json`] with an explicit maximum frame length.
///
/// Frames longer than `max_frame_bytes` are rejected rather than buffered, which is
/// what keeps a hostile or broken peer from exhausting memory.
pub fn framed_json_with_max_frame<S>(
    io: S,
    max_frame_bytes: usize,
) -> impl Transport<WireMessage, WireMessage, TransportError = FramedError> + Unpin + Send
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let codec = LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_length(4)
        .max_frame_length(max_frame_bytes)
        .new_codec();

    WireFramed {
        inner: Framed::new(io, codec),
    }
}

/// Adapts `Framed<S, LengthDelimitedCodec>` (bytes) to `WireMessage` in both
/// directions.
struct WireFramed<S> {
    inner: Framed<S, LengthDelimitedCodec>,
}

impl<S> Stream for WireFramed<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Item = Result<WireMessage, FramedError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => std::task::Poll::Ready(Some(decode(frame))),
            std::task::Poll::Ready(Some(Err(err))) => {
                std::task::Poll::Ready(Some(Err(FramedError::Io(err))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl<S> Sink<WireMessage> for WireFramed<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Error = FramedError;

    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.inner)
            .poll_ready(cx)
            .map_err(FramedError::Io)
    }

    fn start_send(
        mut self: std::pin::Pin<&mut Self>,
        item: WireMessage,
    ) -> Result<(), Self::Error> {
        std::pin::Pin::new(&mut self.inner)
            .start_send(encode(item))
            .map_err(FramedError::Io)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.inner)
            .poll_flush(cx)
            .map_err(FramedError::Io)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::pin::Pin::new(&mut self.inner)
            .poll_close(cx)
            .map_err(FramedError::Io)
    }
}

/// Length-delimited framing over a `futures-io` byte stream.
///
/// The same wire format as [`framed_json`], carried over
/// [`futures::io::AsyncRead`]/[`AsyncWrite`] rather than tokio's. That trait pair is
/// the neutral one: tokio adapts to it through `tokio-util`'s `Compat`, and Nagoya
/// through [`NagoyaStream`](super::nagoya::NagoyaStream) behind the `nagoya-transport`
/// feature, so a runtime-agnostic caller has a path that does not name a runtime.
///
/// `encode` and `decode` are shared with the tokio path, so the bytes on the wire
/// are identical by construction rather than by agreement.
pub fn framed_json_neutral<S>(
    io: S,
) -> impl Transport<WireMessage, WireMessage, TransportError = FramedError> + Unpin + Send
where
    S: FuturesRead + FuturesWrite + Unpin + Send + 'static,
{
    framed_json_neutral_with_max_frame(io, DEFAULT_MAX_FRAME_BYTES)
}

/// [`framed_json_neutral`] with an explicit maximum frame length.
pub fn framed_json_neutral_with_max_frame<S>(
    io: S,
    max_frame_bytes: usize,
) -> impl Transport<WireMessage, WireMessage, TransportError = FramedError> + Unpin + Send
where
    S: FuturesRead + FuturesWrite + Unpin + Send + 'static,
{
    NeutralFramed {
        io,
        max_frame_bytes,
        read_buf: BytesMut::new(),
        write_buf: BytesMut::new(),
        read_eof: false,
    }
}

/// The `futures-io` counterpart of [`WireFramed`].
///
/// `tokio_util`'s `Framed` is what the tokio path gets for free; there is no
/// equivalent in `futures-util`, so the buffering is here. It is the same two
/// buffers `Framed` keeps: one accumulating what has been read but not yet framed,
/// one holding what has been encoded but not yet written.
struct NeutralFramed<S> {
    io: S,
    max_frame_bytes: usize,
    read_buf: BytesMut,
    write_buf: BytesMut,
    read_eof: bool,
}

/// How much to ask the reader for at once when the buffer needs filling.
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// The length prefix itself: `u32` big-endian.
const LENGTH_PREFIX_BYTES: usize = 4;

impl<S> NeutralFramed<S> {
    /// Take one whole frame out of `read_buf`, if one is there.
    ///
    /// `Ok(None)` means "not yet", which is the ordinary case and not an error.
    /// An oversized length is refused **before** the body is buffered, which is the
    /// property that keeps a hostile peer from naming 4 GiB and being believed.
    fn take_frame(&mut self) -> Result<Option<BytesMut>, FramedError> {
        if self.read_buf.len() < LENGTH_PREFIX_BYTES {
            return Ok(None);
        }

        let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
        prefix.copy_from_slice(&self.read_buf[..LENGTH_PREFIX_BYTES]);
        let frame_len = u32::from_be_bytes(prefix) as usize;

        if frame_len > self.max_frame_bytes {
            return Err(FramedError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "frame of {frame_len} bytes exceeds the {} byte maximum",
                    self.max_frame_bytes
                ),
            )));
        }

        if self.read_buf.len() < LENGTH_PREFIX_BYTES + frame_len {
            return Ok(None);
        }

        self.read_buf.advance(LENGTH_PREFIX_BYTES);
        Ok(Some(self.read_buf.split_to(frame_len)))
    }
}

impl<S> Stream for NeutralFramed<S>
where
    S: FuturesRead + FuturesWrite + Unpin,
{
    type Item = Result<WireMessage, FramedError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            match this.take_frame() {
                Err(err) => return Poll::Ready(Some(Err(err))),
                Ok(Some(frame)) => return Poll::Ready(Some(decode(frame))),
                Ok(None) => {}
            }

            // A frame was still arriving when the stream ended. That is a truncated
            // frame and an error, not a clean close: a clean close lands on a frame
            // boundary with nothing buffered.
            if this.read_eof {
                return if this.read_buf.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Err(FramedError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the stream ended in the middle of a frame",
                    )))))
                };
            }

            let before = this.read_buf.len();
            this.read_buf.resize(before + READ_CHUNK_BYTES, 0);
            let result = Pin::new(&mut this.io).poll_read(cx, &mut this.read_buf[before..]);

            match result {
                Poll::Ready(Ok(0)) => {
                    this.read_buf.truncate(before);
                    this.read_eof = true;
                }
                Poll::Ready(Ok(read)) => this.read_buf.truncate(before + read),
                Poll::Ready(Err(err)) => {
                    this.read_buf.truncate(before);
                    return Poll::Ready(Some(Err(FramedError::Io(err))));
                }
                Poll::Pending => {
                    this.read_buf.truncate(before);
                    return Poll::Pending;
                }
            }
        }
    }
}

impl<S> Sink<WireMessage> for NeutralFramed<S>
where
    S: FuturesRead + FuturesWrite + Unpin,
{
    type Error = FramedError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Bound the outbound buffer the way `Framed` does: once a backlog has built
        // up, make the caller wait for it to drain rather than accepting without
        // limit. Below the threshold this is free.
        let this = self.get_mut();
        if this.write_buf.len() >= this.max_frame_bytes {
            Pin::new(&mut *this).poll_flush(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: WireMessage) -> Result<(), Self::Error> {
        let this = self.get_mut();
        let payload = encode(item);

        if payload.len() > this.max_frame_bytes {
            return Err(FramedError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "frame of {} bytes exceeds the {} byte maximum",
                    payload.len(),
                    this.max_frame_bytes
                ),
            )));
        }

        this.write_buf.reserve(LENGTH_PREFIX_BYTES + payload.len());
        this.write_buf
            .put_u32(u32::try_from(payload.len()).map_err(|_| {
                FramedError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame length does not fit in u32",
                ))
            })?);
        this.write_buf.put_slice(&payload);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.get_mut();

        while !this.write_buf.is_empty() {
            match Pin::new(&mut this.io).poll_write(cx, &this.write_buf) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(FramedError::Io(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "the stream accepted no bytes",
                    ))));
                }
                Poll::Ready(Ok(written)) => this.write_buf.advance(written),
                Poll::Ready(Err(err)) => return Poll::Ready(Err(FramedError::Io(err))),
                Poll::Pending => return Poll::Pending,
            }
        }

        Pin::new(&mut this.io)
            .poll_flush(cx)
            .map_err(FramedError::Io)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        let this = self.get_mut();
        Pin::new(&mut this.io)
            .poll_close(cx)
            .map_err(FramedError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};

    #[test]
    fn every_variant_round_trips_through_the_codec() {
        let cases = vec![
            WireMessage::Text("hello".into()),
            WireMessage::Text(String::new().into()),
            WireMessage::Binary(vec![0, 1, 2, 255].into()),
            WireMessage::Binary(Vec::new().into()),
            WireMessage::Ping(vec![9].into()),
            WireMessage::Pong(Vec::new().into()),
            WireMessage::Close(None),
            WireMessage::Close(Some(CloseFrame {
                code: 1001,
                reason: "going away".into(),
            })),
            WireMessage::Close(Some(CloseFrame {
                code: 1000,
                reason: String::new().into(),
            })),
        ];
        for case in cases {
            let encoded = encode(case.clone());
            let decoded = decode(BytesMut::from(&encoded[..])).expect("decode");
            assert_eq!(case, decoded, "round trip changed the message");
        }
    }

    #[test]
    fn frame_layout_is_what_the_docs_promise() {
        // Non-Rust peers implement against this. Text "hi" => kind 0, then bytes.
        let encoded = encode(WireMessage::Text("hi".into()));
        assert_eq!(&encoded[..], &[KIND_TEXT, b'h', b'i']);

        // Close with code 1000 and no reason => kind 4, then u16 BE.
        let encoded = encode(WireMessage::Close(Some(CloseFrame {
            code: 1000,
            reason: String::new().into(),
        })));
        assert_eq!(&encoded[..], &[KIND_CLOSE, 0x03, 0xE8]);
    }

    #[test]
    fn malformed_frames_are_errors_not_panics() {
        assert!(matches!(
            decode(BytesMut::new()),
            Err(FramedError::EmptyFrame)
        ));
        assert!(matches!(
            decode(BytesMut::from(&[99u8][..])),
            Err(FramedError::UnknownKind(99))
        ));
        assert!(matches!(
            decode(BytesMut::from(&[KIND_TEXT, 0xff, 0xfe][..])),
            Err(FramedError::InvalidUtf8)
        ));
        // A close frame with a single byte cannot hold a u16 code.
        assert!(matches!(
            decode(BytesMut::from(&[KIND_CLOSE, 0x01][..])),
            Err(FramedError::MalformedClose)
        ));
    }

    #[test]
    fn decode_reuses_the_framed_payload_allocation() {
        let frame = BytesMut::from(&[KIND_TEXT, b'h', b'e', b'l', b'l', b'o'][..]);
        let payload_pointer = frame.as_ptr().wrapping_add(1);
        let WireMessage::Text(text) = decode(frame).expect("decode") else {
            panic!("text frame changed kind");
        };

        let text_bytes: &[u8] = text.as_ref();
        assert_eq!(text_bytes, b"hello");
        assert_eq!(text_bytes.as_ptr(), payload_pointer);
    }

    #[tokio::test]
    async fn duplex_pipe_carries_messages_both_ways() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut left = framed_json(a);
        let mut right = framed_json(b);

        left.send(WireMessage::Text("ping".into())).await.unwrap();
        let got = right.next().await.unwrap().unwrap();
        assert_eq!(got, WireMessage::Text("ping".into()));

        right
            .send(WireMessage::Binary(vec![7, 7].into()))
            .await
            .unwrap();
        let got = left.next().await.unwrap().unwrap();
        assert_eq!(got, WireMessage::Binary(vec![7, 7].into()));
    }

    #[tokio::test]
    async fn oversized_outbound_frames_are_refused_by_the_encoder() {
        let (a, _b) = tokio::io::duplex(64 * 1024);
        let mut left = framed_json_with_max_frame(a, 64);

        // The limit is enforced on the way out too, so we never emit a frame a
        // conforming peer would have to reject.
        let result = left.send(WireMessage::Binary(vec![0u8; 4096].into())).await;
        assert!(
            matches!(result, Err(FramedError::Io(_))),
            "expected the encoder to refuse an oversized frame, got {result:?}"
        );
    }

    #[tokio::test]
    async fn oversized_inbound_frames_are_rejected_rather_than_buffered() {
        use tokio::io::AsyncWriteExt;

        let (a, mut b) = tokio::io::duplex(64 * 1024);
        let mut left = framed_json_with_max_frame(a, 64);

        // Write the length prefix by hand — a hostile peer is not using our encoder,
        // so this is the case that actually protects memory.
        b.write_all(&5000u32.to_be_bytes()).await.unwrap();
        b.write_all(&[KIND_BINARY]).await.unwrap();
        b.write_all(&[0u8; 128]).await.unwrap();
        b.flush().await.unwrap();

        let got = left.next().await;
        assert!(
            matches!(got, Some(Err(FramedError::Io(_)))),
            "expected an io error for an oversized declared length, got {got:?}"
        );
    }
    /// The neutral path must put the same bytes on the wire as the tokio path.
    /// Not "equivalent": identical, because the format is normative for non-Rust
    /// peers and there are now two implementations that could drift.
    #[tokio::test]
    async fn the_neutral_path_writes_the_same_bytes_as_the_tokio_path() {
        use tokio_util::compat::TokioAsyncReadCompatExt;

        let cases = vec![
            WireMessage::Text("hello".into()),
            WireMessage::Binary(vec![0, 1, 2, 255].into()),
            WireMessage::Ping(vec![9].into()),
            WireMessage::Close(Some(CloseFrame {
                code: 1000,
                reason: "bye".into(),
            })),
        ];

        for case in cases {
            let (mut tokio_sink, tokio_reader) = tokio::io::duplex(4096);
            let mut tokio_framed = framed_json(tokio_reader);
            tokio_framed.send(case.clone()).await.expect("tokio send");
            let mut tokio_bytes = Vec::new();
            tokio::io::AsyncReadExt::read_buf(&mut tokio_sink, &mut tokio_bytes)
                .await
                .expect("tokio read");

            let (mut neutral_sink, neutral_reader) = tokio::io::duplex(4096);
            let mut neutral_framed = framed_json_neutral(neutral_reader.compat());
            neutral_framed
                .send(case.clone())
                .await
                .expect("neutral send");
            let mut neutral_bytes = Vec::new();
            tokio::io::AsyncReadExt::read_buf(&mut neutral_sink, &mut neutral_bytes)
                .await
                .expect("neutral read");

            assert_eq!(
                tokio_bytes, neutral_bytes,
                "the two framing paths disagree on the wire format for {case:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_neutral_path_carries_messages_both_ways() {
        use tokio_util::compat::TokioAsyncReadCompatExt;

        let (client, server) = tokio::io::duplex(4096);
        let mut client = framed_json_neutral(client.compat());
        let mut server = framed_json_neutral(server.compat());

        let sent = WireMessage::Text("ping".into());
        client.send(sent.clone()).await.expect("send");
        let got = server.next().await.expect("a frame").expect("decode");
        assert_eq!(sent, got);

        let back = WireMessage::Binary(vec![7, 7, 7].into());
        server.send(back.clone()).await.expect("send back");
        let got = client.next().await.expect("a frame").expect("decode");
        assert_eq!(back, got);
    }

    /// A length prefix naming more than the maximum is refused before the body is
    /// buffered. Believing it is how a peer asks for an allocation it never sends.
    #[tokio::test]
    async fn the_neutral_path_rejects_an_oversized_length_prefix() {
        use tokio_util::compat::TokioAsyncReadCompatExt;

        let (mut writer, reader) = tokio::io::duplex(4096);
        let mut framed = framed_json_neutral_with_max_frame(reader.compat(), 64);

        tokio::io::AsyncWriteExt::write_all(&mut writer, &u32::MAX.to_be_bytes())
            .await
            .expect("write prefix");

        let err = framed
            .next()
            .await
            .expect("a result")
            .expect_err("must refuse");
        assert!(
            matches!(err, FramedError::Io(_)),
            "expected an io error, got {err:?}"
        );
    }

    /// A stream that ends mid-frame is truncation, not a clean close. A clean close
    /// lands on a frame boundary with nothing buffered.
    #[tokio::test]
    async fn the_neutral_path_reports_a_truncated_frame() {
        use tokio_util::compat::TokioAsyncReadCompatExt;

        let (mut writer, reader) = tokio::io::duplex(4096);
        let mut framed = framed_json_neutral(reader.compat());

        // Promise ten bytes, send three, then hang up.
        tokio::io::AsyncWriteExt::write_all(&mut writer, &10u32.to_be_bytes())
            .await
            .expect("write prefix");
        tokio::io::AsyncWriteExt::write_all(&mut writer, &[KIND_TEXT, b'h', b'i'])
            .await
            .expect("write partial body");
        drop(writer);

        let err = framed
            .next()
            .await
            .expect("a result")
            .expect_err("must refuse");
        assert!(
            matches!(err, FramedError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof),
            "expected UnexpectedEof, got {err:?}"
        );
    }

    /// A clean close after a whole frame ends the stream rather than erroring.
    #[tokio::test]
    async fn the_neutral_path_ends_cleanly_on_a_frame_boundary() {
        use tokio_util::compat::TokioAsyncReadCompatExt;

        let (writer, reader) = tokio::io::duplex(4096);
        let mut sender = framed_json_neutral(writer.compat());
        let mut framed = framed_json_neutral(reader.compat());

        sender
            .send(WireMessage::Text("only".into()))
            .await
            .expect("send");
        sender.close().await.expect("close");

        let got = framed.next().await.expect("a frame").expect("decode");
        assert_eq!(WireMessage::Text("only".into()), got);
        assert!(framed.next().await.is_none(), "expected a clean end");
    }
}
