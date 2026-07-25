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

use bytes::{Buf, BufMut, Bytes, BytesMut};
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
    let mut buf = BytesMut::new();
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
    let payload = frame;
    Ok(match kind {
        KIND_TEXT => WireMessage::Text(
            String::from_utf8(payload.to_vec()).map_err(|_| FramedError::InvalidUtf8)?,
        ),
        KIND_BINARY => WireMessage::Binary(payload.to_vec()),
        KIND_PING => WireMessage::Ping(payload.to_vec()),
        KIND_PONG => WireMessage::Pong(payload.to_vec()),
        KIND_CLOSE => {
            if payload.is_empty() {
                WireMessage::Close(None)
            } else {
                if payload.len() < 2 {
                    return Err(FramedError::MalformedClose);
                }
                let mut payload = payload;
                let code = payload.get_u16();
                let reason =
                    String::from_utf8(payload.to_vec()).map_err(|_| FramedError::MalformedClose)?;
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

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: WireMessage) -> Result<(), Self::Error> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};

    #[test]
    fn every_variant_round_trips_through_the_codec() {
        let cases = vec![
            WireMessage::Text("hello".into()),
            WireMessage::Text(String::new()),
            WireMessage::Binary(vec![0, 1, 2, 255]),
            WireMessage::Binary(Vec::new()),
            WireMessage::Ping(vec![9]),
            WireMessage::Pong(Vec::new()),
            WireMessage::Close(None),
            WireMessage::Close(Some(CloseFrame {
                code: 1001,
                reason: "going away".into(),
            })),
            WireMessage::Close(Some(CloseFrame {
                code: 1000,
                reason: String::new(),
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
            reason: String::new(),
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

    #[tokio::test]
    async fn duplex_pipe_carries_messages_both_ways() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let mut left = framed_json(a);
        let mut right = framed_json(b);

        left.send(WireMessage::Text("ping".into())).await.unwrap();
        let got = right.next().await.unwrap().unwrap();
        assert_eq!(got, WireMessage::Text("ping".into()));

        right.send(WireMessage::Binary(vec![7, 7])).await.unwrap();
        let got = left.next().await.unwrap().unwrap();
        assert_eq!(got, WireMessage::Binary(vec![7, 7]));
    }

    #[tokio::test]
    async fn oversized_outbound_frames_are_refused_by_the_encoder() {
        let (a, _b) = tokio::io::duplex(64 * 1024);
        let mut left = framed_json_with_max_frame(a, 64);

        // The limit is enforced on the way out too, so we never emit a frame a
        // conforming peer would have to reject.
        let result = left.send(WireMessage::Binary(vec![0u8; 4096])).await;
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
}
