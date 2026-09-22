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
{"type":"Error","method":211,"code":100400,"seq":2,"log_id":"...","params":{"kind":"Rejected","message":"ReceiveUserInfo is not processed by this test server"}}
```

The optional fields `appPubId` and `token` can also be included:

```json
{"method": 211, "seq": 3, "params": {"userPubId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890", "username": "alice", "appPubId": "deadbeef-0000-0000-0000-000000000001", "token": "my-token"}}
```

## Run locally

From the repo root — available with either backend:

```sh
cargo run --example ws_echo_server --features ws
```

The server listens on **port 8443**, in plain `ws://`. It used to generate a
self-signed certificate at startup and serve `wss://` itself; this crate does
not terminate TLS any more, because terminating it is what pins the path to
`std` and the fleet's internal services want to stay no_std-friendly. In
deployment the edge proxy terminates and forwards plain to this port. Test with
`websocat`:

```sh
websocat ws://localhost:8443
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
{"type":"Error","method":211,"code":100400,"seq":2,"log_id":"...","params":{"kind":"Rejected","message":"ReceiveUserInfo is not processed by this test server"}}
```
