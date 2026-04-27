#[cfg(feature = "ws")]
pub use tokio_tungstenite::tungstenite::Message as WsMessage;

#[cfg(not(feature = "ws"))]
pub use inner::*;

#[cfg(not(feature = "ws"))]
mod inner {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CloseFrame {
        pub code: u16,
        pub reason: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum WsMessage {
        Text(String),
        Binary(Vec<u8>),
        Ping(Vec<u8>),
        Pong(Vec<u8>),
        Close(Option<CloseFrame>),
    }

    impl From<String> for WsMessage {
        fn from(s: String) -> Self {
            WsMessage::Text(s)
        }
    }
}
