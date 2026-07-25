//! Conversions between the canonical [`WireMessage`] and tungstenite's `Message`.
//!
//! These are the *only* place the tungstenite message type is allowed to meet the rest
//! of the crate. Everything above the backend edge — session, toolbox, subs, push,
//! conn, server, client — deals in [`WireMessage`] exclusively.

use tokio_tungstenite::tungstenite::Message as TMessage;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame as TCloseFrame;

use crate::libs::ws::message::{CloseFrame, WireMessage};

impl From<WireMessage> for TMessage {
    fn from(msg: WireMessage) -> Self {
        match msg {
            WireMessage::Text(t) => Self::Text(t.into()),
            WireMessage::Binary(b) => Self::Binary(b.into()),
            WireMessage::Ping(b) => Self::Ping(b.into()),
            WireMessage::Pong(b) => Self::Pong(b.into()),
            WireMessage::Close(frame) => Self::Close(frame.map(|f| TCloseFrame {
                code: f.code.into(),
                reason: f.reason.into(),
            })),
        }
    }
}

impl From<TMessage> for WireMessage {
    fn from(msg: TMessage) -> Self {
        match msg {
            TMessage::Text(t) => Self::Text(t.as_str().to_owned()),
            TMessage::Binary(b) => Self::Binary(b.into()),
            TMessage::Ping(b) => Self::Ping(b.into()),
            TMessage::Pong(b) => Self::Pong(b.into()),
            TMessage::Close(frame) => Self::Close(frame.map(|f| CloseFrame {
                code: f.code.into(),
                reason: f.reason.as_str().to_owned(),
            })),
            // tungstenite's `Frame` variant is only produced by its low-level frame
            // API, which this crate never uses. Map it to an empty binary payload
            // rather than panicking: an unexpected raw frame is not worth aborting a
            // live session over, and the session layer will simply ignore it.
            TMessage::Frame(_) => Self::Binary(Vec::new()),
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
            WireMessage::Binary(vec![1, 2, 3]),
            WireMessage::Ping(vec![4]),
            WireMessage::Pong(vec![5]),
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
    fn tungstenite_raw_frame_degrades_to_empty_binary() {
        use tokio_tungstenite::tungstenite::protocol::frame::Frame;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
        let raw = TMessage::Frame(Frame::message(
            bytes::Bytes::from_static(b"x"),
            OpCode::Data(Data::Binary),
            true,
        ));
        assert_eq!(WireMessage::from(raw), WireMessage::Binary(Vec::new()));
    }
}
