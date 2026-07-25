# endpoint-libs 2.0 — implementation plan (brief for Claude Code)

Repo: `~/code/endpoint-libs` (currently 1.9.1, edition 2024). Goal of 2.0: make the
schema/handler/MCP machinery **transport-agnostic** so the same server core runs over
TCP+TLS+WebSocket (today's path, unchanged behavior) *and* over local attested
transports (Unix sockets, Windows named pipes, macOS XPC) implemented later in a
sibling crate. Three designs are deliberately stolen from tarpc 0.37 (MIT): its
`Transport` trait shape, its `request_hook` Before/After pattern, and its
`serde_transport` framing stack. Do NOT add tarpc as a dependency.

Ground rules:

- Existing consumers that only use `add_handler` + `listen()` + `RequestHandler` must
  compile with zero or trivial changes. Breaking changes are confined to the types
  named in §3 (peer identity) and §3b (schema model) — nowhere else.
- No new required dependencies in default features. New deps (`tokio-util`,
  `tokio-serde`) go behind a new feature flag.
- Every phase lands green. **NB (corrected against the repo):** `cargo test
  --all-features` can never pass — `ws` and `ws-wtx` are mutually exclusive via
  `compile_error!` guards (`src/lib.rs:4`). CI uses `cargo all-features test`, which
  respects the `cargo-all-features` denylist in `Cargo.toml:230`. Use that, or the
  explicit matrix in §9. Baseline on `main` at 1.9.1: `--features full` = 65 tests
  green.
- The `ws-wtx` backend is **dead code** — `compile_error!` deprecation
  (`src/libs/ws/wtx.rs:1`), denylisted, and no longer implements the current
  `WsUpgrader` trait. **Decision: leave it untouched.** Phase 1 does NOT add
  `WireMessage` conversions for it (the original §2 said to; that was written before
  the rot was known). Do not "fix" it in passing.
- Work in phases, one PR-sized commit series each, in the order given. Phase 1+2 are
  this brief's scope; §7 defines contracts a sibling crate implements later — expose
  the traits, don't implement platform code here.

---

## 1. The stolen pieces (reference shapes)

### 1a. tarpc's Transport: a blanket alias over Sink + Stream

```rust
// tarpc/src/transport.rs (shape to replicate, adapted names)
pub trait Transport<SinkItem, Item>
where
    Self: Stream<Item = Result<Item, <Self as Sink<SinkItem>>::Error>>,
    Self: Sink<SinkItem, Error = <Self as Transport<SinkItem, Item>>::TransportError>,
{
    type TransportError: std::error::Error + Send + Sync + 'static;
}
impl<T, SinkItem, Item, E> Transport<SinkItem, Item> for T
where
    T: ?Sized + Stream<Item = Result<Item, E>> + Sink<SinkItem, Error = E>,
    E: std::error::Error + Send + Sync + 'static,
{ type TransportError = E; }
```

Key property: implementors never name the trait — anything that is `Sink + Stream` of
the right item types *is* a transport. This composes with `tokio_util::codec::Framed`,
`tokio_serde`, and hand-rolled adapters (XPC) for free.

### 1b. tarpc's serde_transport: framing = LengthDelimitedCodec + serde codec

```rust
// shape: Framed<S, LengthDelimitedCodec> wrapped by tokio_serde::Framed
// endpoint-libs version: serde_json value = one length-delimited frame
pub fn framed<S: AsyncRead + AsyncWrite>(io: S) -> impl Transport<WireMessage, WireMessage>
```

### 1c. tarpc's request_hook: BeforeRequest / AfterRequest

Hooks run before execution (may reject with a typed error, may mutate context) and
after completion (observe result). This is the seam where mission-token verification
will plug in without the transport or handler layers knowing about it.

---

## 2. Phase 1 — `WireMessage` inversion (non-breaking)

**Problem:** with feature `ws` on, `WsMessage` is a re-export of
`tokio_tungstenite::tungstenite::Message`, so a backend type leaks through session,
toolbox, subs, push, and conn. The `#[cfg(not(feature = "ws"))] mod inner` enum in
`src/libs/ws/message.rs` is already the shape we want.

**Change:**

- Promote the inner enum to the unconditional canonical type, rename `WireMessage`
  (keep `pub type WsMessage = WireMessage;` alias for compat). Variants: `Text(String)`,
  `Binary(Vec<u8>)`, `Ping(Vec<u8>)`, `Pong(Vec<u8>)`, `Close(Option<CloseFrame>)`.
- Add `From<WireMessage> for tungstenite::Message` and the reverse, gated on the
  tungstenite backend. Conversions live in the tungstenite backend module only.
  **Not the wtx backend** — it is dead code, see ground rules.
- Sweep: session.rs, toolbox.rs, subs.rs, push.rs, conn.rs, server.rs **and client.rs**
  use `WireMessage` exclusively. Grep for `tungstenite::Message` outside the two backend
  dirs → must be zero.
  - `client.rs` is easy to miss and is a real leak site: `WsClient::stream_send`
    (`client.rs:108`) takes tungstenite's `Message`, and the private `enum WsStream`
    (`client.rs:55`) holds tungstenite stream types. Convert the helpers to
    `WireMessage` now — Phase 3 adds a local-transport client constructor on top of
    this, and doing the sweep later means re-opening and re-testing the same paths.
- The `pub type WsMessage = WireMessage;` alias covers *type positions only*. It does
  **not** preserve tungstenite's inherent methods (`.into_text()`, `.is_close()`,
  `.into_data()`). Grep consumers for those call sites and list them in the migration
  doc individually — "the `From` impls cover most uses" is not true for method calls.

