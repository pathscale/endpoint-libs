compile_error!(
    "ws-wtx and ws-wtx-http2 features are deprecated. \
     Use the ws (tungstenite/hyper) feature for WebSocket support, \
     which includes HTTP/2 multiplexing support."
);

mod stream;
mod upgrader;

pub use upgrader::WtxUpgrader;
// Only re-export specific items, not the naming conflicts
// with tungstenite backend when both ws and ws-wtx are enabled.
