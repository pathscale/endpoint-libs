//! The transport seam: what it takes to run the session/dispatch machinery over
//! something that is not a WebSocket.
//!
//! # Design
//!
//! Two layers, deliberately:
//!
//! * [`Transport`] — a *blanket alias* over `Sink + Stream` of [`WireMessage`],
//!   modelled on tarpc's `Transport`. Implementors never name it: anything that is a
//!   `Sink` and a `Stream` of the right item types already is one. This composes for
//!   free with `tokio_util::codec::Framed` and with hand-rolled adapters (XPC
//!   dictionaries, for instance).
//! * [`MessageStream`] — the object-safe, `async fn`-based trait the session loop
//!   actually consumes. It is the pre-2.0 `WsStream` under a transport-neutral name;
//!   `WsStream` remains as an alias.
//!
//! [`TransportStream`] bridges the two, so implementing either one is enough.
//!
//! # Threading
//!
//! [`MessageStream`] is `#[async_trait(?Send)]` — its futures are **not** `Send`,
//! matching the existing `spawn_local` dispatch model. Anything driving a session
//! (`serve_connection`, `serve_with`) must therefore run inside a
//! `tokio::task::LocalSet`. This is not an oversight; it is what lets handlers hold
//! non-`Send` state across await points.

use eyre::eyre;
use futures::{Sink, SinkExt, Stream, StreamExt};

use super::message::WireMessage;
use super::traits::{MessageStream, StreamError};

/// A bidirectional, typed message channel.
///
/// STOLEN SHAPE (tarpc `Transport`): a blanket alias, not a trait to implement. Any
/// `Sink<SinkItem, Error = E> + Stream<Item = Result<Item, E>>` satisfies it.
pub trait Transport<SinkItem, Item>
where
    Self: Stream<Item = Result<Item, <Self as Sink<SinkItem>>::Error>>,
    Self: Sink<SinkItem, Error = <Self as Transport<SinkItem, Item>>::TransportError>,
{
    /// The error type shared by both directions.
    type TransportError: std::error::Error + Send + Sync + 'static;
}

impl<T, SinkItem, Item, E> Transport<SinkItem, Item> for T
where
    T: ?Sized + Stream<Item = Result<Item, E>> + Sink<SinkItem, Error = E>,
    E: std::error::Error + Send + Sync + 'static,
{
    type TransportError = E;
}

/// Adapts any [`Transport`] of [`WireMessage`] into a [`MessageStream`].
///
/// This is the bridge that lets a `Framed<UnixStream, _>`, an in-memory duplex pipe,
/// or an XPC connection drive the ordinary session loop.
pub struct TransportStream<T>(pub T);

impl<T> TransportStream<T> {
    pub fn new(transport: T) -> Self {
        Self(transport)
    }

    /// Recover the wrapped transport.
    pub fn into_inner(self) -> T {
        self.0
    }
}

#[async_trait::async_trait(?Send)]
impl<T> MessageStream for TransportStream<T>
where
    T: Transport<WireMessage, WireMessage> + Unpin + Send,
{
    async fn send(&mut self, msg: WireMessage) -> Result<(), StreamError> {
        SinkExt::send(&mut self.0, msg)
            .await
            .map_err(|err| StreamError::Other(eyre!(err)))
    }

    async fn recv(&mut self) -> Option<Result<WireMessage, StreamError>> {
        StreamExt::next(&mut self.0)
            .await
            .map(|res| res.map_err(|err| StreamError::Other(eyre!(err))))
    }
}

#[cfg(feature = "framed-transport")]
pub mod framed;

#[cfg(feature = "framed-transport")]
pub use framed::{FramedError, framed_json};
