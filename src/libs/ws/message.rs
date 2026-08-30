//! The canonical, backend-independent WebSocket message type.
//!
//! # 3.0 payload ownership
//!
//! Payloads are immutable, reference-counted bytes. A framed local transport and
//! tungstenite can therefore transfer their receive buffer into [`WireMessage`]
//! without allocating and copying a `String` or `Vec<u8>`. [`Utf8Bytes`] preserves
//! the validity guarantee previously provided by `String`.
//!
//! # 2.0 backend independence
//!
//! Before 2.0, `WsMessage` was a *re-export of `tungstenite::Message`* whenever the
//! `ws` feature was on, which leaked a backend type through the session, toolbox,
//! subscription, push and connection layers. [`WireMessage`] is now the canonical type
//! in every configuration; the tungstenite (and any future) backend converts at its own
//! edge via the `From` impls in that backend's module.
//!
//! `pub type WsMessage = WireMessage` remains as a compatibility alias, but note it
//! only covers *type positions*. Code that called tungstenite's inherent methods
//! (`.into_text()`, `.into_data()`, `.is_close()`, …) must migrate — see
//! `docs/2.0-migration.md`.

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;

use bytes::Bytes;

/// An immutable, cheaply cloned UTF-8 payload.
///
/// This keeps the text invariant of `String` without copying buffers received
/// from tungstenite or a framed byte stream.
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Utf8Bytes(Bytes);

impl Utf8Bytes {
    pub const fn from_static(value: &'static str) -> Self {
        Self(Bytes::from_static(value.as_bytes()))
    }

    pub fn as_str(&self) -> &str {
        // The only unchecked constructor is crate-private and is used when
        // converting from another type with the same UTF-8 invariant.
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    #[cfg(any(feature = "ws", feature = "ws-client"))]
    pub(crate) unsafe fn from_bytes_unchecked(value: Bytes) -> Self {
        Self(value)
    }
}

impl Deref for Utf8Bytes {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for Utf8Bytes {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<[u8]> for Utf8Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<Bytes> for Utf8Bytes {
    fn as_ref(&self) -> &Bytes {
        &self.0
    }
}

impl Borrow<str> for Utf8Bytes {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Utf8Bytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<Bytes> for Utf8Bytes {
    type Error = std::str::Utf8Error;

    fn try_from(value: Bytes) -> Result<Self, Self::Error> {
        std::str::from_utf8(&value)?;
        Ok(Self(value))
    }
}

impl TryFrom<Vec<u8>> for Utf8Bytes {
    type Error = std::str::Utf8Error;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        Bytes::from(value).try_into()
    }
}

impl From<String> for Utf8Bytes {
    fn from(value: String) -> Self {
        Self(Bytes::from(value))
    }
}

impl From<&str> for Utf8Bytes {
    fn from(value: &str) -> Self {
        Self(Bytes::copy_from_slice(value.as_bytes()))
    }
}

impl From<&String> for Utf8Bytes {
    fn from(value: &String) -> Self {
        value.as_str().into()
    }
}

impl From<Utf8Bytes> for Bytes {
    fn from(value: Utf8Bytes) -> Self {
        value.0
    }
}

/// A WebSocket close frame: status code plus a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    pub code: u16,
    pub reason: Utf8Bytes,
}

/// A protocol-level message, independent of any WebSocket backend.
///
/// This is the item type carried by [`MessageStream`](crate::libs::ws::WsStream) and,
/// from 2.0 on, by any [`Transport`](crate::libs::ws::Transport) — including non-WS
/// local transports (Unix sockets, named pipes, XPC), where `Ping`/`Pong`/`Close` are
/// mapped onto whatever that transport's control mechanism is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WireMessage {
    Text(Utf8Bytes),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close(Option<CloseFrame>),
}

/// Compatibility alias for the pre-2.0 name.
///
/// Covers type positions only — not tungstenite's inherent methods.
pub type WsMessage = WireMessage;

impl From<String> for WireMessage {
    fn from(s: String) -> Self {
        Self::Text(s.into())
    }
}

impl From<&str> for WireMessage {
    fn from(s: &str) -> Self {
        Self::Text(s.into())
    }
}

impl From<Vec<u8>> for WireMessage {
    fn from(value: Vec<u8>) -> Self {
        Self::Binary(value.into())
    }
}

impl WireMessage {
    pub fn text(value: impl Into<Utf8Bytes>) -> Self {
        Self::Text(value.into())
    }

    pub fn binary(value: impl Into<Bytes>) -> Self {
        Self::Binary(value.into())
    }

    pub fn ping(value: impl Into<Bytes>) -> Self {
        Self::Ping(value.into())
    }

    pub fn pong(value: impl Into<Bytes>) -> Self {
        Self::Pong(value.into())
    }

    /// Borrow the payload as text, if this message carries UTF-8.
    ///
    /// `Text` always succeeds; `Binary` succeeds when the bytes are valid UTF-8
    /// (the legacy protocol and MCP both accept either framing). Control frames
    /// return `None`.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(t.as_str()),
            Self::Binary(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    /// Borrow the application payload without copying it.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_ref(),
            Self::Binary(data) | Self::Ping(data) | Self::Pong(data) => data,
            Self::Close(None) => &[],
            Self::Close(Some(frame)) => frame.reason.as_ref(),
        }
    }

    /// Consume the message and return its application payload.
    pub fn into_data(self) -> Bytes {
        match self {
            Self::Text(text) => text.into_bytes(),
            Self::Binary(data) | Self::Ping(data) | Self::Pong(data) => data,
            Self::Close(None) => Bytes::new(),
            Self::Close(Some(frame)) => frame.reason.into_bytes(),
        }
    }

    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True for `Close`.
    pub fn is_close(&self) -> bool {
        matches!(self, Self::Close(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_inputs_transfer_their_allocations() {
        let text = String::from("owned text payload");
        let text_pointer = text.as_ptr();
        let WireMessage::Text(text) = WireMessage::text(text) else {
            unreachable!()
        };
        assert_eq!(text.as_bytes().as_ptr(), text_pointer);

        let binary = vec![1, 2, 3, 4, 5];
        let binary_pointer = binary.as_ptr();
        let WireMessage::Binary(binary) = WireMessage::binary(binary) else {
            unreachable!()
        };
        assert_eq!(binary.as_ptr(), binary_pointer);
    }

    #[test]
    fn bytes_are_validated_before_becoming_text() {
        assert!(Utf8Bytes::try_from(Bytes::from_static(b"valid text")).is_ok());
        assert!(Utf8Bytes::try_from(Bytes::from_static(b"invalid \xff")).is_err());
    }
}
