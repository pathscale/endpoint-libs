//! The backend edge: where the WebSocket library meets this crate's types.
//!
//! # The name is now wrong
//!
//! This was named after its one backend, `tokio_tungstenite`. There is no
//! tungstenite left anywhere under it: the server's upgrader is nago-wss, the
//! message conversions are nago-wss, and the client moved to nago-wss in its own
//! lane. The name that describes what is here is `backend`, and both files would
//! sit under it unchanged.
//!
//! The rename is not done here on purpose: moving the files in the same change
//! that rewrites them turns a reviewable diff into a pile of adds and deletes,
//! and the rewrite is the part that needs reading. `tungstenite` -> `backend` is
//! a separate, mechanical commit.

mod message;

// The upgrader is server-side; the client brings its own handshake through
// `nago_wss::upgrade::connect` and needs only the message conversions.
#[cfg(feature = "ws")]
pub mod upgrader;

#[cfg(feature = "ws")]
pub use upgrader::NagoWssUpgrader;

// `create_ws_stream` is gone with no replacement. It existed to await hyper's
// `OnUpgrade` and wrap the result in a `WebSocketStream`; nago-wss completes the
// handshake inline and hands back a live connection, so there is no second phase
// to drive and nothing left for the server to call.
#[cfg(feature = "ws")]
#[allow(deprecated)]
pub use upgrader::HyperTungsteniteUpgrader;
