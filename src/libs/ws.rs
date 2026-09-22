#[cfg(feature = "ws-core")]
mod basics;
#[cfg(feature = "ws-core")]
mod conn;
#[cfg(feature = "ws-core")]
pub mod handler;
#[cfg(feature = "ws-core")]
mod headers;
#[cfg(feature = "ws-core")]
pub mod hooks;
#[cfg(feature = "ws-core")]
mod listener;
#[cfg(feature = "ws-core")]
pub mod mcp;
pub mod mcp_wire;
mod message;
#[cfg(feature = "ws-core")]
pub mod outbound;
#[cfg(feature = "ws-core")]
mod push;
#[cfg(feature = "ws-core")]
mod server;
#[cfg(feature = "ws-core")]
mod session;
#[cfg(feature = "ws-core")]
mod subs;
// `tls` is gone. Server-side TLS is terminated at the edge — fly.io's
// `[http_service]` with `force_https`, speaking plain to the internal port — so
// this crate serves `ws://` only. It was deleted rather than ported because TLS
// is what pins this path to `std`, and the fleet's internal services want to
// stay no_std-friendly. `TlsListener`, the certificate loading, the ALPN list and
// the `ws-tls12` version gate went with it. A client dialling an external
// `wss://` needs `ws-client-tls`, which is opt-in for the same std reason.
#[cfg(feature = "ws-core")]
pub mod toolbox;
mod traits;
pub mod transport;

#[cfg(feature = "ws-core")]
mod client;
#[cfg(feature = "ws")]
pub(crate) mod tungstenite;

#[cfg(feature = "ws-core")]
pub use basics::*;
#[cfg(feature = "ws-core")]
pub use conn::*;
#[cfg(feature = "ws-core")]
pub use headers::*;
#[cfg(feature = "ws-core")]
pub use hooks::*;
#[cfg(feature = "ws-core")]
pub use listener::*;
pub use message::*;
#[cfg(feature = "ws-core")]
pub use server::*;
#[cfg(feature = "ws-core")]
pub use session::*;
#[cfg(feature = "ws-core")]
pub use subs::*;
pub use traits::*;
pub use transport::*;

#[cfg(feature = "ws-core")]
pub use client::*;
#[cfg(feature = "ws")]
pub use tungstenite::*;
