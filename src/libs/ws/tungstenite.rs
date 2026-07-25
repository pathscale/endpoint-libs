mod message;

// The upgrader is server-side; the client only needs the message conversions.
#[cfg(feature = "ws")]
pub mod upgrader;

#[cfg(feature = "ws")]
pub use upgrader::HyperTungsteniteUpgrader;
#[cfg(feature = "ws")]
pub use upgrader::create_ws_stream;
