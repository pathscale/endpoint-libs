# Examples

One example remains: [`ws-echo`](ws-echo), a minimal `endpoint-libs` server. Its own
[README](ws-echo/README.md) covers the endpoints it exposes and the frames they answer.

```sh
cargo run --example ws_echo_server --features ws
```

It serves plain `ws://`. There is no certificate to generate and no `-k` flag to pass,
because this crate no longer terminates TLS at all — that belongs to the edge proxy. See
[`ws`](../README.md#ws) in the top-level README.

## What was here before

Four examples were deleted in 3.2.0, along with `test_ws_echo_tls.sh`:

- **`mcp_echo`** — an MCP handshake and a legacy frame on one connection. Nothing
  replaces it yet; `tests/transport_seam.rs` drives the same path.
- **`uds_echo`** — a worked example over a Unix domain socket, built on `framed_json()`
  and therefore on the `framed-transport-tokio` feature. Both are gone;
  `tests/nagoya_transport.rs` and `tests/transport_seam.rs` exercise the framed path
  that replaced them.
- **`ws_echo_ws_client`** and **`ws_echo_rustls`** — the last of the TLS-backend
  comparison clients (`ws_echo_native_tls` had gone earlier). They existed to tell a
  rustls JA4 fingerprint rejection apart from a real connection failure, by pointing
  `tokio-tungstenite` at the same server over rustls and over native-tls. That question
  cannot be asked from here any more: there is no rustls and no tokio-tungstenite in
  this crate's graph, and client-side TLS is an opt-in `ws-client-tls` feature layered
  on nago-wss. Reproduce a fingerprinting suspicion with `websocat` against the target
  instead.
