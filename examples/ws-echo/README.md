# ws-echo

A minimal WebSocket server built with `endpoint-libs`. It exposes a single `Echo` endpoint: send it a message, get it back prefixed with `"echo: "`.

## How it works

The server uses `WebsocketServer` with:

- **`MethodEcho`** — a `RequestHandler` that returns `EchoResponse { message: "echo: <input>" }`.
- **`AllowAllAuthController`** — an `AuthController` that grants role `1` to every connection. Replace this with real auth logic for production.

Messages follow the library's JSON envelope format:

```json
// client → server
{"method": 1, "seq": 1, "params": {"message": "hello"}}

// server → client
{"type": "Immediate", "method": 1, "seq": 1, "params": {"message": "echo: hello"}}
```

## Run locally (no Cloudflare)

From the repo root:

```sh
cargo run --manifest-path examples/ws-echo/Cargo.toml
```

Test with `websocat`:

```sh
websocat ws://localhost:8080
```

Then send a JSON message:

```
{"method":1,"seq":1,"params":{"message":"hello"}}
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
