# Migrating a backend to the MCP surface (endpoint-libs 1.7.x → 1.9.x)

Step-by-step guide for taking an existing endpoint-gen + endpoint-libs backend and exposing every endpoint as an **MCP (Model Context Protocol) tool** over JSON-RPC 2.0, alongside the legacy `{method, seq, params}` protocol.

Worked example: [pathscale/api.support.cafe#3](https://github.com/pathscale/api.support.cafe/pull/3) — a complete migration of a ~25-endpoint backend in one reviewable PR.

## What you get (and what stays the same)

- The WebSocket server treats a frame as JSON-RPC iff it carries a top-level `"jsonrpc": "2.0"` member. Legacy frames can never match, so **both protocols share every connection and legacy clients are completely unaffected**.
- MCP methods served: `initialize` (protocol `2025-06-18`), `ping`, `tools/list`, `tools/call`, `notifications/*`.
- Every endpoint registered via `server.add_handler(...)` becomes a tool named as the snake_case of the endpoint name (`UserListSymbols` → `user_list_symbols`). `inputSchema`/`outputSchema` are generated from the endpoint schemas via `Type::to_json_schema`.
- **Role gating is identical to legacy**: `tools/list` only shows tools the connection's roles allow, and calling a forbidden tool answers exactly like an unknown tool (no existence oracle).
- **Auth needs no changes.** Endpoints registered on an `EndpointAuthController` run at WS-handshake time from the `Sec-WebSocket-Protocol` header (`0<methodname>,1<param1>,2<param2>,...`) and are deliberately *not* tools. MCP clients authenticate the same way every other client does.
- Streaming endpoints (`stream_response: Some(...)`) deliver only their immediate response over MCP; the tool description is annotated automatically.
- Public handler errors (`CustomError`) become tool results with `isError: true`; invalid params map to `-32602`, unknown methods to `-32601`, internal errors to `-32603` (with `logId` in `error.data`).
- No infra changes: MCP rides the existing WebSocket listener. Note the transport is WebSocket — MCP clients that only speak stdio/streamable-HTTP need a thin bridge (out of scope here).

## Version matrix

| Component | Old (typical) | Target |
|---|---|---|
| `endpoint-libs` in `Cargo.toml` | 1.7.x | **≥1.9.0** (keep your existing feature list, e.g. `["ws", "ws-http1"]`) |
| `honey_id-types` (if used) | ≤1.13 | **≥1.14.0** — first release built against the 1.8+ typed-error traits |
| `endpoint-gen` binary and `config/version.toml` `[binary]` | 1.5.x/1.6.x | **≥1.9.0** |
| `config/version.toml` `[libs]` | 1.7.x | match your `endpoint-libs` version |

Heads-up on effort: enabling MCP itself is a one-line change. The real migration cost is that **endpoint-libs 1.8.0 introduced typed handler errors**, and projects coming from 1.7.x must adopt them. Expect roughly three mechanical compile errors per handler file; the patterns in Phase 3 resolve all of them.

## Phase 0 — dependencies

1. Bump `endpoint-libs` (and `honey_id-types` if used) in `Cargo.toml`.
2. Update `config/version.toml` (`[binary]` / `[libs]`).
3. `cargo update -p endpoint-libs -p honey_id-types` — the lockfile must unify on a single endpoint-libs ≥1.9.
4. Install the matching `endpoint-gen` binary so the project's regenerate script finds it on PATH.

## Phase 1 — RON descriptions (this is what makes the tools usable)

The RON schema format is backward compatible (the `description` and `errors` fields are serde-defaulted), but endpoints without `description:` produce tools with empty descriptions — useless to an agent. In every `config/schema_lists/**/*.ron`, inside each `schema: (...)`:

```ron
schema: (
    name: "SendMessage",
    code: 20002,
    description: "Send a message into an existing chat session. The message is relayed to the app's support staff. Staff reply out-of-band, not via this endpoint.",
    ...
)
```

Write descriptions for an LLM consumer: what the endpoint does, who may call it, what ids/parameters mean, and side effects. Add them to auth endpoints too for docs completeness, even though those never appear in `tools/list`.

Notes:
- **`Field`-level descriptions cannot be set from RON** (`Field.description` is `#[serde(skip)]`) — only endpoint descriptions and enum *variant* descriptions (from your enums RON) flow into the generated schemas.
- All standard schema types already map to JSON Schema (`String`, `Boolean`, `Int64`, `TimeStampMs`, `NanoId(len)`, `Optional`, `Vec`, `EnumRef`, `StructRef`, `StructTable`, `UUID`, `BlockchainAddress`, ...). `Optional` fields are excluded from `required`. No type changes are needed.

## Phase 2 — regenerate

Run the project's regenerate script (typically `scripts/utils/regenerate_endpoints.sh`) with the ≥1.9 binary. Expect:

- The generated model now contains `pub const TYPE_DEFINITIONS` and `pub fn type_registry() -> TypeRegistry` — needed for `enable_mcp`.
- `EnumErrorCode` now carries the standard catalog (`BadRequest = 100400`, `Unauthorized = 100401`, `Forbidden = 100403`, ... `InternalError = 100500`) plus `impl From<EnumErrorCode> for ErrorCode`. **If your project had a placeholder variant (e.g. `Xxx = 0`), code referencing it will break — switch to a real variant.**
- New `docs/<service>_mcp_tools.json` files per service: the exact `tools/list` output a server built from these schemas will report. Commit them and review them as the MCP surface; future schema changes show up as diffs there.

## Phase 3 — typed-error migration (the bulk; entirely mechanical)

The 1.9 traits:

```rust
// endpoint_libs::libs::ws::handler
pub trait RequestHandler {
    type Request: WsRequest + 'static;
    type Error: Into<CustomError> + 'static;   // new in 1.8 — no default, must be declared
    async fn handle(&self, ctx: RequestContext, req: Self::Request)
        -> Response<Self::Request, Self::Error>;
}
pub type Response<T, E = CustomError> = Result<T::Response, HandlerError<E>>;
pub enum HandlerError<E> { Public(E), Internal(eyre::Report), NoResponse }
// impl<E> From<E> for HandlerError<E>    → `?` works on your Error type directly
// HandlerResultExt::internal()           → converts Result<_, impl Into<eyre::Report>>
```

`CustomError::new` takes one argument (`impl Into<ErrorCode>`); messages go through the builder: `CustomError::new(code).with_message(msg)` (also `.with_kind(...)`).

### Per handler file

1. Imports:
```rust
use endpoint_libs::libs::toolbox::{CustomError, RequestContext};
use endpoint_libs::libs::ws::handler::{HandlerResultExt, RequestHandler, Response};
use crate::codegen::model::EnumErrorCode; // plus your existing model imports
```
2. Right after `type Request = ...;`:
```rust
    type Error = CustomError;
```
3. Sort error sites into two buckets:

   **Caller-facing conditions → public errors.** These render as clean MCP `isError` results (and structured legacy errors). At minimum do this for auth/authz checks:
```rust
.ok_or_else(|| {
    CustomError::new(EnumErrorCode::Unauthorized)
        .with_message("Connection not authenticated")
})?;

// an authz eyre::bail!(...) becomes:
return Err(CustomError::new(EnumErrorCode::Forbidden)
    .with_message("Session does not belong to this app")
    .into());
```

   **Infrastructure failures → `.internal()`.** Database ops, service calls returning `eyre::Result`, id packing/conversion. This preserves the pre-1.8 behavior (opaque internal error response) exactly:
```rust
self.some_service.do_thing(x).await.internal()?;
let rows = table.select_all().execute().internal()?;           // WorkTableError
.map_err(|e| eyre::eyre!("...: {e:?}")).internal()?;            // keep the eyre mapping, add .internal()
self.user_service.get_my_info(id).internal()                    // tail delegation
```

4. Handlers whose only fallible call already returns `Result<_, CustomError>` need nothing beyond the `type Error` line — `?` converts via `From`.

A workflow that scales: apply steps 1–2 to every file first, then run `cargo check --message-format=short` and fix each remaining `?`-conversion error at its reported line with `.internal()`. Repeat to zero errors, then clear any unused-import warnings.

### `SubAuthController` impls (custom auth endpoints)

The pre-1.8 shape took `param: serde_json::Value` and returned `eyre::Result<Value>`. The current trait is typed and the library does the serde:

```rust
use endpoint_libs::libs::toolbox::{ArcToolbox, CustomError, RequestContext};
use endpoint_libs::libs::ws::{AuthResponse, SubAuthController, WsConnection};
use futures::FutureExt;
use futures::future::LocalBoxFuture;

impl SubAuthController for MethodAppConnect {          // note: no #[async_trait]
    type Request = AppConnectRequest;                  // generated request type
    type Error = CustomError;

    fn auth(
        self: Arc<Self>,
        _toolbox: &ArcToolbox,
        req: AppConnectRequest,                        // already deserialized
        _ctx: RequestContext,
        conn: Arc<WsConnection>,
    ) -> LocalBoxFuture<'static, AuthResponse<Self::Request, Self::Error>> {
        async move {
            // ... registry bookkeeping, conn.set_roles(...) ...
            Ok(AppConnectResponse { /* plain response struct — no to_value */ })
        }
        .boxed_local()
    }
}
```

honey_id-types ≥1.14 ships its handlers (`GenericAuthorizedConnect`, `MethodApiKeyConnect`, `MethodReceive*`) against the new traits — existing wiring typically compiles unchanged.

## Phase 4 — enable MCP

After **all** `add_handler` calls, before `listen()`:

```rust
use endpoint_libs::libs::ws::mcp::McpServerInfo;
use crate::codegen::model::type_registry;

server.enable_mcp(
    &type_registry(),
    McpServerInfo {
        name: "<service-name>".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    },
)?;
```

`enable_mcp` fails at startup on unresolved `StructRef`/`EnumRef` names or duplicate snake_case tool names — misconfiguration surfaces immediately instead of serving broken schemas.

## Phase 5 — verify

1. `cargo check` / `cargo build` clean, including your CI feature set. Zero warnings.
2. Run the server locally and drive it with any WebSocket CLI client (e.g. [websocat](https://github.com/vi/websocat)), authenticating via the subprotocol header:

```sh
{ echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}';
  sleep 0.4; echo '{"jsonrpc":"2.0","method":"notifications/initialized"}';
  sleep 0.4; echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}';
  sleep 0.4; echo '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"<some_tool>","arguments":{}}}';
  sleep 0.4; echo '{"method":<some_legacy_code>,"seq":7,"params":{}}';
  sleep 1; } | websocat -t ws://127.0.0.1:8443 --protocol '0<authmethod>,1<param1>,2<param2>'
```

Assert all of:
- `initialize` returns protocol `2025-06-18` and your `serverInfo`;
- `tools/list` is filtered to the connection's roles and matches `docs/*_mcp_tools.json`;
- a `tools/call` round-trips with `isError: false` and correct `structuredContent`;
- a **legacy frame on the same connection** still answers normally (regression check);
- `tools/call` on a tool the role can't see → `-32602 "Unknown tool"`, byte-identical in shape to a nonexistent name;
- a malformed argument (e.g. wrong `NanoId` length) → `-32602` with a field path.

## Gotchas index

- **The 1.8.0 typed-error change, not MCP, causes nearly all compile errors.** `type Error` has no default; every `RequestHandler` impl must declare it.
- **`CustomError::new` is now one-argument**; message via `.with_message(...)`.
- **Placeholder `EnumErrorCode` variants disappear on regeneration**, replaced by the standard 100400+ catalog. Grep for the placeholder before regenerating.
- **`Field.description` is `#[serde(skip)]`** — field descriptions in RON don't flow through; only endpoint and enum-variant descriptions do.
- **Auth endpoints are not tools by design.** Don't try to expose them; the subprotocol header is the auth path for MCP clients too.
- **`SubAuthController` lost `#[async_trait]`** and its `Value`-based signature.
- Endpoint names must stay unique after snake_casing, or startup fails.
- **Review responses for secrets before merging.** Everything a handler returns now reaches LLM agents verbatim via `structuredContent` — audit list/read endpoints for tokens and credentials.
