#![cfg_attr(test, allow(deprecated))]

#[cfg(all(feature = "ws", feature = "ws-wtx"))]
compile_error!(
    "features `ws` and `ws-wtx` are mutually exclusive — they provide conflicting WebSocket backends (tungstenite vs wtx). Enable at most one."
);

#[cfg(all(feature = "ws", feature = "ws-wtx-http2"))]
compile_error!(
    "features `ws` and `ws-wtx-http2` are mutually exclusive — ws-wtx-http2 requires the ws-wtx backend."
);

pub mod libs;
#[cfg(feature = "types")]
pub mod model;
