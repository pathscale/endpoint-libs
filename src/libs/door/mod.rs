//! The door: request/reply over a local socket, independent of application vocabulary.
//!
//! The counted wire (a request line; a reply `ok N` or `err N` and exactly N lines) and the
//! framed wire ([`frame`], this crate's own length-delimited frame from [`crate::libs::frame`]),
//! ordered TOON/JSON documents ([`document`]), a strict JSON reader that never round-trips a
//! number through a float ([`json`]), MCP discovery and dispatch ([`mcp`]), the typed tool
//! registry ([`registry`], [`typed`]), the job queue ([`queue`]) and blocking client and server
//! halves ([`client`], [`server`]).
//!
//! `no_std` on [`ekostd`](https://crates.io/crates/ekostd) (`eko`): no async runtime, no serde,
//! no proc macro, no build script, so it builds in the stage0 world (empty sysroot) as well as
//! with `std`. It was ekopathrs's `crates/door` (`eko-door`) until 16 November 2026; ekod's own
//! request and reply vocabulary stays in ekopathrs (`eko-compiler-protocol`).

pub use eko as sys;

pub mod client;
pub mod document;
pub mod frame;
pub mod json;
pub mod legacy;
pub mod mcp;
pub mod panics;
pub mod protocol;
pub mod queue;
pub mod registry;
pub mod server;
pub mod typed;

/// The scale is the scale supplied by `eko::clock::now_ns` on this target.
pub fn since(started: u64) -> core::time::Duration {
    core::time::Duration::from_nanos(eko::clock::now_ns().wrapping_sub(started))
}
