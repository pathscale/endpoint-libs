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
//! The default maximum frame length is 16 MiB; see
//! [`framed_json_neutral_with_max_frame`].
//!
//! **This format is normative for any non-Rust peer.** It is deliberately trivial to
//! implement: a 4-byte length prefix, one tag byte, and a payload. It is also recorded
//! in the AsyncAPI document emitted in 2.1, which is the machine-readable copy.
//!
//! # There is one framing path, and it names no runtime
//!
//! The byte stream is [`futures::io::AsyncRead`]/[`AsyncWrite`], which is the trait
//! pair that belongs to no executor. The length prefix is read and written by hand,
//! a few dozen lines below, rather than by `tokio_util::codec::LengthDelimitedCodec`.
//! That is not a reimplementation for its own sake: a codec from `tokio-util` puts
//! `tokio` in the dependency graph of every consumer of this module, and this crate
//! is required to have a graph with no tokio in it at all. `tokio_serde` was never
//! used either — the kind byte means the payload is not a bare serde value, so a
//! serde codec layer would buy nothing.
//!
//! ## Breaking change in 3.2.0: `framed_json` is gone
//!
//! Until 3.2.0 this module also exposed `framed_json`, the same wire format over
//! `tokio::io::{AsyncRead, AsyncWrite}`, behind a `framed-transport-tokio` feature.
//! Both flavours shared `encode`/`decode`, so they only ever differed in the adapter
//! type they accepted; the tokio one has been deleted along with the feature.
//!
//! A consumer holding a tokio `AsyncRead + AsyncWrite` (a `tokio::net::UnixStream`,
//! a `tokio::io::DuplexStream`) bridges on its own side, in one call:
//!
//! ```ignore
//! use tokio_util::compat::TokioAsyncReadCompatExt;
//!
//! let transport = framed_json_neutral(tokio_stream.compat());
//! ```
//!
//! `tokio-util`'s `compat` feature is then **the consumer's** dependency, declared in
//! the consumer's `Cargo.toml`. That is the whole point of the removal: the crate that
//! wants tokio is the crate that pays for it, and everyone else gets a graph without
//! it. The bytes on the wire do not change, so a bridged peer and an unbridged peer
//! still talk to each other.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::io::{AsyncRead, AsyncWrite};
use futures::{Sink, Stream};

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

/// Wrap a `futures-io` byte stream in the framing described in this module's docs.
///
/// The result is a [`Transport`] of [`WireMessage`], which
/// [`TransportStream`](super::TransportStream) turns into a
/// [`MessageStream`](super::super::traits::MessageStream) for the session loop.
///
/// [`futures::io::AsyncRead`]/[`AsyncWrite`] is the neutral trait pair: Nagoya reaches
/// it through [`NagoyaStream`](super::nagoya::NagoyaStream) behind the
/// `nagoya-transport` feature, and a caller still holding a tokio stream reaches it
/// through `tokio-util`'s `compat` on its own side — see the module docs. Neither
/// route is named here, which is what keeps this path free of a runtime.
pub fn framed_json_neutral<S>(
    io: S,
) -> impl Transport<WireMessage, WireMessage, TransportError = FramedError> + Unpin + Send
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    framed_json_neutral_with_max_frame(io, DEFAULT_MAX_FRAME_BYTES)
}

/// [`framed_json_neutral`] with an explicit maximum frame length.
///
/// Frames longer than `max_frame_bytes` are rejected rather than buffered, which is
/// what keeps a hostile or broken peer from exhausting memory.
pub fn framed_json_neutral_with_max_frame<S>(
    io: S,
    max_frame_bytes: usize,
) -> impl Transport<WireMessage, WireMessage, TransportError = FramedError> + Unpin + Send
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    NeutralFramed {
        io,
        max_frame_bytes,
        read_buf: BytesMut::new(),
        write_buf: BytesMut::new(),
        read_eof: false,
    }
}

