//! Sans-IO codec for the length-delimited frame, on `core` and `alloc` only.
//!
//! This is the byte format documented in `ws::transport::framed`, without the futures
//! machinery around it: a `u32` big-endian length that counts the kind byte plus the
//! payload, one kind byte (`0` Text, `1` Binary, `2` Ping, `3` Pong, `4` Close), then
//! the payload. Nothing here reads or writes a socket; a caller feeds bytes in with
//! [`FrameDecoder::push`] and takes whole frames out with [`FrameDecoder::next_frame`],
//! and writes what [`encode_frame`] appends. That is what lets a crate with its own
//! blocking socket layer (and no runtime, no `std`, no proc macro, no build script)
//! speak the same wire as `framed_json_neutral`.
//!
//! Behaviour matches the futures flavour on purpose: an oversized length is refused
//! before the body is buffered, an unknown kind is an error, a Text payload must be
//! UTF-8, a Close payload is empty or `u16` code plus UTF-8 reason, and a stream that
//! ends inside a frame is an error ([`FrameDecoder::finish`]).

use alloc::vec::Vec;

/// Default maximum frame length (kind byte plus payload): 16 MiB.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// The length prefix: `u32` big-endian.
pub const LENGTH_PREFIX_BYTES: usize = 4;

pub const KIND_TEXT: u8 = 0;
pub const KIND_BINARY: u8 = 1;
pub const KIND_PING: u8 = 2;
pub const KIND_PONG: u8 = 3;
pub const KIND_CLOSE: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The declared length exceeds the decoder's maximum (or does not fit `u32` when encoding).
    TooLarge { length: usize, max: usize },
    /// A frame with length 0: not even a kind byte.
    EmptyFrame,
    UnknownKind(u8),
    /// A Text frame (or a Close reason) was not UTF-8.
    InvalidUtf8,
    /// A Close payload of exactly one byte.
    MalformedClose,
    /// The stream ended in the middle of a frame.
    Truncated,
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooLarge { length, max } => write!(f, "frame of {length} bytes exceeds the {max} byte maximum"),
            Self::EmptyFrame => f.write_str("empty frame"),
            Self::UnknownKind(kind) => write!(f, "unknown frame kind: {kind}"),
            Self::InvalidUtf8 => f.write_str("text frame was not valid UTF-8"),
            Self::MalformedClose => f.write_str("malformed close frame"),
            Self::Truncated => f.write_str("the stream ended in the middle of a frame"),
        }
    }
}

/// One decoded frame. Payloads are owned bytes; a Text payload has been checked as UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<(u16, Vec<u8>)>),
}

impl Frame {
    /// The text of a Text frame (already validated).
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(bytes) => core::str::from_utf8(bytes).ok(),
            _ => None,
        }
    }
}

/// Append one frame (`kind` and `payload`) to `out`.
pub fn encode_frame(kind: u8, payload: &[u8], out: &mut Vec<u8>) -> Result<(), FrameError> {
    let length = payload.len() + 1;
    let prefix = u32::try_from(length).map_err(|_| FrameError::TooLarge { length, max: u32::MAX as usize })?;
    out.reserve(LENGTH_PREFIX_BYTES + length);
    out.extend_from_slice(&prefix.to_be_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    Ok(())
}

/// Append a Text frame.
pub fn encode_text(text: &str, out: &mut Vec<u8>) -> Result<(), FrameError> {
    encode_frame(KIND_TEXT, text.as_bytes(), out)
}

/// Append a Close frame; `None` is the empty-payload form.
pub fn encode_close(close: Option<(u16, &str)>, out: &mut Vec<u8>) -> Result<(), FrameError> {
    match close {
        None => encode_frame(KIND_CLOSE, &[], out),
        Some((code, reason)) => {
            let mut payload = Vec::with_capacity(2 + reason.len());
            payload.extend_from_slice(&code.to_be_bytes());
            payload.extend_from_slice(reason.as_bytes());
            encode_frame(KIND_CLOSE, &payload, out)
        }
    }
}

/// Accumulates bytes and yields whole frames.
#[derive(Debug, Clone)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    max_frame_bytes: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::with_max_frame(DEFAULT_MAX_FRAME_BYTES)
    }

    pub fn with_max_frame(max_frame_bytes: usize) -> Self {
        Self { buffer: Vec::new(), max_frame_bytes }
    }

    /// Bytes buffered and not yet returned as a frame.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next whole frame, `Ok(None)` when more bytes are needed. After an `Err` the
    /// stream is not resynchronised; the caller drops the connection.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if self.buffer.len() < LENGTH_PREFIX_BYTES {
            return Ok(None);
        }
        let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
        prefix.copy_from_slice(&self.buffer[..LENGTH_PREFIX_BYTES]);
        let length = u32::from_be_bytes(prefix) as usize;
        if length > self.max_frame_bytes {
            return Err(FrameError::TooLarge { length, max: self.max_frame_bytes });
        }
        if self.buffer.len() < LENGTH_PREFIX_BYTES + length {
            return Ok(None);
        }
        let body: Vec<u8> = self.buffer[LENGTH_PREFIX_BYTES..LENGTH_PREFIX_BYTES + length].to_vec();
        self.buffer.drain(..LENGTH_PREFIX_BYTES + length);
        decode(body).map(Some)
    }

    /// Call at end of stream: `Ok` on a frame boundary, `Truncated` when bytes are left over.
    pub fn finish(&self) -> Result<(), FrameError> {
        if self.buffer.is_empty() { Ok(()) } else { Err(FrameError::Truncated) }
    }
}

fn decode(mut body: Vec<u8>) -> Result<Frame, FrameError> {
    if body.is_empty() {
        return Err(FrameError::EmptyFrame);
    }
    let kind = body.remove(0);
    Ok(match kind {
        KIND_TEXT => {
            core::str::from_utf8(&body).map_err(|_| FrameError::InvalidUtf8)?;
            Frame::Text(body)
        }
        KIND_BINARY => Frame::Binary(body),
        KIND_PING => Frame::Ping(body),
        KIND_PONG => Frame::Pong(body),
        KIND_CLOSE => match body.len() {
            0 => Frame::Close(None),
            1 => return Err(FrameError::MalformedClose),
            _ => {
                let code = u16::from_be_bytes([body[0], body[1]]);
                let reason = body.split_off(2);
                core::str::from_utf8(&reason).map_err(|_| FrameError::MalformedClose)?;
                Frame::Close(Some((code, reason)))
            }
        },
        other => return Err(FrameError::UnknownKind(other)),
    })
}
