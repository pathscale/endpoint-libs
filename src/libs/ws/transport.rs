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
//!   free with `framed_json_neutral` (the `framed-transport` feature) and with
//!   hand-rolled adapters (XPC dictionaries, for instance).
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
//! in place on one thread, and TCP `listen` polls connections the same way.
//!
//! There is no `LocalSet` anywhere in this crate any more, and no executor to
//! host one. The last one existed because the hyper upgrader `spawn_local`ed
//! onto `TokioExecutor`; the upgrade is nago-wss's now, and the server runs on
//! a nagoya reactor it builds itself (`server.rs`: `Reactor::local` plus
//! `block_on_with`). A caller that polls a session from somewhere else owns the
//! thread while it does — that is the whole requirement, and any single-threaded
//! executor satisfies it.
//!
//! # Which feature sets are free of tokio
//!
//! All of them, with nothing left over. That is a claim about
//! `cargo tree -e normal -i tokio`, which is the fact; a feature flag alone is
//! not.
//!
//! There is no exception and no interop flavour held back for a consumer that
//! still runs tokio. `framed-transport-tokio` was one and is deleted: the
//! neutral path put identical bytes on the wire, so it offered an adapter type
//! and a dependency. `otel` was the other, and the edge there was genuinely
//! upstream's -- `opentelemetry-otlp` reaches `tonic` for the OTLP protobuf
//! message types and tonic reaches tokio through tokio-stream -- so rather
//! than carry a runtime for a feature nothing in the fleet enabled, the OTLP
//! exporter is deleted too. `OtelConfig` stays, inert, because backends
//! construct it in a `LoggingConfig` literal.
//!
//! Everything this crate used to do with tokio, it now does with nagoya or with
//! `futures`:
//!
//! 1. **The TCP server path** is nagoya. `listener.rs` is
//!    `nagoya::reactor::TcpListener` and `ConnectionListener`'s associated types
//!    are bounded on `nagoya::io::Stream`, which is an async-fn trait rather
//!    than a `poll_read`/`ReadBuf` pair. `WebsocketServer::listen_impl` builds a
//!    `Reactor::local` and drives the accept loop under `block_on_with`; the
//!    per-shard threads and their accept `mpsc` went with it.
//! 2. **Signal delivery** is nagoya. The flag is a `nagoya::sync::Notify` plus
//!    an `AtomicBool` (`Shutdown`), and delivery is `nagoya::signal::Signal`. A
//!    `Signal` is registered on one reactor and fires only while that reactor is
//!    polled, which is why `init_signals` takes the `Handle`.
//! 3. **The per-connection queue** is `outbound`, this crate's own. Its bound is
//!    the number of queued `WsMessage`s and a sender does not reserve a slot, so
//!    `drop_conn_on_buffer_full` fires at exactly the configured depth.
//!    `futures::channel::mpsc` reserves `buffer + num_senders` and would not.
//! 4. **The `select!`s** are `futures::future::select` plus `Either`
//!    (`session.rs::run_loop`, `server.rs::listen_impl`, `signal.rs`). That is
//!    not a fairness-preserving swap: `select` takes its left arm the moment it
//!    is ready and never polls the right one, where `tokio::select!` chose at
//!    random. So the session loop's order is a deliberate priority
//!    (`session::priority4`): a finished handler, then outbound, then inbound,
//!    then the policy flag. Handlers come first because they are the one arm a
//!    peer cannot keep ready, so anywhere below inbound they starve.
//! 5. **`TOOLBOX`** in `toolbox.rs` is a `thread_local` installed on poll entry
//!    and restored on return, including on panic and drop — never a
//!    `tokio::task_local`.
//!
//! `RawStream` is the one bound that looks like it should be `futures_io` and is
//! not: it is an object-safe mirror of `nagoya::io::Stream`, because its
//! consumers are the nago-wss upgrade path. See the comment on it in
//! `traits.rs`. Transport code that must stay runtime-neutral takes `futures_io`
//! and goes through `framed_json_neutral` — that
//! is the seam, and it is what this module is for.

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

// `framed_json` was re-exported here alongside these until 3.2.0. It is gone with
// the tokio flavour, and nothing replaces it at this path: `framed_json_neutral`
// takes a `futures_io` stream and lives in `framed`, which is public. A consumer
// that was on `framed_json` bridges its tokio stream with `tokio-util`'s `compat`
// on its own side; `framed`'s module docs spell out the one line.
#[cfg(feature = "framed-transport")]
pub use framed::{FramedError, framed_json_neutral, framed_json_neutral_with_max_frame};

#[cfg(feature = "nagoya-transport")]
pub use nagoya::NagoyaStream;
