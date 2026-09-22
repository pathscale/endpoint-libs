//! Conversions between the canonical [`WireMessage`] and nago-wss's `Message`.
//!
//! This is the *only* place the backend message type is allowed to meet the rest
//! of the crate. Everything above the backend edge — session, toolbox, subs,
//! push, conn, server, client — deals in [`WireMessage`] exclusively.
//!
//! # What was here before
//!
//! `tokio_tungstenite::Message`, in both directions, plus a fifth case: its
//! `Frame` variant, produced only by its low-level frame API, which this crate
//! never used and which degraded to an empty binary payload. nago-wss has no
//! such escape hatch — a `Message` is one of exactly the five kinds RFC 6455
//! defines — so the conversion is total in both directions with nothing to
//! degrade and nothing to drop.
//!
//! Both message types carry their payload as `Bytes`, so every conversion here
//! is a move. The allocation the kernel's read landed in is the one the handler
//! sees, and the tests below assert that rather than trusting it.

use crate::libs::ws::message::{CloseFrame, Utf8Bytes, WireMessage};
use nago_wss::proto::message::{CloseFrame as NCloseFrame, Message as NMessage};

impl From<WireMessage> for NMessage {
    fn from(msg: WireMessage) -> Self {
        match msg {
            // nago-wss carries text as plain `Bytes` and enforces UTF-8 at the
            // protocol layer rather than in the type, so there is no unchecked
            // constructor to reach for and nothing to assert: handing over the
            // bytes of an already validated `Utf8Bytes` can only be correct.
            WireMessage::Text(text) => Self::Text(text.into_bytes()),
            WireMessage::Binary(payload) => Self::Binary(payload),
            WireMessage::Ping(payload) => Self::Ping(payload),
            WireMessage::Pong(payload) => Self::Pong(payload),
            WireMessage::Close(frame) => Self::Close(frame.map(|frame| NCloseFrame {
                code: frame.code.into(),
                reason: frame.reason.into_bytes(),
            })),
        }
    }
}

impl From<NMessage> for WireMessage {
    fn from(msg: NMessage) -> Self {
        match msg {
            NMessage::Text(payload) => {
                // SAFETY: nago-wss's assembler rejects a text message whose
                // payload is not valid UTF-8 with a protocol error, §5.6, before
                // it can reach a reader. Anything arriving as `Text` is checked.
                Self::Text(unsafe { Utf8Bytes::from_bytes_unchecked(payload) })
            }
            NMessage::Binary(payload) => Self::Binary(payload),
            NMessage::Ping(payload) => Self::Ping(payload),
            NMessage::Pong(payload) => Self::Pong(payload),
            NMessage::Close(frame) => Self::Close(frame.map(|frame| CloseFrame {
                code: frame.code.into(),
                // SAFETY: the same assembler applies the same rule to a close
                // reason, §5.5.1, and a frame that fails it never gets here.
                reason: unsafe { Utf8Bytes::from_bytes_unchecked(frame.reason) },
            })),
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
            let there: NMessage = case.clone().into();
            let back: WireMessage = there.into();
            assert_eq!(case, back, "round trip changed the message");
        }
    }

    #[test]
    fn the_payload_allocation_survives_both_directions() {
        // The whole point of both message types being `Bytes`: a conversion is a
        // move, not a copy. A regression here would be invisible in behaviour and
        // would cost a memcpy of every payload in both directions.
        let binary = bytes::Bytes::from_static(b"shared binary");
        let pointer = binary.as_ptr();
        let NMessage::Binary(out) = NMessage::from(WireMessage::Binary(binary)) else {
            panic!("binary message changed kind");
        };
        assert_eq!(out.as_ptr(), pointer);

        let text = bytes::Bytes::from_static(b"shared text");
        let pointer = text.as_ptr();
        let WireMessage::Text(back) = WireMessage::from(NMessage::Text(text)) else {
            panic!("text message changed kind");
        };
        assert_eq!(back.as_bytes().as_ptr(), pointer);
    }

    #[test]
    fn the_close_code_survives_the_newtype() {
        // nago-wss wraps the code in a `CloseCode` newtype and this crate keeps a
        // bare `u16`, so the conversion crosses a type boundary in both
        // directions and an inverted one would be silent.
        let NMessage::Close(Some(frame)) = NMessage::from(WireMessage::Close(Some(CloseFrame {
            code: 1011,
            reason: "internal".into(),
        }))) else {
            panic!("close message changed kind");
        };
        assert_eq!(u16::from(frame.code), 1011);
        assert_eq!(frame.reason, bytes::Bytes::from_static(b"internal"));

        let WireMessage::Close(Some(frame)) =
            WireMessage::from(NMessage::Close(Some(NCloseFrame {
                code: nago_wss::CloseCode(1011),
                reason: bytes::Bytes::from_static(b"internal"),
            })))
        else {
            panic!("close message changed kind");
        };
        assert_eq!(frame.code, 1011);
        assert_eq!(frame.reason.as_str(), "internal");
    }
}
