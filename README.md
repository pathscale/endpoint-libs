# endpoint-libs

[![Crates.io](https://img.shields.io/crates/v/endpoint-libs)](https://crates.io/crates/endpoint-libs)
[![Docs.rs](https://docs.rs/endpoint-libs/badge.svg)](https://docs.rs/endpoint-libs)
[![CI](https://github.com/pathscale/endpoint-libs/actions/workflows/rust.yml/badge.svg)](https://github.com/pathscale/endpoint-libs/actions/workflows/rust.yml)
[![License: MIT](https://img.shields.io/crates/l/endpoint-libs)](LICENSE)
[![Security audit](https://deps.rs/crate/endpoint-libs/2.1.3/status.svg)](https://deps.rs/crate/endpoint-libs/2.1.3)

`endpoint-libs` exists to make it fast and easy to launch MCP services — killing
boilerplate, tightening security, and raising the quality of both hand-written and
AI-generated code.

You describe your endpoints once in RON.
[`endpoint-gen`](https://github.com/pathscale/EndpointGen) generates the Rust models, the
docs, the MCP tool schemas, and the machine-readable description that
[`endpoint-validator`](https://crates.io/crates/endpoint-validator) drives end-to-end
tests from. This crate is the runtime that serves them.

Together they are a schema-first RPC pipeline: **`endpoint-gen` generates,
`endpoint-libs` serves, `endpoint-validator` verifies** — one declarative source of truth
behind all three.

- **WebSocket RPC server** — `{method, seq, params}` frames over a persistent socket, with
  connection/session management, push and subscription infrastructure, and typed handlers.
- **Roles and typed public errors** — endpoints declare which roles may call them; handlers
  return a typed error enum that becomes a stable public error contract.
- **Every endpoint is an MCP tool, for free** — one `enable_mcp()` call exposes your whole
  RPC surface as Model Context Protocol tools over JSON-RPC 2.0, on the same socket, with
  `inputSchema`/`outputSchema` generated from the endpoint definitions and `tools/list`
  filtered by the caller's roles. You write no tool definitions, no JSON Schema, and no
  second server. Off by default and fully additive — see
  [MCP support](#mcp-model-context-protocol-support).
- **Schema model** — `Type`/`Field`/`EndpointSchema` plus `to_json_schema`, emitting JSON
  Schema 2020-12; the basis for MCP tool schemas and the OpenAPI/AsyncAPI documents.
- **Transport-agnostic core (2.0)** — the WebSocket backend is one implementation of a
  transport seam. Length-delimited framing over Unix sockets, named pipes or inherited
  socketpairs is a feature flag away, with no TLS or HTTP compiled in.

### How it compares

What the pipeline does that the alternatives do not. "Codegen direction" is the row that
matters most: everything in the OpenAPI column derives a spec *from* handwritten handlers,
so it deletes no boilerplate.

| | endpoint-libs + endpoint-gen | tonic | tarpc | jsonrpsee | utoipa / aide / dropshot |
|---|---|---|---|---|---|
| Codegen direction | **spec → code** | spec → code | none | none | code → spec |
| One external file drives everything | **yes** (RON) | proto | no | no | no |
| Generated typed handlers | **yes** | yes | via macro | via macro | no |
| Generated docs | **yes** | no | no | no | **yes** |
| **MCP tools generated** | **yes, built in** | no | no | no | via `rmcp-openapi` |
| Roles / RBAC in the schema | **yes** | no | no | no | no |
| Typed public error contract | **yes** | partial | no | no | no |
| Describes a WS message protocol | **yes** (AsyncAPI) | n/a | n/a | partial | **no** |
| Emits OpenAPI / AsyncAPI | **yes** (opt-in) | no | no | no | OpenAPI only |
| Transport-agnostic core | yes (2.0) | no (h2) | **yes** | yes | no |
| Ecosystem maturity | small (8.6k dl) | **338M** | 8.9M | 22.8M | **37M** (utoipa) |

The last row is not a typo, and it is the honest counterweight to the rest of the table:
this is a small crate. Every alternative has more users, more integrations and more
answers written about it. Pick the rows that matter to you, not the count of bold cells.

Full reasoning, sources and download figures: [`docs/comparison.md`](docs/comparison.md).

### When this is *not* the right crate

Be honest about the fit — it is a narrow one:

- **You want a REST/HTTP API.** Use `axum` with [`utoipa`](https://crates.io/crates/utoipa)
  or [`aide`](https://crates.io/crates/aide), or [`dropshot`](https://crates.io/crates/dropshot)
  if you want the spec to be the contract. They are far more mature at that job.
- **You want gRPC.** [`tonic`](https://crates.io/crates/tonic) is the answer and it is not
  close.
- **You want general-purpose Rust-to-Rust RPC** with no config file and no browser client.
  [`tarpc`](https://crates.io/crates/tarpc) is cleaner, and its `Transport` design is what
  the 2.0 seam here was modelled on.
- **You want standards-first JSON-RPC.** [`jsonrpsee`](https://crates.io/crates/jsonrpsee)
  is the mature choice.

This crate earns its place only when you want *one declarative source of truth* driving
generated types, generated docs, generated MCP tools, role gating and typed errors together
— and when a WebSocket message protocol, not HTTP, is what you are actually serving. See
[`docs/comparison.md`](docs/comparison.md) for the full survey.

## Version compatibility

Minor versions do **not** need to match across `endpoint-libs`, `endpoint-gen` and
`honey_id-types`. They are separate crates on separate cadences — as of this writing
endpoint-libs 2.1, endpoint-gen 1.13 and honey_id-types 2.0 interoperate in production.

What is actually enforced:

- **`endpoint-gen`** records the `endpoint-libs` version it was built against and checks
  it at generation time against `[libs] version` in your `config/version.toml`. A
  mismatch fails generation with an explicit message rather than emitting subtly wrong
  code.
- **`honey_id-types`** re-exports this crate's `WsRequest`/`WsResponse` traits. Bumping
  one without the other can put two incompatible copies of `endpoint-libs` in a single
  dependency graph; the resulting error names two different `endpoint-libs` paths and is
  otherwise baffling. The check that catches it is one line:

  ```sh
  grep -c 'name = "endpoint-libs"' Cargo.lock   # must be exactly 1
  ```

The release order for the whole chain is in [`docs/release-order.md`](docs/release-order.md).

## Features

The crate is feature-gated. The default feature set is `types` only.

### `types` (default)

Endpoint schema types shared between services and `endpoint-gen`:

- `Type`, `Field`, `EnumVariant` — the type system used to describe endpoint request/response schemas
- `TypeRegistry` and `Type::to_json_schema` — conversion of endpoint schemas to JSON Schema (used for MCP tool definitions, see below)
- Blockchain primitive types: `BlockchainAddress`, `BlockchainTransactionHash`, `U256`, `H256`

### `ws-core`

Shared WebSocket infrastructure — `WireMessage`, server, session, traits, toolbox — with
no backend, no TLS and no HTTP. Everything the other `ws-*` features build on. `WsClient`
and `WsClient::from_stream` are available here too, so a sidecar speaking only a local
transport does not compile a TLS/WebSocket stack it never uses.

### `agent-control`

The minimal built-in local wire surface for always-on application control. It enables
`WireMessage`, `TransportStream`, and length-delimited `framed_json` without the endpoint
server, WebSocket, HTTP, TLS, database, scheduler, or diagnostics layers. Agent-control
messages use the built-in `mcp_wire` JSON-RPC envelopes; application-specific generated
endpoints define the semantic inspect/action/lifecycle tools.

### `ws-client`

The connecting half: `WsClient::new` (TCP/TLS), `WsClientBuilder` and the connect helpers.
Standalone — you can build a client without the server.

### `framed-transport`

Length-delimited `WireMessage` framing over any byte stream: Unix sockets, named pipes,
inherited socketpairs. No WebSocket, no TLS, no HTTP. Wire format under
[Transports (2.0)](#transports-20) below, and machine-readable in the generated AsyncAPI
document.

### `ws`

Async WebSocket server built on `tokio-tungstenite` with TLS support via `rustls`. Includes:

- Connection management and session tracking
- Push/subscription infrastructure
- Request handler and auth subcontroller traits with typed public errors
- HTTP header parsing helpers

Auth endpoints registered through `EndpointAuthController::add_auth_endpoint` use the same
typed error model as regular `RequestHandler` implementations. A `SubAuthController` declares
its generated request type and endpoint-local public error type, then returns `AuthResponse`.

```rust
use std::sync::Arc;

use endpoint_libs::libs::error_code::ErrorCode;
use endpoint_libs::libs::handler::HandlerError;
use endpoint_libs::libs::toolbox::{ArcToolbox, CustomError, RequestContext};
use endpoint_libs::libs::ws::{AuthResponse, SubAuthController, WsConnection};
use futures::future::LocalBoxFuture;
use futures::FutureExt;

pub struct MethodSignup;

pub enum SignupError {
    UsernameTaken,
}

impl From<SignupError> for CustomError {
    fn from(err: SignupError) -> Self {
        match err {
            SignupError::UsernameTaken => {
                CustomError::new(ErrorCode::CONFLICT)
                    .with_message("username taken")
                    .with_kind("UsernameTaken")
            }
        }
    }
}

impl SubAuthController for MethodSignup {
    type Request = SignupRequest;
    type Error = SignupError;

    fn auth(
        self: Arc<Self>,
        _toolbox: &ArcToolbox,
        req: SignupRequest,
        _ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, AuthResponse<SignupRequest, SignupError>> {
        async move {
            let user = create_user(req).await.map_err(HandlerError::internal)?;
            conn.set_user_id(user.id);
            conn.set_roles(Arc::new(vec![user.role as u32]));
            Ok(SignupResponse { user_id: user.id })
        }
        .boxed_local()
    }
}
```

### MCP (Model Context Protocol) support

The WebSocket server can optionally expose every registered endpoint as an
**MCP tool** over JSON-RPC 2.0, alongside the legacy `{method, seq, params}`
protocol. MCP is **off by default** and fully additive: with it disabled the
server behaves exactly as before, and even with it enabled, legacy frames are
routed unchanged, so both protocols work on the same connection. Set
`WsServerConfig::mcp_only` to `true` to reject non-MCP frames. MCP-only mode
requires `enable_mcp` and fails validation at startup without it.

Supported MCP methods: `initialize`, `ping`, `tools/list` (filtered by the
connection's roles), `tools/call`, and `notifications/*`. Tool metadata
(`inputSchema` / `outputSchema`) is generated from the endpoint schemas via
`Type::to_json_schema` (`model::json_schema`, available under the default
`types` feature).

```rust
use endpoint_libs::libs::ws::mcp::McpServerInfo;
use endpoint_libs::model::TypeRegistry;

let mut server = WebsocketServer::new(config);
server.set_auth_controller(MyAuthController);
server.add_handler(MethodEcho);
// ... all other add_handler() calls ...

// With endpoint-gen generated code, use the generated `type_registry()`.
// Endpoints that only use primitive types can pass an empty registry.
let registry: TypeRegistry = type_registry();
server.enable_mcp(
    &registry,
    McpServerInfo { name: "my-service".into(), version: env!("CARGO_PKG_VERSION").into() },
)?;

server.listen().await
```

Behavior notes:

- **Frame detection** — a frame is treated as JSON-RPC iff it carries a
  top-level `"jsonrpc": "2.0"` member, which legacy frames can never contain.
- **Tool names** — endpoint names in snake_case (`UserListSymbols` →
  `user_list_symbols`).
- **Roles** — `tools/list` only shows tools the connection's roles allow;
  calling a forbidden tool answers identically to an unknown tool.
- **Errors** — public handler errors (`CustomError`) become MCP tool results
  with `isError: true`; invalid params map to `-32602`, unknown methods to
  `-32601`, internal errors to `-32603` (with `logId` in `error.data`).
- **Streaming** — endpoints with a `stream_response` deliver only their
  immediate response over MCP; stream frames are not forwarded (tools are
  annotated accordingly in their description).
- `enable_mcp` fails at startup on unresolved `StructRef`/`EnumRef` names or
  duplicate tool names, rather than serving broken schemas.

A runnable end-to-end example (MCP handshake + legacy frame on one
connection) is provided:

```sh
cargo run --example mcp_echo --features ws-http1
```

Migrating an existing backend from 1.7.x? See the step-by-step guide in
[docs/mcp-migration.md](docs/mcp-migration.md) (covers the typed-error
migration, RON descriptions, codegen, activation, and verification), and
[pathscale/api.support.cafe#3](https://github.com/pathscale/api.support.cafe/pull/3)
for a complete worked example.

### Machine-readable descriptions of your API

Three of these, serving different audiences. They are **parallel outputs, not a
progression** — nothing here deprecates anything else:

| Artifact | Always emitted? | Audience |
|---|---|---|
| `docs/services.json` | **yes** | Internal tooling. Our own format, our own rules. |
| `docs/<service>_mcp_tools.json` | **yes** | Review — what a server reports via `tools/list`. |
| `docs/asyncapi.json` | opt-in (`--asyncapi`) | External consumers who want a standard. |
| `docs/openapi.json` | opt-in (`--openapi`) | OpenAPI tooling — clients, doc renderers, bridges. |

**`services.json` is the one to build internal tooling against.** It is always written,
it is a format we define and control, and it changes when we decide it changes — no
specification committee, no version negotiation, no vocabulary that almost fits. Shape:

```json
{ "services": [ { "name": "userApi", "id": 1,
                  "endpoints": [ { "name": "...", "code": 10000, "description": "...",
                                   "parameters": [...], "returns": [...], "errors": [...],
                                   "roles": [...], "stream_response": null } ] } ],
  "enums": [...], "structs": [...] }
```

Note it contains **only `frontend_facing` endpoints**, by design — it is the
public-surface view. The AsyncAPI document defaults to every endpoint unless you pass
`--public-only`.

Reach for AsyncAPI when something *outside* your control needs to read the protocol and a
bespoke format would be friction — a third-party integrator, a code generator, a
standards-shaped toolchain. Inside our own stack, `services.json` is less friction, and
that is the right trade.

### API specification documents (2.1)

`model::api_document` turns the endpoint model into document-scope JSON Schema, shared by
every emitter so there is one implementation rather than three drifting copies:

- `SchemaComponents::collect` walks a set of endpoints with one shared `defs` map, so every
  referenced struct and enum is emitted exactly once and operations share `$ref`s.
- `relocate_refs` moves `#/$defs/X` to wherever a given format keeps its definitions
  (`#/components/schemas/X` for both OpenAPI and AsyncAPI). Idempotent, and it only touches
  strings under a `$ref` key.
- `apply_meta` carries `Field.meta` / `EndpointSchema.meta` annotations through: `x-` keys
  verbatim as specification extensions, a fixed list of JSON Schema keywords verbatim, and
  anything else a hard error naming the endpoint and field. A typo'd `exmaple` that silently
  vanished would be invisible until someone read the spec and believed it.

`endpoint-gen` uses this to emit **OpenAPI 3.1** and **AsyncAPI 3.0** documents, both
opt-in (`--openapi`, `--asyncapi`).

> **The OpenAPI document is a projection for tooling, not a servable API.** This transport
> has no URLs, so paths are synthesized as `/{serviceName}/{endpoint_snake_name}`. Point an
> HTTP client at them and nothing will answer. The **AsyncAPI** document is the
> authoritative one *of the two specification documents* — including the `framed_json` byte layout
> under `x-framing`, which is the only machine-readable copy of that format.

MCP tool schemas deliberately do **not** go through this path: `to_mcp_input_schema` and
`to_mcp_output_schema` keep their own self-contained `$defs` so each tool schema stands
alone, which consumers depend on. A test asserts that stays true.

### `database`

Pooled PostgreSQL access: `tokio-postgres` behind `deadpool`, a background data thread,
and `postgres-from-row` mapping.

### `ws-http1` / `ws-tls12`

Narrowing options on `ws`. `ws-http1` adds HTTP/1.1 upgrade support alongside HTTP/2;
`ws-tls12` accepts TLS 1.2 in addition to 1.3. Default is HTTP/2 and TLS 1.3 only.

### `s3-sync`

Forwards to `cert-provider/s3-sync`, for certificate material synced from S3.

### `full`

`types` + `ws` + `database` + `signal` + `scheduler` + `log_reader` +
`error_aggregation` + `log_throttling` + `ws-http1` + `ws-tls12`. Convenience only, and it
does **not** include `ws-client` or `framed-transport` — prefer naming what you use.

### `signal`

Unix signal handling (`SIGTERM`/`SIGINT`) with a global `CancellationToken` for coordinating graceful shutdown across async tasks.

### `scheduler`

Task scheduling utilities built on `tokio-cron-scheduler`:

- Fixed-interval repeated jobs
- `AdaptiveJob` — jobs whose interval can be changed at runtime via a `JobTrigger` handle

### `log_reader`

Utilities for reading and parsing structured log files, including reverse-line iteration for reading recent entries efficiently.

### `error_aggregation`

A `tracing` layer that captures recent error-level log events into an in-memory container, allowing them to be queried programmatically (e.g. to expose recent errors via an API endpoint).

### `log_throttling`

> **Do not use.** This feature is currently non-functional and is excluded from CI. It is present for future development only.

Rate-limiting layer for `tracing` events to suppress repeated log spam.

## Transports (2.0)

The server core is transport-agnostic. The WebSocket path (`listen()`) is unchanged;
these entry points let the same handlers, roles, typed errors and MCP surface run over
a Unix socket, a Windows named pipe, or macOS XPC.

```rust
// Server: one already-established connection, any transport.
server.serve_connection(peer, states, stream, /* auth token */ None).await;

// Server: accept loop over any listener.
server.serve_with(my_listener).await?;   // my_listener: SessionListener

// Client: the mirror image.
let client = WsClient::from_stream(stream);
```

Both sides need a `MessageStream`. For byte-stream transports, the `framed-transport`
feature supplies one:

```rust
use endpoint_libs::libs::ws::transport::{TransportStream, framed_json};

let stream: Box<dyn MessageStream> =
    Box::new(TransportStream::new(framed_json(unix_stream)));
```

`examples/uds_echo.rs` is a complete worked example over a Unix domain socket:

```bash
cargo run --example uds_echo --features full,framed-transport,ws-client
```

### `framed_json` wire format

One length-delimited frame per message — implementable by a non-Rust peer in a few
lines:

```text
+---------------+--------+--------------------------+
| u32 BE length | u8 kind| payload (length-1 bytes) |
+---------------+--------+--------------------------+
```

`length` counts the kind byte plus payload. `kind` is `0=Text, 1=Binary, 2=Ping,
3=Pong, 4=Close`. `Text` is UTF-8; `Close` is empty or `u16 BE code` + UTF-8 reason.
Default max frame is 16 MiB (`framed_json_with_max_frame` to change it).

### Peer identity and attestation

`WsConnection.peer` / `RequestContext.peer` carry a `PeerIdentity`:
`Network(SocketAddr)` for TCP/TLS, or `Local(LocalPeer { pid, uid, attestation })`.
`Attestation::Verified { mechanism, subject }` records *code* identity a transport
verified — an XPC code-signing requirement, an executable digest, a SID. This crate
defines the vocabulary; the platform implementations live in a sibling crate.

### Hooks

`BeforeRequest` (may reject and may attach claims to `ctx.extensions`), `AfterRequest`
(observes outcomes), `OnConnect` (refuses a peer once, rather than per request), and
`OnDisconnect` (cleans up connection-owned work after the peer leaves). Request hooks
run on both the legacy and MCP dispatch paths unless MCP-only mode rejects legacy
frames first.

```rust
server.add_before_hook(MyMissionTokenCheck);
server.add_on_connect_hook(RefuseUnattestedPeers);
server.add_on_disconnect_hook(CleanUpConnectionWork);
```

> **Note:** `MessageStream`'s futures are not `Send`, so `serve_connection`,
> `serve_with` and a `from_stream` client must run inside a `tokio::task::LocalSet`.

## Logging Setup

The `setup_logging` function (available without any optional features) provides a batteries-included `tracing` subscriber with:

- Stdout logging with thread names and line numbers
- Optional file logging with configurable rotation
- Runtime log level reloading via `LogReloadHandle`
- Optional `error_aggregation` layer (requires `error_aggregation` feature)

## OpenTelemetry (OTel) Integration

The logging framework supports forwarding all `tracing` spans and log events to an OpenTelemetry (OTLP) collector as primary signals (**Traces** and **Logs**). This operates as a parallel layer and does not affect stdout or file logging.

### Enabling OTel

To enable OTLP forwarding, configure `OtelConfig` in your `LoggingConfig`:

```rust
use std::collections::HashMap;
use endpoint_libs::libs::log::{LoggingConfig, OtelConfig, OtelProtocol};

let mut headers = HashMap::new();
headers.insert("x-api-key".to_string(), "your-token".to_string());

let config = LoggingConfig {
    otel_config: OtelConfig {
        enabled: true,
        service_name: Some("my-service".into()),
        endpoint: Some("http://localhost:4317".into()), // OTLP collector endpoint
        protocol: OtelProtocol::Grpc,
        headers,
    },
    ..Default::default()
};

let setup = setup_logging(config)?;
// CRITICAL: Keep `setup.otel_guards` alive for the duration of the program.
// It flushes pending traces and logs to the collector on drop.
```

### Environment Variables

OTel can also be configured via standard environment variables:

| Variable | Description |
|----------|-------------|
| `OTEL_SERVICE_NAME` | Name of the service |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Collector endpoint for traces |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | Collector endpoint for logs (falls back to traces endpoint) |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` or `http/protobuf` |
| `OTEL_EXPORTER_OTLP_HEADERS` | Key-value pairs for auth (e.g. `api-key=val,other=val`) |
| `OTEL_PROPAGATORS` | Context propagators (default: `tracecontext,baggage`) |

Note: Values in `OtelConfig` override environment variables. To prevent recursive logging, OTel internal crates are capped at the `WARN` level when global logging is set to `DEBUG` or `TRACE`.

## Config Loading

A `load_config` utility parses a JSON config file, defaulting to `etc/config.json` or overridden via `--config`/`CONFIG` env var. Supports an optional `--config-entry` for selecting a sub-key within the config object.

---

## Releasing

Releases are managed with [`cargo-release`](https://github.com/crate-ci/cargo-release) and [`git-cliff`](https://github.com/orhun/git-cliff). Both must be installed:

```sh
cargo install cargo-release git-cliff
```

To cut a release:

```sh
./scripts/release.sh [--skip-bump] <patch|minor|major>
```

The script will:
1. Run `cargo release --execute <level>` — bumps the version in `Cargo.toml`, updates the deps.rs badge in this README, regenerates `CHANGELOG.md`, and commits everything as `chore(release): vX.Y.Z`.
2. Open your `$EDITOR` with the auto-generated tag notes (from `git-cliff`) for review.
3. Create an annotated tag using the edited notes as the tag body (shown as GitHub Release notes).
4. Push the commit and tag.
5. Prompt whether to publish to crates.io.

To preview what `cargo-release` would do without making changes:

```sh
cargo release patch  # omit --execute for a dry run
```
