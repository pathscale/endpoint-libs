use std::fmt;
#[cfg(feature = "ws-core")]
use std::net::SocketAddr;

use async_trait::async_trait;
#[cfg(feature = "ws-core")]
use eyre::Result;
#[cfg(feature = "ws-core")]
use futures::channel::mpsc::Receiver;
#[cfg(feature = "ws-core")]
use futures::future::LocalBoxFuture;

use super::WsMessage as Message;
#[cfg(feature = "ws-core")]
use super::WsServerConfig;

/// An object-safe raw byte stream that can be used across thread boundaries.
///
/// **Breaking change in the tokio removal.** This was
/// `AsyncRead + AsyncWrite + Unpin + Send + 'static` over `tokio::io`, and it named
/// tokio because its only consumers did: the hyper upgrader wrapped a
/// [`BoxedStream`] in `TokioIo` immediately. The upgrader is nago-wss now and the
/// byte stream underneath is [`nagoya::io::Stream`], so the bound follows it.
///
/// It cannot simply *be* `nagoya::io::Stream`. That trait uses `async fn` in a
/// trait, which is not dyn-compatible, and [`BoxedStream`] needs a trait object.
/// So this is a hand-written object-safe mirror of the two methods that boxes each
/// future, and the blanket impl below adapts any `nagoya::io::Stream` to it. The
/// boxing is the price of the trait object and is paid once per read or write on
/// the upgrade path, not per frame: once a connection is established it is owned
/// concretely by `nago_wss::Connection<S>`.
///
/// Transport code that must stay runtime-neutral takes `futures_io` directly and
/// goes through [`framed_json_neutral`](super::transport::framed_json_neutral);
/// that is the seam, not this trait. Gated on `ws-core`, as before.
#[cfg(feature = "ws-core")]
pub trait RawStream: Send + 'static {
    /// Read into `buffer`, returning how many bytes arrived. Zero is the end of
    /// the stream and is not an error, exactly as [`nagoya::io::Stream::read`].
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
    ) -> LocalBoxFuture<'a, Result<usize, StreamError>>;

    /// Write the whole of `buffer`.
    fn write_all<'a>(&'a mut self, buffer: &'a [u8])
    -> LocalBoxFuture<'a, Result<(), StreamError>>;
}

#[cfg(feature = "ws-core")]
impl<T: nagoya::io::Stream + Unpin + Send + 'static> RawStream for T {
    fn read<'a>(
        &'a mut self,
        buffer: &'a mut [u8],
    ) -> LocalBoxFuture<'a, Result<usize, StreamError>> {
        Box::pin(async move {
            nagoya::io::Stream::read(self, buffer)
                .await
                .map_err(|err| StreamError::Other(eyre::eyre!("{err}")))
        })
    }

    fn write_all<'a>(
        &'a mut self,
        buffer: &'a [u8],
    ) -> LocalBoxFuture<'a, Result<(), StreamError>> {
        Box::pin(async move {
            nagoya::io::Stream::write_all(self, buffer)
                .await
                .map_err(|err| StreamError::Other(eyre::eyre!("{err}")))
        })
    }
}

#[cfg(feature = "ws-core")]
pub type BoxedStream = Box<dyn RawStream>;

/// A [`BoxedStream`] is itself a byte stream, so it can be handed to nago-wss.
///
/// Without this the box would be a dead end: `nago_wss::upgrade::accept` wants a
/// `nagoya::io::Stream`, and the whole reason [`RawStream`] exists is that the
/// concrete type has been erased by then.
#[cfg(feature = "ws-core")]
impl nagoya::io::Stream for BoxedStream {
    async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, nagoya::io::StreamError> {
        RawStream::read(self.as_mut(), buffer)
            .await
            .map_err(|_| nagoya::io::StreamError(libc_eio()))
    }

    async fn write_all(&mut self, buffer: &[u8]) -> Result<(), nagoya::io::StreamError> {
        RawStream::write_all(self.as_mut(), buffer)
            .await
            .map_err(|_| nagoya::io::StreamError(libc_eio()))
    }
}

/// `EIO`, spelled once. The error that comes back through [`RawStream`] has already
/// been flattened to a message, so the number is the only thing left to report.
#[cfg(feature = "ws-core")]
fn libc_eio() -> i32 {
    5
}

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
/// Note `?Send`: implementations' futures need not be `Send`. `serve_with`
/// and the session loop poll them in place on one thread. TCP `listen` polls
/// connections the same way; a `LocalSet` remains only for the hyper
/// upgrader's `spawn_local`.
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
    ) -> Result<Receiver<UpgradeEvent>>;
}
