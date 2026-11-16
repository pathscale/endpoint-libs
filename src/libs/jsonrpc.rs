//! JSON-RPC 2.0 constants shared by every MCP speaker in this crate.
//!
//! No dependency and no feature: the serde wire types (`ws::mcp_wire`) and the `no_std` door
//! (`door::mcp`) both answer JSON-RPC, and the version string and error codes are defined once,
//! here.

pub const JSONRPC_VERSION: &str = "2.0";
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
