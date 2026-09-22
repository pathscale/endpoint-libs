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
//! [`MessageStream`] is `#[async_trait(?Send)]` — its futures are **not** `Send`.
//! `serve_connection` / `serve_with` poll the session and its request handlers
//! in place on one thread, so they do not need a `LocalSet`. TCP `listen` now
//! polls connections the same way. A `LocalSet` remains only because the hyper
//! upgrader `spawn_local`s onto `TokioExecutor`; nago-wss is the tokio-free
//! replacement for that backend, and is not wired here yet.
//!
//! # Which feature sets are free of tokio
//!
//! Verified with `cargo tree -e normal -i tokio`, which is the fact; a feature
//! flag alone is not.
//!
//! This became answerable only when `tokio` was made `optional = true` in
//! `Cargo.toml`. Up to 3.1.1 it was a non-optional `features = ["full"]`
//! dependency, so every claim below was false by construction, whatever the
//! feature list said. tokio now hangs off the features whose own code calls it,
//! and names the tokio features that code uses rather than `full`.
//!
//! - `wire-core,framed-transport,nagoya-transport` prints nothing. This is the
//!   neutral path and it is genuinely runtime-free.
//! - `types` alone prints nothing either, since the OTLP exporters moved behind
//!   the `otel` feature.
//! - Anything including `ws-core` still carries tokio. This is no longer an
//!   accident of `types`: `ws-core` names `dep:tokio` on its own account, and
//!   the code behind it means that.
//!
//! ## What `ws-core` would have to give up
//!
//! Ranked by how hard it is to remove, not by how often it is cited.
//!
//! 1. **The TCP server path.** `listener.rs` is `tokio::net::TcpListener` and
//!    `ConnectionListener`'s associated types are bounded on `tokio::io`.
//!    `WebsocketServer::listen_impl` and `run_shard` build a
//!    `tokio::runtime::Builder::new_current_thread` runtime per shard, a
//!    `LocalSet`, a `tokio::spawn`ed date-cache task and a `tokio::time::sleep`.
//!    The `futures` crate owns no reactor, so there is no futures-only
//!    substitute for any of it. Replacing it means a second server built on a
//!    `nagoya::reactor::TcpListener`, which is a parallel implementation rather
//!    than a primitive swap.
//! 2. **Signal delivery.** `ws-core` requires the `signal` feature. The flag
//!    is a `nagoya::sync::Notify` plus an `AtomicBool` (`Shutdown`); the
//!    `tokio_util` `CancellationToken` is gone. What remains is
//!    `tokio::signal::unix`. `listen_impl` waits on it to shut down. nagoya
//!    0.1.9 has `notify_waiters` and no signal module, so delivery is not
//!    portable inside this crate yet. `signal` names `dep:nagoya` for the
//!    flag and `tokio/signal` for delivery, and the nagoya dependency enables
//!    `reactor`, so the reactor comes along too.
//! 3. **`tokio::task_local!`** for `TOOLBOX` in `toolbox.rs`. `futures` has no
//!    task-local. `scoped-tls` is not a substitute: its scope is synchronous and
//!    does not survive an `.await`, and `TOOLBOX.scope(..).await` spans awaits.
//!    A replacement has to be a hand-rolled future that sets a `thread_local!`
//!    on poll entry and restores it on return, which is what tokio's own
//!    `TaskLocalFuture` is.
//! 4. **Channels**, the part usually named first. `tokio::sync::mpsc` remains in
//!    `session.rs`, `conn.rs` and `toolbox.rs`. The `select!`s are already
//!    `futures::future::select` plus `Either` (`session.rs::run_loop`,
//!    `server.rs::listen_impl`, `signal.rs`). A `futures` bounded channel is not
//!    a drop-in for the queues: it reserves a slot per sender, so
//!    `drop_conn_on_buffer_full` would fire at a different depth.
//!
//! `TransportStream`/`RawStream` over `tokio::io` is *not* on this list. See the
//! comment on `RawStream` in `traits.rs`: its only consumers are the hyper
//! upgrader, tokio-tungstenite and tokio-rustls, all of which are gated on
//! `ws`/`ws-client` and tokio-bound regardless.
//!
//! Doing only 3 and 4 changes the public API and moves the `cargo tree` output
//! not at all, because 1 and 2 still name tokio. So the useful shape is a
//! feature split first: put the TCP/TLS/upgrade cluster and `signal` behind
//! their own feature, leaving a `ws-core` that offers `serve_connection` and
//! `serve_with` over a `SessionListener`, and only
//! then port 3 and 4. Until that lands, a consumer that wants no tokio at all
//! takes the neutral transport and leaves `ws-core` out.

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

#[cfg(feature = "nagoya-transport")]
pub mod nagoya;

#[cfg(feature = "framed-transport")]
pub use framed::FramedError;
#[cfg(feature = "framed-transport-tokio")]
pub use framed::framed_json;

#[cfg(feature = "nagoya-transport")]
pub use nagoya::NagoyaStream;
