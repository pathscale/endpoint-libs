# ws-echo

A minimal WebSocket server built with `endpoint-libs`. It exposes two endpoints:

- **`Echo` (method 1)** — send a message, get it back prefixed with `"echo: "`.
- **`ReceiveUserInfo` (method 211)** — mirrors the `ReceiveUserInfo` contract; parses the request and always returns an error confirming the test passed.

## How it works

The server uses `WebsocketServer` with:

- **`MethodEcho`** — a `RequestHandler` that returns `EchoResponse { message: "echo: <input>" }`.
- **`MethodReceiveUserInfo`** — a `RequestHandler` that parses a `ReceiveUserInfo`-shaped payload and returns a `BAD_REQUEST` error with a message confirming receipt (test-server behaviour).
- **`AllowAllAuthController`** — an `AuthController` that grants role `1` to every connection. Replace this with real auth logic for production.

Messages follow the library's JSON envelope format:

```json
// client → server
{"method": 1, "seq": 1, "params": {"message": "hello"}}

// server → client
{"type": "Immediate", "method": 1, "seq": 1, "params": {"message": "echo: hello"}}
```

```json
// client → server (ReceiveUserInfo, method 211)
{"method": 211, "seq": 2, "params": {"userPubId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890", "username": "alice"}}

// server → client (always an error — test server)
{"type":"Error","method":211,"seq":2,"params":"Test passed: received ReceiveUserInfo for user 'alice' (id: a1b2c3d4-e5f6-7890-abcd-ef1234567890) — this is a test server and will not process the request","error_code":{"code":100400},"log_id":"..."}
```

The optional fields `appPubId` and `token` can also be included:

```json
{"method": 211, "seq": 3, "params": {"userPubId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890", "username": "alice", "appPubId": "deadbeef-0000-0000-0000-000000000001", "token": "my-token"}}
```

## Run locally

From the repo root — available with either backend:

```sh
# Hyper + tungstenite backend (default)
cargo run --example ws_echo_server --features ws

# WTX backend
cargo run --example ws_echo_server_wtx --features ws-wtx
```

The server generates a self-signed TLS certificate at startup and listens on **port 8443**. Test with `websocat` (note the `wss://` scheme and `-k` flag to skip certificate verification):

```sh
websocat -k wss://localhost:8443
```

Then send a JSON message:

```
{"method":1,"seq":1,"params":{"message":"hello"}}
```

To call the `ReceiveUserInfo` endpoint (minimal, no optional fields):

```
{"method":211,"seq":2,"params":{"userPubId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","username":"alice"}}
```

With optional fields:

```
{"method":211,"seq":3,"params":{"userPubId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","username":"alice","appPubId":"deadbeef-0000-0000-0000-000000000001","token":"my-token"}}
```

Expected response (error confirming the test passed):

```
{"type":"Error","method":211,"seq":2,"params":"Test passed: received ReceiveUserInfo for user 'alice' (id: a1b2c3d4-e5f6-7890-abcd-ef1234567890) — this is a test server and will not process the request","error_code":{"code":100400},"log_id":"..."}
```

## Run with Cloudflare Containers (wrangler dev)

The Docker build context must be the repo root (so it can access the local `endpoint-libs` source via the path dependency). Run from the **repo root**:

```sh
npm install
npx wrangler dev --config examples/ws-echo/wrangler.toml
```

Test:

```sh
websocat ws://localhost:8787
```

Then send a JSON message:

```
{"method":1,"seq":1,"params":{"message":"hello"}}
```

Expected response:

```
{"type":"Immediate","method":1,"seq":1,"params":{"message":"echo: hello"}}
```

Or call `ReceiveUserInfo`:

```
{"method":211,"seq":2,"params":{"userPubId":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","username":"alice"}}
```

Expected response:

```
{"type":"Error","method":211,"seq":2,"params":"Test passed: received ReceiveUserInfo for user 'alice' (id: a1b2c3d4-e5f6-7890-abcd-ef1234567890) — this is a test server and will not process the request","error_code":{"code":100400},"log_id":"..."}
```