**Acceptance:** all tests/examples pass with `--features full`; `ws-core` builds
without tungstenite in the tree (`cargo tree -e features` proof).

## 3. Phase 2 — `PeerIdentity` (breaking change 1 of 2; see also §3b)

New module `src/libs/peer.rs`:

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PeerIdentity {
    /// TCP/TLS network peer (today's behavior).
    Network(SocketAddr),
    /// Same-machine peer over a local transport.
    Local(LocalPeer),
    Unknown,
}

#[derive(Debug, Clone)]
pub struct LocalPeer {
    pub pid: Option<u32>,
    pub uid: Option<u32>,          // unix only; None on Windows
    pub attestation: Attestation,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Attestation {
    /// Transport did not verify code identity.
    None,
    /// OS/kernel or transport-level verification succeeded.
    Verified {
        /// e.g. "xpc-codesign-requirement", "pidfd-exe-sha256", "pipe-sid-dacl"
        mechanism: &'static str,
        /// e.g. the requirement string, digest, or SID that matched
        subject: String,
    },
}

impl PeerIdentity {
    /// Best-effort IP for logging; loopback for local peers.
    pub fn ip_addr(&self) -> IpAddr { ... }
}
```

**Threading it through (breaking):**

- `WsConnection.address: SocketAddr` → `WsConnection.peer: PeerIdentity`. Add
  `pub fn address(&self) -> SocketAddr` compat accessor (Network addr, else
  `127.0.0.1:0`), mark `#[deprecated(note = "use .peer")]`.
- `RequestContext`: keep `ip_addr` field **populated from `peer.ip_addr()`** (most
  consumers only log it — they keep compiling), add `pub peer: PeerIdentity`.
- `WsConnection` and `RequestContext` gain `extensions: Extensions` — implement a
  minimal typed map (`anymap` pattern over `HashMap<TypeId, Box<dyn Any + Send + Sync>>`,
  ~60 lines, no new dep; model after `http::Extensions`). Connection-scoped extensions
  carry the attestation; request-scoped extensions will carry verified mission claims.
- Logging call sites that format `?addr` switch to `%conn.peer_display()` (add a
  compact Display).

**Acceptance:** compile + tests; a grep inventory of every `ip_addr`/`address` consumer
goes in the PR description (there are ~15 sites: server.rs, session.rs, toolbox
logging, headers.rs real-IP handling — the header-derived real-IP override must keep
working for the Network variant).

## 3b. Phase 2b — schema-model future-proofing (breaking, ~20 lines, do it with Phase 2)

**Why this is in 2.0 even though nothing uses it yet.** OpenAPI/AsyncAPI emission is a
2.1 feature living in *endpointgen* (see `PLAN-2.1.md`). It touches no runtime code and
is purely additive — **provided** the schema model can absorb new information without a
breaking change. Today it cannot. This phase buys that, and it is only free while 2.0 is
already breaking. Skipping it means OpenAPI enrichment forces a 3.0.

**What already works — protect it.** Generated consumer code does *not* struct-literal
schema types; endpointgen emits `serde_json::from_str(schema).unwrap()`
(`endpointgen/src/rust.rs:513`). Therefore **adding a field to `EndpointSchema` is
already non-breaking for generated code, as long as it carries `#[serde(default)]`.**
Record this as a load-bearing invariant in the module docs so nobody "optimises" the
generated schema into a struct literal later.

**Changes in `src/model/`:**

- `#[non_exhaustive]` on `Type` (`model/types.rs:82`), `Field` (`types.rs:5`),
  `EnumVariant` (`types.rs:44`), `EndpointSchema` (`model/endpoint.rs:11`), and
  `EndpointErrorSchema`. `Type` is the critical one: it is a plain public enum, so any
  future variant (a decimal with precision/scale, a constrained string, a format
  carrier) is breaking without this.
- Add a forward-compatible metadata slot, populated by nobody in 2.0:

```rust
// on both Field and EndpointSchema
/// Emitter-facing annotations (examples, constraints, tags, deprecation,
/// protocol-binding hints). Unused in 2.0; consumed by the OpenAPI/AsyncAPI
/// emitters in 2.1. Unknown keys must round-trip untouched.
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub meta: BTreeMap<String, serde_json::Value>,
```

- Because `Field`/`EndpointSchema`/`EnumVariant` already have `new*` constructors, keep
  them as the supported construction path and add `with_meta`.

**Known fallout, fix in the same lockstep release:** `#[non_exhaustive]` forbids struct
literals *outside* the defining crate, which breaks endpointgen's
`impl From<EndpointSchemaElement> for EndpointSchema`
(`endpointgen/src/definitions.rs:430`). One-line fix — switch it to
`EndpointSchema::new(...)` plus setters. Nothing in consumer projects constructs these
by literal, so the blast radius is endpointgen only.

**Acceptance:** `cargo test --all-features`; a round-trip test proving an
`EndpointSchema` JSON blob containing an *unknown* future field deserializes, and that
a populated `meta` map survives serialize → deserialize unchanged. That test is the
contract 2.1 depends on.

## 4. Phase 3 — transport seam

New module `src/libs/transport.rs` (in `ws-core`):

```rust
/// STOLEN SHAPE (tarpc): blanket transport alias.
pub trait Transport<SinkItem, Item>: /* as §1a, items = WireMessage */ { ... }

/// Object-safe session-facing stream. The existing `WsStream` trait keeps this
/// exact API; rename to `MessageStream` with `pub use ... as WsStream` alias.
#[async_trait(?Send)]
pub trait MessageStream: Unpin + Send {
    async fn send(&mut self, msg: WireMessage) -> Result<(), StreamError>;
    async fn recv(&mut self) -> Option<Result<WireMessage, StreamError>>;
}

/// Adapter: any Sink+Stream transport is a MessageStream.
pub struct TransportStream<T>(pub T);
#[async_trait(?Send)]
impl<T> MessageStream for TransportStream<T>
where T: Transport<WireMessage, WireMessage> + Unpin + Send { ... }
```

New module `src/libs/transport/framed.rs` behind new feature
`framed-transport = ["ws-core", "dep:tokio-util", "dep:tokio-serde"]`:

```rust
/// STOLEN SHAPE (tarpc serde_transport): length-delimited JSON frames over any
/// byte stream. Text frames only (the legacy protocol is JSON either way);
/// Ping/Pong map to empty control frames w/ a 1-byte tag; Close = clean EOF.
pub fn framed_json<S>(io: S) -> impl Transport<WireMessage, WireMessage>
where S: AsyncRead + AsyncWrite + Unpin + Send + 'static;
```

Frame format: `u32 BE length | u8 kind (0=Text,1=Binary,2=Ping,3=Pong,4=Close) | payload`.
Configure `LengthDelimitedCodec` max frame length from a parameter (default 16 MiB).
Document the format in the module docs — the sibling crate and any non-Rust peer must
be able to implement it.

**Server entry seam** in `server.rs`:

```rust
impl WebsocketServer {
    /// Public, transport-agnostic entry: runs auth + session for one
    /// already-established connection. Generalizes post_upgrade_connection.
    pub async fn serve_connection(
        self: Arc<Self>,
        peer: PeerIdentity,
        states: Arc<WebsocketStates>,
        stream: Box<dyn MessageStream>,
        auth_protocol: Option<String>,   // WS subprotocol today; token handoff for local
    );

    /// Accept loop over any listener (see trait below). `listen()` becomes a
    /// thin wrapper: TCP/TLS accept + HTTP upgrade → serve_connection.
    pub async fn serve_with<L: SessionListener>(self, listener: L) -> Result<()>;
}

#[async_trait]
pub trait SessionListener: Send + Sync + 'static {
    async fn accept(&self) -> Result<(Box<dyn MessageStream>, PeerIdentity)>;
}
```

**Client-side seam (same shape, do it here — AgentOne is a client):**

```rust
impl WsClient {
    /// Transport-agnostic constructor: drive the existing request/reply +
    /// seq-correlation logic over any MessageStream. Mirrors serve_connection.
    pub fn from_stream(stream: Box<dyn MessageStream>) -> Self;
}
```

The private `enum WsStream` (`client.rs:55`) gains a `Message(Box<dyn MessageStream>)`
variant. Both changes are **additive and non-breaking** (the enum is private,
`WsClient::new` is untouched), so this could technically wait — do it now anyway:
Phase 1 already rewrites these exact functions, the A0↔A1 sidecar needs a client over
XPC/UDS/pipe, and the alternative is the sibling crate reimplementing seq correlation,
MCP framing and reconnect. Ship `from_stream` even though no local transport exists yet
— the duplex acceptance test below exercises it.

Notes:

- `post_upgrade_connection` refactors to call `serve_connection`; the
  upgrader/H1/H2/TLS/shard machinery stays exactly where it is, feeding the same
  entry. The shard model is a property of `listen()`, not of `serve_with` — document
  that `serve_with` runs single-runtime (fine for local IPC's connection counts).
- **`MessageStream` is `#[async_trait(?Send)]`**, so its futures are not `Send` and
  `serve_with`/`serve_connection` must run inside a `LocalSet` (consistent with the
  existing `spawn_local` dispatch). State this explicitly in the module docs and in
  §7's contract — otherwise sibling-crate authors will discover it via a confusing
  `Send` bound error at integration time.
- `AuthController::auth` currently receives the WS subprotocol string; local
  transports pass their token via `auth_protocol` — no trait change needed. Document it.

**Acceptance test (the proof of the whole seam):** an in-process test using
`tokio::io::duplex` + `framed_json` + `serve_connection` that (a) round-trips a legacy
`{method,seq,params}` request through a real registered handler, (b) completes an MCP
`initialize` → `tools/call` on the same connection, (c) never touches a TCP socket,
and (d) drives the client half through `WsClient::from_stream` on the other end of the
duplex — so both seams are proven by the same test. This test is the definition of done
for 2.0's core claim.

## 5. Phase 4 — request hooks (STOLEN SHAPE: tarpc request_hook)

New module `src/libs/hooks.rs` (ws-core):

```rust
#[async_trait(?Send)]
pub trait BeforeRequest: Send + Sync {
    /// Runs after role-check, before dispatch. Err → typed error to client,
    /// handler never runs. May mutate ctx (e.g. insert verified claims into
    /// ctx.extensions).
    async fn before(
        &self,
        ctx: &mut RequestContext,
        endpoint: &EndpointSchema,
        params: &serde_json::Value,
    ) -> Result<(), CustomError>;
}

#[async_trait(?Send)]
pub trait AfterRequest: Send + Sync {
    async fn after(&self, ctx: &RequestContext, endpoint: &EndpointSchema,
                   outcome: &RequestOutcome);  // Ok / PublicErr(code) / InternalErr
}

impl WebsocketServer {
    pub fn add_before_hook(&mut self, hook: impl BeforeRequest + 'static);
    pub fn add_after_hook(&mut self, hook: impl AfterRequest + 'static);
}
```

Wiring — this is the subtle part, get both paths:

- `session.rs::handle_message` (legacy path): after `check_roles`, before
  `spawn_local(handler.handle(...))`. Hooks run inside the spawned task (they're
  async; don't block the session loop). On `Err(custom)`, emit via the existing
  `WsResponseError` shape with the hook's code/params.
- `session.rs::handle_mcp_frame` → `McpAction::ToolCall` path: same placement inside
  the spawned task, error emitted as MCP tool error (`encode_tool_error`), mirroring
  `HandlerError::Public` handling in `handler.rs::handle_mcp`.
- Hooks execute in registration order; first error short-circuits. Store as
  `Vec<Arc<dyn BeforeRequest>>` on the server, snapshot into the spawned task.

**Also add `OnConnect` while you are here (cheap, additive):**

```rust
#[async_trait]
pub trait OnConnect: Send + Sync {
    /// Runs after auth, before the session loop. Err → connection refused,
    /// no messages exchanged. May populate connection-scoped extensions.
    async fn on_connect(&self, peer: &PeerIdentity, ext: &mut Extensions)
        -> Result<(), CustomError>;
}
```

Strictly additive (new trait + new register method), so it is not a 2.0-or-never item —
but it is ~30 lines next to the hooks you are already writing, and without it every
`BeforeRequest` has to re-read attestation from extensions per request instead of
rejecting an unattested peer once at connect. The XPC spike's finding that rejected
peers are invisible to the listener process makes this the natural place for
attestation telemetry.

**Acceptance:** test registering a hook that denies method X with a custom code —
assert the exact error frame on the legacy path AND the MCP path; test a hook that
inserts a claim into `ctx.extensions` and a handler that reads it.

## 6. Phase 5 — release mechanics

- Version `2.0.0-alpha.1`; CHANGELOG section listing the four breaking items
  (`WsConnection.address`; `WsMessage` no longer tungstenite's type — `From` impls
  cover type positions but *not* inherent method calls, see §2; `WsStream` renamed
  w/ alias; the model types now `#[non_exhaustive]` per §3b) and the compat shims.
- `docs/2.0-migration.md`: per-symbol table (old → new → action), modeled on the
  existing mcp-migration.md.
- README: new "Transports" section — framed_json format spec, `serve_with`/
  `serve_connection` examples, one worked UDS example (`examples/uds_echo.rs`, unix
  cfg, plain UnixListener + framed_json — no attestation; ~40 lines) proving the seam
  end-to-end on a real OS transport.
- endpointgen lockstep (separate repo — note it, don't do it here):
  1. Bump `ENDPOINT_LIBS_REQUIREMENT` to `2.0`.
  2. Fix the `EndpointSchema` struct literal broken by Phase 2b's `#[non_exhaustive]`
     (`endpointgen/src/definitions.rs:430` → `EndpointSchema::new(...)` + setters).
  3. **Add a `--check` mode** (borrowed from Oxide's `dropshot-api-manager`
     discipline): regenerate every artifact into a temp dir, diff against what is
     committed, exit non-zero on drift, and wire it into CI. endpointgen today has
     `check_compatibility` for *version* lockstep (`endpointgen/src/main.rs:412`) but
     nothing that catches "someone edited the RON and forgot to regenerate." This is
     independently worth doing for the Rust/docs/MCP artifacts, and it is the
     prerequisite for trusting committed OpenAPI/AsyncAPI documents in 2.1.

## 7. Contracts for the sibling crate (define here, implement later — NOT in this repo)

`endpoint-transport-local` will provide `SessionListener`/`Transport` impls:

| Backend | Rendezvous | Attestation → `Attestation::Verified` |
|---|---|---|
| `uds` (linux) | named socket or inherited socketpair | `SO_PEERPIDFD` + exe SHA-256 (`mechanism: "pidfd-exe-sha256"`); `SO_PEERCRED` fills pid/uid |
| `pipe` (windows) | named pipe (SID DACL, first-instance flag) or inherited handle | DACL admission + impersonation SID check (`"pipe-sid-dacl"`) |
| `xpc` (macos) | private mach service via rama-net-apple-xpc | `PeerSecurityRequirement` (`"xpc-codesign-requirement"`); XpcMessage dict wrapping one `WireMessage` frame per message |

Phase 1–4 must not require ANY change for these to slot in — that's the design test.
If while implementing you find a needed hook that isn't in this plan (e.g. connection-
close callbacks for attestation telemetry), add it in the same style and note it in
the migration doc.

## 8. Explicit non-goals for 2.0

- No platform attestation code in endpoint-libs (sibling crate).
- No change to the wire protocols (legacy JSON frames + MCP are untouched).
- No mission-token semantics (that's a `BeforeRequest` impl in AgencyZero, not here).
- No client-side *feature* work: `WsClient::new` (TCP/TLS) is untouched and no local
  transport ships here. `WsClient::from_stream` **is** in scope (§4) — it is the seam
  only, additive, and the sibling crate supplies the actual transports.
- No OpenAPI/AsyncAPI emission — that is 2.1, in endpointgen, and Phase 2b is what
  keeps it a minor release. See `PLAN-2.1.md`.
- Don't fix the double-serialization TODO in `handler.rs` unless it falls out free.

## 9. Order of work & verification loop

1. Phase 1 (WireMessage, incl. client.rs) → full test suite + both examples.
2. Phase 2 (PeerIdentity) → suite + grep inventory in PR notes.
3. Phase 2b (schema-model future-proofing) → unknown-field round-trip test. Land it
   *with* Phase 2 so all model breakage is in one commit series.
4. Phase 3 (transport seam + `WsClient::from_stream`) → the duplex acceptance test
   (both halves) + uds example.
5. Phase 4 (hooks, incl. `OnConnect`) → both-path hook tests.
6. Phase 5 (docs/release) → `cargo semver-checks` if available; alpha tag.

**Forward-compatibility invariants that must survive 2.0** (2.1 depends on all four —
breaking any of them turns the OpenAPI release into a 3.0):

1. Generated schemas stay JSON-deserialized at runtime, never struct literals.
2. New model fields always carry `#[serde(default)]`.
3. `Type` and the schema structs stay `#[non_exhaustive]`.
4. `Field.meta` / `EndpointSchema.meta` round-trip unknown keys untouched.

After each phase, run this matrix (NOT `--all-features`, see ground rules):

```bash
cargo build --no-default-features --features types
cargo build --features ws-core            # must not pull tungstenite
cargo build --features full
cargo test  --features full               # baseline: 65 tests green at 1.9.1
cargo clippy --all-targets --features full -- -D warnings
cargo all-features test                   # what CI runs; respects the denylist
```
