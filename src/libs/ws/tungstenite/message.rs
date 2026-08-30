//! Conversions between the canonical [`WireMessage`] and tungstenite's `Message`.
//!
//! These are the *only* place the tungstenite message type is allowed to meet the rest
//! of the crate. Everything above the backend edge — session, toolbox, subs, push,
//! conn, server, client — deals in [`WireMessage`] exclusively.

use tokio_tungstenite::tungstenite::Message as TMessage;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame as TCloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes as TUtf8Bytes;

use crate::libs::ws::message::{CloseFrame, WireMessage};

impl From<WireMessage> for TMessage {
    fn from(msg: WireMessage) -> Self {
        match msg {
            // SAFETY: endpoint-libs' Utf8Bytes enforces the same invariant as
            // tungstenite's wrapper. Moving the Bytes preserves it.
            WireMessage::Text(t) => {
                Self::Text(unsafe { TUtf8Bytes::from_bytes_unchecked(t.into_bytes()) })
            }
            WireMessage::Binary(b) => Self::Binary(b),
            WireMessage::Ping(b) => Self::Ping(b),
            WireMessage::Pong(b) => Self::Pong(b),
            WireMessage::Close(frame) => Self::Close(frame.map(|f| TCloseFrame {
                code: f.code.into(),
                // SAFETY: both reason types enforce valid UTF-8.
                reason: unsafe { TUtf8Bytes::from_bytes_unchecked(f.reason.into_bytes()) },
            })),
        }
    }
}

impl From<TMessage> for WireMessage {
    fn from(msg: TMessage) -> Self {
        match msg {
            TMessage::Text(t) => {
                // SAFETY: tungstenite validated this payload before exposing
                // it as Utf8Bytes.
                Self::Text(unsafe {
                    super::super::message::Utf8Bytes::from_bytes_unchecked(t.into())
                })
            }
            TMessage::Binary(b) => Self::Binary(b),
            TMessage::Ping(b) => Self::Ping(b),
            TMessage::Pong(b) => Self::Pong(b),
            TMessage::Close(frame) => Self::Close(frame.map(|f| CloseFrame {
                code: f.code.into(),
                // SAFETY: tungstenite's close reason has the same invariant.
                reason: unsafe {
                    super::super::message::Utf8Bytes::from_bytes_unchecked(f.reason.into())
                },
            })),
            // tungstenite's `Frame` variant is only produced by its low-level frame
            // API, which this crate never uses. Map it to an empty binary payload
            // rather than panicking: an unexpected raw frame is not worth aborting a
            // live session over, and the session layer will simply ignore it.
            TMessage::Frame(_) => Self::Binary(bytes::Bytes::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_variant() {
        let cases = vec![
            WireMessage::Text("hello".into()),
            WireMessage::Binary(vec![1, 2, 3].into()),
            WireMessage::Ping(vec![4].into()),
            WireMessage::Pong(vec![5].into()),
            WireMessage::Close(None),
            WireMessage::Close(Some(CloseFrame {
                code: 1000,
                reason: "bye".into(),
            })),
        ];
        for case in cases {
            let there: TMessage = case.clone().into();
            let back: WireMessage = there.into();
            assert_eq!(case, back, "round trip changed the message");
        }
    }

    #[test]
    fn websocket_data_conversion_keeps_the_payload_allocation() {
        let binary = bytes::Bytes::from_static(b"shared binary");
        let pointer = binary.as_ptr();
        let tungstenite: TMessage = WireMessage::Binary(binary).into();
        let TMessage::Binary(tungstenite_binary) = tungstenite else {
            panic!("binary message changed kind");
        };
        assert_eq!(tungstenite_binary.as_ptr(), pointer);

        let text = TUtf8Bytes::from_static("shared text");
        let pointer = AsRef::<bytes::Bytes>::as_ref(&text).as_ptr();
        let WireMessage::Text(text) = WireMessage::from(TMessage::Text(text)) else {
            panic!("text message changed kind");
        };
        assert_eq!(AsRef::<bytes::Bytes>::as_ref(&text).as_ptr(), pointer);
    }

    #[test]
    fn tungstenite_raw_frame_degrades_to_empty_binary() {
        use tokio_tungstenite::tungstenite::protocol::frame::Frame;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
        let raw = TMessage::Frame(Frame::message(
            bytes::Bytes::from_static(b"x"),
            OpCode::Data(Data::Binary),
            true,
        ));
        assert_eq!(
            WireMessage::from(raw),
            WireMessage::Binary(bytes::Bytes::new())
        );
    }
}
