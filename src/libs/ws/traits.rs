use std::fmt;
use std::net::SocketAddr;

use async_trait::async_trait;
use crossfire::{AsyncRx, mpsc::Array};
use eyre::Result;
use tokio::io::{AsyncRead, AsyncWrite};

use super::WsMessage as Message;
use super::WsServerConfig;

/// Combined trait alias for a raw byte stream that can be used across thread boundaries.
pub trait RawStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> RawStream for T {}

pub type BoxedStream = Box<dyn RawStream>;

#[derive(Debug)]
pub enum StreamError {
    Closed,
    WriteBufferFull,
    Protocol(String),
    Other(eyre::Error),
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Closed => write!(f, "connection closed"),
            StreamError::WriteBufferFull => write!(f, "write buffer full"),
            StreamError::Protocol(s) => write!(f, "protocol error: {s}"),
            StreamError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StreamError::Other(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

#[async_trait(?Send)]
pub trait WsStream: Unpin + Send {
    async fn send(&mut self, msg: Message) -> Result<(), StreamError>;
    async fn recv(&mut self) -> Option<Result<Message, StreamError>>;
}

/// An upgrade event yielded by the upgrader.
/// Contains the on_upgrade future and the negotiated protocol.
pub struct UpgradeEvent {
    pub on_upgrade: hyper::upgrade::OnUpgrade,
    pub protocol: String,
}

#[async_trait]
pub trait WsUpgrader: Send + Sync {
    /// Returns a receiver that yields upgrade events.
    /// - H1: receiver yields exactly one event (single WebSocket per TCP connection)
    /// - H2: receiver yields multiple events (one per CONNECT request, multiplexing)
    async fn upgrade_stream(
        &self,
        stream: BoxedStream,
        addr: SocketAddr,
        config: &WsServerConfig,
        cached_date: &str,
    ) -> Result<AsyncRx<Array<UpgradeEvent>>>;
}