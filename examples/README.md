# Examples

## WebSocket Echo Tools

Three small binaries for testing WebSocket connectivity. All send lines from stdin and print what the server sends back. Useful for isolating TLS and protocol issues against echo servers or real backends.

### `ws_echo_rustls`

Uses raw `tokio-tungstenite` with **rustls** — the same TLS stack as the production handlers.

```bash
cargo run --example ws_echo_rustls --features ws -- <server_url> [protocol]
```

### `ws_echo_native_tls`

Uses raw `tokio-tungstenite` with **native-tls** — the same TLS stack as websocat. Use this to isolate whether Cloudflare's TLS fingerprinting is blocking rustls.

```bash
cargo run --example ws_echo_native_tls --features ws -- <server_url> [protocol]
```

### `ws_echo_ws_client`

Uses `WsClient` from this crate. Supports both TLS backends via a flag, making it easy to compare them against the same server without switching binaries.

```bash
# rustls (default)
cargo run --example ws_echo_ws_client --features native-tls -- <server_url>

# native-tls
cargo run --example ws_echo_ws_client --features native-tls -- <server_url> --native-tls
```

> Note: `--features native-tls` implies `ws`, so it covers both backends.

---

### Comparing TLS backends

To determine whether a connection failure is a TLS fingerprinting issue (e.g. Cloudflare blocking rustls):

1. Run `ws_echo_native_tls` against the target — if it connects, the server accepts native-tls
2. Run `ws_echo_rustls` against the same target — if it fails, the issue is the rustls JA4 fingerprint
3. Use `ws_echo_ws_client --native-tls` to verify `WsClient`'s native-tls path specifically
