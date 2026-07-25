//! The canonical, backend-independent WebSocket message type.
//!
//! # 2.0 change
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

/// A WebSocket close frame: status code plus a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    pub code: u16,
    pub reason: String,
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
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<CloseFrame>),
}

/// Compatibility alias for the pre-2.0 name.
///
/// Covers type positions only — not tungstenite's inherent methods.
pub type WsMessage = WireMessage;

impl From<String> for WireMessage {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<&str> for WireMessage {
    fn from(s: &str) -> Self {
        Self::Text(s.to_owned())
    }
}

impl WireMessage {
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

    /// True for `Close`.
    pub fn is_close(&self) -> bool {
        matches!(self, Self::Close(_))
    }
}