/// The framing itself: a `futures-io` byte stream in, [`WireMessage`]s out.
///
/// `tokio_util`'s `Framed` is the thing this would have been built on if a tokio
/// dependency were acceptable; it is not, and `futures-util` has no equivalent, so
/// the buffering is here. It is the same two buffers `Framed` keeps: one accumulating
/// what has been read but not yet framed, one holding what has been encoded but not
/// yet written.
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
    S: AsyncRead + AsyncWrite + Unpin,
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
    S: AsyncRead + AsyncWrite + Unpin,
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

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::task::Waker;

    use futures::executor::block_on;
    use futures::{AsyncReadExt, AsyncWriteExt, SinkExt, StreamExt};

    /// One direction of [`duplex`]: bytes in at one end, out at the other.
    ///
    /// The async cases used to run over `tokio::io::duplex`, bridged into `futures-io`
    /// with `tokio-util`'s `compat`. Both are gone with the tokio flavour, and a test
    /// pipe is not worth a dependency that would put a runtime back in the graph — so
    /// it is thirty lines here instead. Unbounded on purpose: a writer never blocks,
    /// which is what lets a test send and then read back on one thread under
    /// `block_on` without a second task to drain it.
    #[derive(Default)]
    struct Pipe {
        bytes: VecDeque<u8>,
        writer_gone: bool,
        reader_waker: Option<Waker>,
    }

    /// One end of an in-memory bidirectional byte stream.
    struct DuplexEnd {
        incoming: Arc<Mutex<Pipe>>,
        outgoing: Arc<Mutex<Pipe>>,
    }

    /// A connected pair. Whatever one end writes, the other end reads.
    fn duplex() -> (DuplexEnd, DuplexEnd) {
        let left_to_right = Arc::new(Mutex::new(Pipe::default()));
        let right_to_left = Arc::new(Mutex::new(Pipe::default()));
        (
            DuplexEnd {
                incoming: Arc::clone(&right_to_left),
                outgoing: Arc::clone(&left_to_right),
            },
            DuplexEnd {
                incoming: left_to_right,
                outgoing: right_to_left,
            },
        )
    }

    impl DuplexEnd {
        /// Mark the write side finished, so the peer's reader sees EOF rather than
        /// waiting forever. Both `poll_close` and the drop go through here: dropping
        /// a socket closes it, and a test that hangs up without closing first is
        /// exactly the truncation case worth exercising.
        fn hang_up(&self) {
            let mut pipe = self.outgoing.lock().expect("pipe lock");
            pipe.writer_gone = true;
            if let Some(waker) = pipe.reader_waker.take() {
                waker.wake();
            }
        }
    }

    impl Drop for DuplexEnd {
        fn drop(&mut self) {
            self.hang_up();
        }
    }

    impl AsyncRead for DuplexEnd {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let mut pipe = self.incoming.lock().expect("pipe lock");
            if pipe.bytes.is_empty() {
                return if pipe.writer_gone {
                    Poll::Ready(Ok(0))
                } else {
                    pipe.reader_waker = Some(cx.waker().clone());
                    Poll::Pending
                };
            }
            let take = pipe.bytes.len().min(buffer.len());
            for slot in buffer.iter_mut().take(take) {
                *slot = pipe.bytes.pop_front().expect("checked length");
            }
            Poll::Ready(Ok(take))
        }
    }

    impl AsyncWrite for DuplexEnd {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut pipe = self.outgoing.lock().expect("pipe lock");
            pipe.bytes.extend(buffer.iter().copied());
            if let Some(waker) = pipe.reader_waker.take() {
                waker.wake();
            }
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.hang_up();
            Poll::Ready(Ok(()))
        }
    }

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

    /// The length prefix belongs to the sink, not to `encode`, so the byte-level
    /// check above does not cover it. It used to be covered indirectly, by comparing
    /// this path's output against `tokio_util`'s `LengthDelimitedCodec`. With the
    /// tokio flavour deleted there is no second implementation to agree with, and
    /// the format is still normative for non-Rust peers — so assert the whole frame
    /// against the ASCII art in the module docs directly.
    #[test]
    fn the_sink_writes_the_length_prefix_the_docs_promise() {
        let (sender, mut receiver) = duplex();

        block_on(async {
            let mut framed = framed_json_neutral(sender);
            framed
                .send(WireMessage::Text("hi".into()))
                .await
                .expect("send");
            framed.flush().await.expect("flush");

            // Three bytes of body: the kind byte plus "hi".
            let mut got = [0u8; 7];
            receiver.read_exact(&mut got).await.expect("read");
            assert_eq!(got, [0, 0, 0, 3, KIND_TEXT, b'h', b'i']);
        });
    }

    #[test]
    fn a_duplex_pipe_carries_messages_both_ways() {
        let (client, server) = duplex();

        block_on(async {
            let mut client = framed_json_neutral(client);
            let mut server = framed_json_neutral(server);

            let sent = WireMessage::Text("ping".into());
            client.send(sent.clone()).await.expect("send");
            let got = server.next().await.expect("a frame").expect("decode");
            assert_eq!(sent, got);

            let back = WireMessage::Binary(vec![7, 7, 7].into());
            server.send(back.clone()).await.expect("send back");
            let got = client.next().await.expect("a frame").expect("decode");
            assert_eq!(back, got);
        });
    }

    /// The limit is enforced on the way out too, so we never emit a frame a
    /// conforming peer would have to reject.
    #[test]
    fn oversized_outbound_frames_are_refused_by_the_encoder() {
        let (client, _server) = duplex();

        block_on(async {
            let mut framed = framed_json_neutral_with_max_frame(client, 64);
            let result = framed
                .send(WireMessage::Binary(vec![0u8; 4096].into()))
                .await;
            assert!(
                matches!(result, Err(FramedError::Io(_))),
                "expected the encoder to refuse an oversized frame, got {result:?}"
            );
        });
    }

    /// A length prefix naming more than the maximum is refused before the body is
    /// buffered. Believing it is how a peer asks for an allocation it never sends.
    #[test]
    fn an_oversized_length_prefix_is_rejected_rather_than_buffered() {
        let (mut writer, reader) = duplex();

        block_on(async {
            // Written by hand: a hostile peer is not using our encoder, so this is
            // the case that actually protects memory.
            writer
                .write_all(&u32::MAX.to_be_bytes())
                .await
                .expect("write prefix");

            let mut framed = framed_json_neutral_with_max_frame(reader, 64);
            let err = framed
                .next()
                .await
                .expect("a result")
                .expect_err("must refuse");
            assert!(
                matches!(err, FramedError::Io(_)),
                "expected an io error, got {err:?}"
            );
        });
    }

    /// A stream that ends mid-frame is truncation, not a clean close. A clean close
    /// lands on a frame boundary with nothing buffered.
    #[test]
    fn a_truncated_frame_is_reported() {
        let (mut writer, reader) = duplex();

        block_on(async {
            // Promise ten bytes, send three, then hang up.
            writer
                .write_all(&10u32.to_be_bytes())
                .await
                .expect("write prefix");
            writer
                .write_all(&[KIND_TEXT, b'h', b'i'])
                .await
                .expect("write partial body");
            drop(writer);

            let mut framed = framed_json_neutral(reader);
            let err = framed
                .next()
                .await
                .expect("a result")
                .expect_err("must refuse");
            assert!(
                matches!(err, FramedError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof),
                "expected UnexpectedEof, got {err:?}"
            );
        });
    }

    /// A clean close after a whole frame ends the stream rather than erroring.
    #[test]
    fn a_stream_ending_on_a_frame_boundary_ends_cleanly() {
        let (writer, reader) = duplex();

        block_on(async {
            let mut sender = framed_json_neutral(writer);
            let mut framed = framed_json_neutral(reader);

            sender
                .send(WireMessage::Text("only".into()))
                .await
                .expect("send");
            sender.close().await.expect("close");

            let got = framed.next().await.expect("a frame").expect("decode");
            assert_eq!(WireMessage::Text("only".into()), got);
            assert!(framed.next().await.is_none(), "expected a clean end");
        });
    }
}
