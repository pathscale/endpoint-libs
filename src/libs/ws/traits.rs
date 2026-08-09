use std::fmt;
#[cfg(feature = "ws-core")]
use std::net::SocketAddr;

use async_trait::async_trait;
#[cfg(feature = "ws-core")]
use crossfire::{AsyncRx, mpsc::Array};
#[cfg(feature = "ws-core")]
use eyre::Result;
use tokio::io::{AsyncRead, AsyncWrite};

use super::WsMessage as Message;
#[cfg(feature = "ws-core")]
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

/// An object-safe, bidirectional message channel — what the session loop consumes.
///
/// Renamed from `WsStream` in 2.0 (the alias below keeps old code compiling): it is
/// no longer WebSocket-specific. A Unix socket, a named pipe, or an XPC connection
/// implements this just as well, either directly or via
/// [`TransportStream`](super::TransportStream).
///
/// Note `?Send`: implementations' futures need not be `Send`, which matches the
/// `spawn_local` dispatch model. Drivers must run inside a `LocalSet`.
#[async_trait(?Send)]
pub trait MessageStream: Unpin + Send {
    async fn send(&mut self, msg: Message) -> Result<(), StreamError>;
    async fn recv(&mut self) -> Option<Result<Message, StreamError>>;
}

/// Compatibility alias for the pre-2.0 name.
pub use MessageStream as WsStream;

/// An upgrade event yielded by the upgrader.
/// Contains the on_upgrade future and the negotiated protocol.
#[cfg(feature = "ws-core")]
pub struct UpgradeEvent {
    /// Only present with a hyper-based backend (`ws` or `ws-client`), which is
    /// what provides the `hyper` dependency.
    #[cfg(any(feature = "ws", feature = "ws-client"))]
    pub on_upgrade: hyper::upgrade::OnUpgrade,
    pub protocol: String,
}

#[cfg(feature = "ws-core")]
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
