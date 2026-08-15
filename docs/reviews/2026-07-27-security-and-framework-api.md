# endpoint-libs Review: Security and Framework API

**Date:** 2026-07-27
**Scope:** `src/libs/ws/**` (session, server, upgrader, headers, toolbox, handler, hooks, mcp, conn, tls, transport, push), `src/libs/{config,utils,peer,database}.rs`, `src/model/{json_schema,types,api_document,endpoint}.rs`, `Cargo.toml` / `Cargo.lock`, `AGENTS.md`, `README.md`, `docs/mcp-migration.md`. Read-only cross-reference of `api.support.cafe`, `nofilter.io-backend`, `pays.online-backend`, `web3.trading-backend`.
**Commit:** `b9f7563`
**Reviewer slice:** security-and-framework-api; sibling slices cover logging/OTel, database, codegen/model emitters, and the client.

Note on versioning: the task brief describes this crate as "1.9+". It is **2.1.3** (`Cargo.toml:3`). The MCP surface and auth-via-header landed in 1.9 and survived the 2.0/2.1 releases essentially unchanged, so the brief's framing still applies, but anyone grepping for "1.9" will be looking at the wrong docs. All four backends pin `endpoint-libs = "2.0"`, which resolves to 2.1.3, so **every finding below is live in api.support.cafe, nofilter.io-backend, pays.online-backend and web3.trading-backend simultaneously**.

## Summary

- **No `unsafe`, no hand-rolled crypto, no SQL string building, no shell-outs from request paths.** The classic Rust-framework hazards are absent. TLS defaults are good (TLS 1.3 only unless `ws-tls12`; the client's `danger_accept_invalid_certs` is opt-in and honestly named). The MCP role-gating is genuinely well built: `tools/list` is role-filtered and a forbidden tool answers byte-identically to a nonexistent one (`src/libs/ws/mcp.rs:366-394`), so there is no existence oracle. Credit where due.
- **The three things to fix, in order.** (1) A failed authentication leaks a permanent entry in the global connection map, remotely and without credentials (`src/libs/ws/server.rs:257-282`). (2) `get_conn_id()` truncates a microsecond timestamp to `u32` (`src/libs/utils.rs:7-9`), and connection id is the routing key for every response, every subscription, and every downstream identity registry. (3) The framework has **no `on_disconnect` hook at all**, which is why api.support.cafe's `UserConnectionRegistry::unregister` exists and is never called from anywhere.
- **The framework has no timeouts, no rate limits, no connection cap and no configurable WebSocket message size.** `rg -i "timeout|rate.?limit|semaphore"` over `src/` returns only database statement timeouts and the log throttler. Every downstream service must remember, and none of them do.
- **The extension surface has one specific hole that generates most of the downstream boilerplate:** `WsConnection.extensions` is a plain owned `Extensions` with no interior mutability (`src/libs/peer.rs:178-181`, `src/libs/ws/basics.rs:57`), so an `AuthController` holding `Arc<WsConnection>` cannot attach the authenticated identity to the connection. Every backend works around it with a side map keyed by connection id. api.support.cafe says so in a comment: `// TODO: remove when elibs will be capable to have xustome context.`
- **Dependency weight is out of proportion.** The `types`-only default feature pulls **246 crates**; `full` pulls 344. `hyper-rustls`, `derive_more` and `tonic` are unconditional dependencies with **zero uses in `src/`**.
- **`AGENTS.md` is actively wrong about what this repo is**, and it is loaded into every agent session by mandate (`CLAUDE.md`). It claims the JS side is "the primary surface"; there are zero `.js`/`.ts` files in the repo.

---

## Findings

### [SEV-1] Failed authentication leaks a connection slot forever, unauthenticated and remotely

- **ID:** `endpoint-libs-security-and-framework-api-01`
- **Severity:** Critical
- **Category:** Security (DoS)
- **Confidence:** High
- **Location:** `src/libs/ws/server.rs:257-282` (leak), `src/libs/ws/server.rs:322` (the only `remove`), `src/libs/ws/conn.rs:33-40`, error paths at `src/libs/ws/headers.rs:203,208,214,217`
- **What:** `serve_connection` registers the connection in the global `WebsocketStates` map *before* running auth:

  ```rust
  let (tx, rx) = mpsc::channel(self.config.message_buffer_size);
  states.insert(conn.connection_id, tx, conn.clone());          // :258

  let auth_result = /* ... */ .auth(...).await;                 // :260-266
  if let Err(err) = auth_result {
      self.toolbox.send_request_error(...);
      error!(...);
      return;                                                    // :281 — no states.remove
  }
  self.handle_session_connection(conn, states, stream, rx).await; // :284
  ```

  `states.remove(...)` lives only in `handle_session_connection` (`:322`), which the error path never reaches. The `OnConnect` refusal at `:232-240` correctly returns before the insert; the auth path does not.
- **Why it matters:** The shipped `EndpointAuthController` returns `Err` on four attacker-controlled inputs, all reachable from an anonymous WebSocket handshake by varying `Sec-WebSocket-Protocol`: missing method (`headers.rs:203`), unknown method (`:208`), unparseable parameter (`:214`, via `parse_ty`), missing required parameter (`:217`). So `websocat --protocol 'x'` in a loop leaks one `Arc<WsConnection>` plus an `Arc<WsStreamState>` plus an `mpsc::Sender` per attempt, permanently, with no credentials. `DashMap` has no eviction and nothing else ever calls `remove` for these ids. At a modest 1000 handshakes/second this is roughly a megabyte per second of unreclaimable heap, growing until OOM. It also poisons `WebsocketStates` with entries whose ids will later be re-issued (see finding 02).
- **Fix:** Mechanical. Move the insert after auth, or add the removal to the error path. Preferred, because it also fixes the `OnConnect` ordering question, is to make the slot RAII-scoped:

  ```rust
  struct StateGuard(Arc<WebsocketStates>, ConnectionId);
  impl Drop for StateGuard { fn drop(&mut self) { self.0.remove(self.1); } }
  ```

  Hold the guard for the whole of `serve_connection` and let `handle_session_connection` stop calling `remove` itself. That closes this path and any future early return.
- **Effort:** S
- **Blast radius:** `src/libs/ws/server.rs` only; no public API change. Fixes all four backends on a version bump.

### [SEV-2] `get_conn_id()` truncates a microsecond timestamp to `u32`; ids collide and are the routing key for everything

- **ID:** `endpoint-libs-security-and-framework-api-02`
- **Severity:** Critical
- **Category:** Security (cross-user data exposure) / Correctness
- **Confidence:** High on the collision; Medium on the worst-case exploitation path (needs a human to confirm timing in production)
- **Location:** `src/libs/utils.rs:7-9`; consumers at `src/libs/ws/toolbox.rs:171-185` and `:193-207`, `src/libs/ws/conn.rs:33-40`, `src/libs/ws/push.rs:55`, `api.support.cafe/src/service/user_connection_registry.rs:21-30`
- **What:**

  ```rust
  pub fn get_conn_id() -> u32 {
      chrono::Utc::now().timestamp_micros() as _
  }
  ```

  `timestamp_micros()` is `i64`; `as u32` keeps the low 32 bits. Two connections receive the same id when their microsecond timestamps differ by a multiple of 2^32 µs, i.e. **exactly every 71.58 minutes**, or when they arrive in the same microsecond. There is no uniqueness check and no retry.
- **Why it matters:** `ConnectionId` is not a log tag, it is the addressing primitive. `Toolbox::send` looks the id up in the `DashMap` and writes the response to whatever `message_queue` it finds (`toolbox.rs:171-185`). `WebsocketStates::insert` is an unconditional `DashMap::insert`, so a colliding new connection silently **replaces** the older connection's queue. Three consequences, in ascending order of seriousness:
  1. The older connection becomes unroutable (its responses vanish).
  2. When the older session eventually ends it calls `states.remove(id)` (`server.rs:322`) and evicts the *newer*, live connection.
  3. In the window between, handler responses for the old connection are delivered to the new one. WebSocket connections here are long-lived by design (that is the whole model), so connections older than 71.58 minutes are the normal case, not the edge case.

  Downstream makes this worse rather than better. api.support.cafe authorizes off connection id: `MethodListApps` resolves the caller's identity purely from `user_connection_registry.get(ctx.connection_id)` and then branches on `is_platform_admin(user_pub_id)` (`api.support.cafe/src/handlers/app_admin/list_apps.rs:29-40`). The registry is a `HashMap<u32, UserPublicId>` whose `unregister` is never called by anything (see finding 04), so entries persist indefinitely. A new connection inheriting a stale id inherits that user's identity, including platform-admin. The same pattern repeats across 26 files in `api.support.cafe/src/handlers/`. `SubscribeManager` (`push.rs:55`) keys subscribers the same way, so a new connection can inherit another user's stream subscription.
- **Fix:** Mechanical for the framework, and it is the right fix regardless of the exploitation odds:

  ```rust
  static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);
  pub fn get_conn_id() -> ConnectionId { NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed) }
  ```

  Widening `ConnectionId` from `u32` to `u64` is the correct end state and is a breaking change (`pub type ConnectionId = u32;` at `basics.rs:19` is re-exported and used in downstream signatures). If that is too disruptive for a patch release, a monotonic `AtomicU32` counter is still strictly better than the timestamp: it makes reuse take 4.29 billion connections instead of 71 minutes. Belt and braces: make `WebsocketStates::insert` reject a duplicate id (log and refuse the connection) rather than overwrite.
  `get_log_id()` on the line above has the same shape but stays `u64`, so it is fine.
- **Effort:** S for the counter; M if `ConnectionId` is widened to `u64` and the four backends are updated.
- **Blast radius:** Breaking if the type widens. `api.support.cafe/src/service/user_connection_registry.rs` and `nofilter.io-backend/src/handlers/utils/subscription_router.rs` both hardcode `u32`.

### [SEV-3] The framework logs access tokens and full request payloads at `debug`

- **ID:** `endpoint-libs-security-and-framework-api-03`
- **Severity:** High
- **Category:** Security (secrets in logs)
- **Confidence:** High
- **Location:** `src/libs/ws/headers.rs:201`, `src/libs/ws/session.rs:84`
- **What:** `EndpointAuthController::auth` logs the raw subprotocol header and its parsed splits:

  ```rust
  debug!(ws_server = true, raw_header = %header, splits = ?splits,
         "EndpointAuthController: parsed protocol header");
  ```

  That header **is** the credential. The documented format is `0<methodname>,1<param1>,...` and slot 1 is the access token; the crate's own test names the fixture `1token_value` (`headers.rs:278`). Separately, `session.rs:84` logs every inbound text frame verbatim (`"Handling request {}", t`), which includes whatever secrets a handler's parameters carry.
- **Why it matters:** These are `debug!` on the `tracing` pipeline, which `libs/log` wires to both a file appender and the OTLP exporter. Any service that raises its level to debug (`WsServerConfig.debug` exists precisely to do this, `server.rs:629`) ships live session tokens to the log sink and to whatever OTel backend is configured. Session tokens in a log aggregator are replayable credentials: they defeat the entire auth-via-header design. api.support.cafe's `Init` endpoint takes `access_token` as its first parameter (`api.support.cafe/src/handlers/auth_api.rs:25-27`), so this is not hypothetical.
- **Fix:** Redact at the source. Log only the method name (`splits.get("0")`) and the parameter *count*; never `raw_header`, never `splits`. For `session.rs:84`, log the byte length and the parsed `method`/`seq` after deserialization instead of the payload, or gate the payload behind an explicit `trace!` plus a config flag whose name says what it does (`log_request_bodies_insecure`). A framework should not make "turn on debug logging" a credential-disclosure event.
- **Effort:** S
- **Blast radius:** Two log statements. Downstream operational runbooks that rely on seeing payloads would need an alternative.

### [SEV-4] There is no `on_disconnect` hook, so nothing keyed by connection id can ever be cleaned up

- **ID:** `endpoint-libs-security-and-framework-api-04`
- **Severity:** High
- **Category:** Design / Security (unbounded growth, stale authorization state)
- **Confidence:** High
- **Location:** `src/libs/ws/hooks.rs:91-95` (the hook set), `src/libs/ws/server.rs:302-329` (where the callback would go). Downstream: `api.support.cafe/src/service/user_connection_registry.rs:25` (`unregister`, never called), `nofilter.io-backend/src/handlers/utils/subscription_router.rs:75`
- **What:** `Hooks` offers `before`, `after` and `on_connect`. There is no disconnect counterpart: `rg -i disconnect src/` returns only two log strings in `toolbox.rs`. `handle_session_connection` knows exactly when a session ends (`server.rs:320-328`) and tells nobody.
- **Why it matters:** The framework hands out a connection id and invites downstream code to key state by it (`RequestContext.connection_id` is the only per-connection handle a handler gets), then never signals teardown. The predictable result is in the tree:
  - api.support.cafe wrote `UserConnectionRegistry::unregister` and **calls it from nowhere**: `rg "user_connection_registry\.unregister" api.support.cafe/src` returns nothing. The `HashMap<ConnectionId, UserPublicId>` grows for the process lifetime and every stale entry is a live authorization grant waiting for finding 02 to reissue its id.
  - nofilter.io-backend's subscription router unsubscribes only when the client explicitly calls the unsubscribe endpoint (`sub_session_events.rs:40`, `sub_studio_events.rs:31` and three siblings). A client that just closes the socket leaks its subscription.
  - The framework's own `SubscribeManager` has the same problem and papers over it lazily: it drops subscribers only when a publish to them fails (`push.rs:98-102`), so a topic that is never published to accumulates dead subscribers forever.
- **Fix:** Add the hook, and drive the framework's own `SubscribeManager` from it.

  ```rust
  #[async_trait(?Send)]
  pub trait OnDisconnect: Send + Sync {
      async fn on_disconnect(&self, conn: &WsConnection, ext: &Extensions);
  }
  ```

  Call it from `handle_session_connection` right before `states.remove` (`server.rs:322`), and from the auth-failure path once finding 01 is fixed so refused connections also fire it. Needs a short design discussion on one point: whether it fires for connections that never authenticated (it should, with the identity absent).
- **Effort:** M for the framework; S per backend to wire it up.
- **Blast radius:** Additive, non-breaking. Deletes the `// TODO: remove when elibs will be capable to have xustome context.` in `api.support.cafe/src/service/user_connection_registry.rs:8`.

### [SEV-5] No timeouts, no rate limits, no connection cap, and no way to bound WebSocket message size

- **ID:** `endpoint-libs-security-and-framework-api-05`
- **Severity:** High
- **Category:** Security (DoS)
- **Confidence:** High
- **Location:** `src/libs/ws/tungstenite/upgrader.rs:368`, `src/libs/ws/session.rs:168` and `:268`, `src/libs/ws/tls.rs:66-70`, `src/libs/ws/server.rs:617-670` (`WsServerConfig`)
- **What:** Four separate gaps, all of the same shape: the framework provides no knob and no default, so every backend must remember.
  1. **Message size.** `WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None)` passes `None` for `WebSocketConfig`, taking tungstenite's defaults (64 MiB per message, 16 MiB per frame) with **no way for a downstream service to lower them**. `WsServerConfig` has no field for it. Contrast the framed transport, which does it right: `framed_json_with_max_frame` is public and `DEFAULT_MAX_FRAME_BYTES` is 16 MiB (`transport/framed.rs:43,171`). The WebSocket path, which is the one exposed to the internet, is the one you cannot bound.
  2. **Handshake and idle timeouts.** `self.acceptor.accept(channel).await?` (`tls.rs:68`) has no timeout, nor does the hyper upgrade, nor the auth call, nor the session loop. A slowloris client holds a task and a TLS buffer indefinitely.
  3. **Per-connection concurrency.** `handle_message` does `tokio::task::spawn_local` per inbound frame (`session.rs:168`, `:268`) with no semaphore and no in-flight cap. A client can pipeline unboundedly; each task holds a cloned `RequestContext`, `EndpointSchema` and `Hooks`.
  4. **Rate limiting and connection caps.** Neither exists anywhere in the crate.
- **Why it matters:** 64 MiB × concurrent connections is the memory ceiling, and it is set by the attacker. A thousand connections each sending one maximal message is 64 GB of read buffers. There is no backpressure between the socket and the spawn loop. Because this is a framework, the absence is inherited by every service and none of the four backends compensates: `rg -i "timeout|rate.?limit"` over their `src/` finds nothing at the WS layer.
- **Fix:** Add the fields to `WsServerConfig` with safe defaults and thread them through:

  ```rust
  pub max_message_bytes: usize,        // default 1 MiB, not 64
  pub max_frame_bytes: usize,          // default 1 MiB
  pub handshake_timeout: Duration,     // default 10s
  pub idle_timeout: Option<Duration>,  // default 5min
  pub max_inflight_per_conn: usize,    // default 64
  pub max_connections: Option<usize>,
  ```

  `max_message_bytes` / `max_frame_bytes` map straight onto `WebSocketConfig` at `upgrader.rs:368`. The in-flight cap is an `Arc<Semaphore>` acquired before `spawn_local` and released on task completion. Timeouts are `tokio::time::timeout` wrappers. Do this as one release rather than piecemeal so the migration note is one entry.
- **Effort:** M
- **Blast radius:** `WsServerConfig` gains fields; adding `#[serde(default = ...)]` for each keeps existing config JSON valid. Lowering the message ceiling from 64 MiB to 1 MiB is a behaviour change that needs a changelog note, since a backend relying on large payloads would start failing.

### [SEV-6] `WsConnection.extensions` is immutable after construction, so auth cannot attach identity; every backend hand-rolls a side map

- **ID:** `endpoint-libs-security-and-framework-api-06`
- **Severity:** High
- **Category:** Design
- **Confidence:** High
- **Location:** `src/libs/ws/basics.rs:57`, `src/libs/peer.rs:178-229`, `src/libs/ws/server.rs:231-249`, `src/libs/ws/headers.rs:18-25`. Downstream: `api.support.cafe/src/service/user_connection_registry.rs`, and the same lookup repeated in 26 files under `api.support.cafe/src/handlers/`
- **What:** `Extensions` is a plain `HashMap<TypeId, Box<dyn CloneAny>>` with `insert`/`get_mut` taking `&mut self` (`peer.rs:189,205`). `WsConnection.extensions` is an owned `Extensions` field, and `WsConnection` is only ever reachable behind an `Arc` (`server.rs:242`, `AuthController::auth(self, toolbox, header, conn: Arc<WsConnection>)`). There is no interior mutability and no setter. `RequestContext::from_conn` clones the map (`toolbox.rs:141`), so `BeforeRequest`'s `&mut RequestContext` writes are per-request and discarded.

  Net effect: `WsConnection` gives an auth controller exactly two mutable slots, `set_user_id(u64)` and `set_roles(Arc<Vec<u32>>)` (`basics.rs:78-86`). Any application whose identity is not a `u64` has nowhere to put it.
- **Why it matters:** This single hole generates most of the boilerplate in the slice. api.support.cafe's identity is a `UserPublicId` (a packed nanoid), so it built `UserConnectionRegistry`, a `RwLock<HashMap<u32, UserPublicId>>`, and wrote the reason in a comment at `user_connection_registry.rs:8`:

  > `// TODO: remove when elibs will be capable to have xustome context.`

  The consequences compound: the side map is keyed by the colliding id from finding 02, it is never cleaned up because of finding 04, and the lookup-plus-error block is copy-pasted into every handler. `rg -c user_connection_registry api.support.cafe/src/handlers` matches 26 files and `"Connection not authenticated"` appears at 20 sites, each in this shape:

  ```rust
  let user_pub_id = self.user_connection_registry.get(ctx.connection_id).await
      .ok_or_else(|| CustomError::new(EnumErrorCode::Unauthorized)
          .with_message("Connection not authenticated"))?;
  ```

  Every one of those is an authorization check the framework should have made unrepresentable.
- **Fix:** Make connection extensions writable and have `RequestContext` read a snapshot.

  ```rust
  // basics.rs
  pub struct WsConnection {
      /* ... */
      pub extensions: parking_lot::RwLock<Extensions>,
  }
  impl WsConnection {
      pub fn insert_ext<T: Clone + Send + Sync + 'static>(&self, v: T) { self.extensions.write().insert(v); }
  }
  // toolbox.rs — from_conn takes a snapshot
  extensions: conn.extensions.read().clone(),
  ```

  Then api.support.cafe's auth handler does `conn.insert_ext(user_pub_id)` once and every handler does `ctx.extensions.get::<UserPublicId>()`, deleting the registry, the 26 imports and the 20 duplicated error blocks. Worth pairing with a typed helper so the "unauthenticated" case is one call:

  ```rust
  impl RequestContext {
      pub fn require<T: Clone + Send + Sync + 'static>(&self) -> Result<&T, CustomError> { /* ... */ }
  }
  ```

  Needs a short design discussion: whether the snapshot-per-request cost is acceptable (it is a `HashMap` clone of typically one entry) or whether `RequestContext` should hold `Arc<RwLock<Extensions>>` and give up `Clone`-without-sharing.
- **Effort:** M for the framework, M per backend to adopt.
- **Blast radius:** `WsConnection.extensions` is a public field, so changing its type is breaking. Confine it by keeping the field private and exposing `insert_ext`/`with_ext`. Deletes roughly 40 lines plus 26 imports in api.support.cafe alone.

### [SEV-7] CORS defaults to `*`, and the wildcard header block is frozen from whichever config initialised it first

- **ID:** `endpoint-libs-security-and-framework-api-07`
- **Severity:** Medium
- **Category:** Security
- **Confidence:** High on the `OnceLock` bug; Medium on the practical impact of the wildcard, which depends on the auth model
- **Location:** `src/libs/ws/tungstenite/upgrader.rs:31`, `:42-82` (esp. `:63-79`), `:382-435`; default at `src/libs/ws/server.rs:657`
- **What:** Two problems in `build_response`.
  1. **The default is `*`.** `WsServerConfig::default()` sets `allow_cors_urls: Arc::new(None)` and `None` means wildcard: `Access-Control-Allow-Origin: *`, `Timing-Allow-Origin: *`, and a preflight echo that reflects `Access-Control-Request-Headers` verbatim (`:392-396`). A service that simply does not set `allow_cors_urls` gets the permissive mode silently. The named-domains branch is the one that does the right thing (exact origin match, `Vary: Origin`, credentials).
  2. **`BASE_HEADERS` is a process-global `OnceLock<HeaderMap>` initialised from the first `config` it sees** (`:31`, `:42-82`). The closure captures `config.server_name` and branches on `config.allow_cors_urls`. Any process running two `WebsocketServer`s (or a test suite that constructs several) gets the first server's CORS mode and `Server` header applied to all of them, permanently. That is a caching bug wearing a performance hat.
  
  Separately, and by design as far as I can tell: the actual upgrade is never checked against `Origin`. CORS headers are advisory; WebSocket is exempt from the same-origin policy, so any page can open a socket to this server.
- **Why it matters:** Cross-site WebSocket hijacking is a real risk only when credentials ride ambient state (cookies). Here they do not: auth is a token in `Sec-WebSocket-Protocol`, which an attacker's page cannot forge from a victim's session, so the wildcard is much less dangerous than it looks. I am calling it Medium rather than High for that reason. But the framework does not *enforce* that model, and a backend that later adds cookie-based auth inherits a wide-open default. The `OnceLock` bug is unambiguous and will produce a confusing "why is CORS wrong on my second server" report eventually.
- **Fix:** Flip the default: make `allow_cors_urls` an explicit choice (`enum CorsPolicy { Wildcard, Origins(Vec<String>) }` with no `Default`), so a service has to say "wildcard" out loud. Move `BASE_HEADERS` from a `OnceLock` static into a field on `WsServerConfig` (or an `Arc<HeaderMap>` built once per server in `WebsocketServer::new`), which costs one `Arc` clone per response and removes the cross-server bleed. Optionally add `require_origin: Option<Vec<String>>` that rejects the upgrade itself, for backends that do use cookies.
- **Effort:** S for the `OnceLock`; M for the policy type (breaking).
- **Blast radius:** `WsServerConfig.allow_cors_urls` is `pub` and `#[serde(skip)]`, so it is set in code, not config files. All four backends would need a one-line change.

### [SEV-8] `EndpointAuthController::auth` returns `Ok(())` even when authentication failed

- **ID:** `endpoint-libs-security-and-framework-api-08`
- **Severity:** Medium
- **Category:** Security / Design
- **Confidence:** High
- **Location:** `src/libs/ws/headers.rs:189-248` (esp. `:234-245`), `src/libs/ws/server.rs:260-282`
- **What:** The `AuthController` contract is `-> Result<()>`, and `serve_connection` treats `Err` as "refuse the connection". But `EndpointAuthController::auth` calls `endpoint.handler.auth(...)` which returns `()` (the erased `SubAuthControllerErased` sends its own response through the toolbox, `headers.rs:108-113`) and then unconditionally returns `Ok(())` at `:245`. A rejected login and a successful one are indistinguishable to the caller.
- **Why it matters:** The system is fail-closed, but by accident rather than design. Safety rests entirely on `check_roles` returning `false` when `actual_roles` is empty (`session.rs:390-395`) and the auth handler only calling `conn.set_roles(...)` on success. That is a two-step invariant held together by convention across two crates, with no type or test enforcing it. A downstream `SubAuthController` that sets roles before its final validation step, or an endpoint registered with a role set that somehow intersects the default empty vector, silently becomes an authentication bypass. Secondary effect: a connection whose auth failed is not closed, so it sits in the session loop consuming a task and a socket until the client goes away.
- **Fix:** Change `SubAuthControllerErased::auth` to return `Result<(), ()>` (or propagate the `Response`'s success bit) and have `EndpointAuthController::auth` return `Err` when the sub-handler produced an error. Then close the connection on the error path in `serve_connection`. Alternatively, and more strongly, make the success path *be* the role assignment: have `SubAuthController::auth` return the roles rather than mutating `conn`, so "authenticated" and "has roles" are the same event and cannot drift.
- **Effort:** M, and it needs design discussion because the second option is a breaking trait change coordinated with `honey_id-types`.
- **Blast radius:** `SubAuthController` implementors in api.support.cafe (`handlers/app/auth.rs`), nofilter.io-backend and `honey_id-types` (`GenericAuthorizedConnect`, `MethodApiKeyConnect`).

### [SEV-9] `AfterRequest` cannot observe failures; `RequestOutcome::InternalErr` is never constructed

- **ID:** `endpoint-libs-security-and-framework-api-09`
- **Severity:** Medium
- **Category:** Design / AI-smell (dead enum variant)
- **Confidence:** High
- **Location:** `src/libs/ws/hooks.rs:34-42` (the enum), `src/libs/ws/session.rs:201` and `:298` (unconditional `Ok`), `src/libs/ws/handler.rs:130-137`
- **What:** `rg "RequestOutcome::" src/` returns exactly four sites: two `PublicErr` (both from a rejecting `BeforeRequest` hook) and two `Ok`. `InternalErr` is declared and never built. The `Ok` sites fire regardless of what the handler actually did, and the code says so:

  ```rust
  // The erased handler reports its own outcome through the toolbox, so
  // AfterRequest observes completion rather than the specific result here.
  hooks.run_after(&context, &schema, &RequestOutcome::Ok).await;
  ```
- **Why it matters:** `hooks.rs:6` advertises this seam for "quota enforcement, audit logging". An audit log that records every request as `Ok` is worse than no audit log, because it looks like one. A quota hook cannot bill only successes. The cause is structural: `RequestHandlerErased::handle` returns `()` and pushes the response into the toolbox itself (`handler.rs:136`), so the outcome is gone by the time `run_after` is reached. That is the same design decision that forces `handle_mcp` to duplicate the whole response-encoding match (`handler.rs:174-222`) rather than share it with `handle`.
- **Fix:** Have `RequestHandlerErased::handle` and `handle_mcp` return the outcome (`-> RequestOutcome`) and let the session loop pass it to `run_after`. The blanket impl already has the value in hand at `handler.rs:133` and `:175`. `RequestHandlerErased` is `#[doc(hidden)]` but has a defaulted method, so hand-written impls exist downstream; give the new method a default that returns `Ok` to keep them compiling, then fix them. If nobody intends to fix this, delete `RequestOutcome::InternalErr` and document that `AfterRequest` sees completion only, so the next reader does not build a compliance feature on top of it.
- **Effort:** M
- **Blast radius:** `#[doc(hidden)]` trait, so nominally internal, but generated code and manual impls in the backends implement it.

### [SEV-10] Every legacy request pays a full JSON re-serialization that is only needed on the error path

- **ID:** `endpoint-libs-security-and-framework-api-10`
- **Severity:** Medium
- **Category:** Performance
- **Confidence:** High
- **Location:** `src/libs/ws/handler.rs:99-112`; the same mistake avoided at `:147-158`; duplicated at `src/libs/ws/headers.rs:78`
- **What:**

  ```rust
  async fn handle(&self, toolbox: &ArcToolbox, ctx: RequestContext, req: Value) {
      // TODO: find a better way to avoid double parsing or serialization
      let buf = serde_json::to_string(&req).unwrap();     // :101 — runs on EVERY request
      let data: T::Request = match serde_json::from_value(req) {
          Ok(data) => data,
          Err(err) => { /* only here is `buf` used */ }
  ```

  `buf` exists solely so the error branch can re-run the parse through `serde_path_to_error` for a field path. It is computed unconditionally. The MCP twin at `:152` gets it right, building the string inside the `Err` arm.
- **Why it matters:** It is a full traversal plus a heap allocation proportional to payload size on the hot path of every request on every backend, discarded 99.9% of the time. This is the single hottest function in the request path, and the fix costs nothing.
- **Fix:** `serde_path_to_error` works over any `Deserializer`, and `&Value` is one. Drop `buf` entirely:

  ```rust
  let data: T::Request = match serde_path_to_error::deserialize(&req) {
      Ok(data) => data,
      Err(err) => {
          let message = format!("{}: {}", err.path(), err.inner());
          /* send BAD_REQUEST */ return;
      }
  };
  ```

  That removes the double parse *and* the double serialize in one move, and removes the `.unwrap()`. Apply the same at `headers.rs:78`. The `TODO` on `:100` has been asking for exactly this.
- **Effort:** S
- **Blast radius:** `handler.rs` and `headers.rs`. Error message text changes slightly (the path is now always present), which a downstream test might assert on.

### [SEV-11] MCP tool parameters can never carry descriptions, so agents see untyped blobs

- **ID:** `endpoint-libs-security-and-framework-api-11`
- **Severity:** Medium
- **Category:** Design / AI-smell (unreachable branch)
- **Confidence:** High
- **Location:** `src/model/types.rs:67-69` (`#[serde(skip)]`), `src/model/json_schema.rs:111-117` (the dead branch), `src/model/api_document.rs:177` (the only `apply_meta` call site), `src/libs/ws/server.rs:115`
- **What:** Three things line up badly.
  1. `Field.description` is `#[serde(skip)]`. The server builds its `EndpointSchema` by deserializing the generated `T::Request::SCHEMA` string (`server.rs:115`), so `description` is **always** `""` at runtime, by construction.
  2. `fields_to_object_schema` therefore has an unreachable branch: `if !field.description.is_empty() { ... }` (`json_schema.rs:111-117`) can never fire on the server path.
  3. 2.1 added `Field.meta` as the replacement channel, but `apply_meta` is called only from `api_document.rs:177`, never from `json_schema.rs`. So `example`, `deprecated` and `x-` annotations reach the OpenAPI and AsyncAPI documents and **not** the MCP tool schemas. `README.md:323-326` frames the separation as being about self-contained `$defs`; the meta consequence is not stated.
- **Why it matters:** Measured on the real surface: across api.support.cafe's seven `docs/*_mcp_tools.json` files, **0 of 35 tool parameters carry a description**. An LLM calling `app_admin` tools sees `{"type":"string"}` for every argument, with no hint about what `appPublicId` is or what values are legal. For a crate whose one-line description is "Launch MCP services fast", the parameter documentation channel being structurally closed is a product defect, not a cosmetic one. `docs/mcp-migration.md` lists the `#[serde(skip)]` as a "gotcha" rather than a bug, which suggests it is known and unowned.
- **Fix:** Either remove `#[serde(skip)]` from `Field.description` (it is a `String` with `Default`, so old serialized schemas still deserialize, and endpoint-gen would need to start emitting it), or route `Field.meta` through `fields_to_object_schema` the way `api_document.rs` does. The second is cheaper and consistent with the 2.1 direction: call `apply_meta(&mut schema, &field.meta, ctx)` right after building each field's schema in `json_schema.rs:105-118`. Either way, endpoint-gen has to start carrying field descriptions from RON, which is a coordinated change across the chain (`docs/chain.md`).
- **Effort:** M, spanning endpoint-libs and endpoint-gen.
- **Blast radius:** Changes the committed `docs/*_mcp_tools.json` fixtures in every backend, which is the intent.

### [SEV-12] `parse_protocol_header` slices at byte index 1 and silently loses parameters containing commas

- **ID:** `endpoint-libs-security-and-framework-api-12`
- **Severity:** Medium
- **Category:** Correctness / Security (latent panic)
- **Confidence:** High on the truncation; **Medium on the panic being currently reachable** (see below)
- **Location:** `src/libs/ws/headers.rs:180-187`, tests at `:311-318`
- **What:**

  ```rust
  fn parse_protocol_header(header: &str) -> HashMap<&str, &str> {
      header.split(',').map(str::trim).filter(|x| !x.is_empty())
            .map(|x| (&x[..1], &x[1..]))     // panics if x starts with a multi-byte char
            .collect()
  }
  ```

  Two defects. First, `&x[..1]` panics with "byte index 1 is not a char boundary" on any segment starting with a non-ASCII character. Second, `collect()` into a `HashMap` means duplicate positional keys silently last-win, and because the split is on `,`, **any parameter value containing a comma is truncated at the comma and the remainder is reinterpreted as a new positional parameter**. The crate's own test documents this as expected behaviour rather than flagging it (`:311-318`: `"0method,1val,ue"` yields `{"0":"method","1":"val","u":"e"}`).
- **Why it matters:** On the panic: today the WebSocket path is protected by accident. `hyper`'s `HeaderValue::to_str` rejects anything outside visible ASCII, so `upgrader.rs:281-286` can only produce ASCII, and the panic is unreachable *there*. But `WebsocketServer::serve_connection` is public and takes `auth_protocol: Option<String>` from any transport (`server.rs:222-228`), explicitly so local transports can hand over a token. A Unix-socket or XPC transport passing a UTF-8 token panics the session task. A framework should not have a panic one `pub fn` away from arbitrary input. I want a human to confirm no downstream already does this before treating it as live.

  On the truncation: it is not a privilege escalation (the client composes the whole header anyway, so injecting `2admin` grants nothing the client could not already ask for), but it is a silent correctness trap. Any credential format that can contain a comma, base64 with padding stripped is fine but JWT-with-claims-array or a URL-encoded value is not, breaks in a way that surfaces as "auth mysteriously fails for some users".
- **Fix:** Use `split_at` on a char boundary and reject rather than panic, and separate the parameter delimiter from the value alphabet:

  ```rust
  fn parse_protocol_header(header: &str) -> Result<HashMap<&str, &str>> {
      header.split(',').map(str::trim).filter(|s| !s.is_empty())
          .map(|seg| {
              let mut it = seg.char_indices();
              let (_, _) = it.next().context("empty segment")?;
              let split = it.next().map_or(seg.len(), |(i, _)| i);
              Ok(seg.split_at(split))
          }).collect()
  }
  ```

  And document, in `docs/mcp-migration.md` next to the header format, that parameter values must be percent-encoded. `parse_ty` already percent-decodes `Type::String` (`headers.rs:155-157`) but nothing encodes on the way in and no other type is decoded, so the contract is half-implemented.
- **Effort:** S
- **Blast radius:** `headers.rs` only. Percent-encoding the values is a client-side protocol change, so it needs a version note.

### [SEV-13] `load_config` prints the entire configuration, secrets included, to stdout

- **ID:** `endpoint-libs-security-and-framework-api-13`
- **Severity:** Medium
- **Category:** Security (secrets in logs) / Design
- **Confidence:** High on the behaviour; Low on current impact, because no backend calls it today
- **Location:** `src/libs/config.rs:44`, with `.unwrap()`s at `:37` and `:38`
- **What:** `println!("App config {config:#?}")` dumps the fully-deserialized config, unconditionally, not gated on `debug`. `DatabaseConfig.password` is a `SecretString` and redacts itself (`database.rs:26`), but nothing else does, and a downstream `Config` struct is free to hold plain `String` API keys.
- **Why it matters:** Container stdout is the log pipeline. The reason I am not calling this High is that **no backend uses this function**: `rg load_config /Users/revenge/code --type rust` finds only the definition and an unrelated function in EndpointValidator. All four backends wrote their own loader instead (`api.support.cafe/src/config/loader.rs`, 39 lines plus a 116-line Doppler source; `nofilter.io-backend/src/config/loader.rs`, 38 lines plus 831 lines of types; `pays.online-backend/src/config.rs`, 85 lines). That unanimous divergence is itself the finding: the framework's config story provides no environment-variable layering, no secret source, no redaction, and a `println!`, so everyone reimplements it.
- **Fix:** Remove the `println!` (or make it `tracing::debug!` behind a flag) and replace the two `.unwrap()`s with real errors naming the offending path. Then decide whether `load_config` should exist at all: either delete it and delete the `clap` dependency it drags in, or take the shape the backends converged on, JSON base plus `PREFIX__NESTED__KEY` env overrides plus a pluggable secret source, and let three loaders collapse into one.
- **Effort:** S to fix; L if the loader is generalized.
- **Blast radius:** Nothing depends on it today, which is the cheapest possible time to change it.

### [SEV-14] The `types`-only default feature pulls 246 crates, four of them unused

- **ID:** `endpoint-libs-security-and-framework-api-14`
- **Severity:** Medium
- **Category:** Maintainability / Supply chain
- **Confidence:** High
- **Location:** `Cargo.toml:109-180` (the unconditional `[dependencies]` block)
- **What:** Measured:

  ```
  cargo tree --no-default-features --features types -e normal | sort -u | wc -l   →  246
  cargo tree --features full -e normal | sort -u | wc -l                          →  344
  ```

  `default = ["types"]` is documented as the model-only surface, yet it unconditionally compiles `tokio` with `features = ["full"]`, the whole OpenTelemetry stack (`opentelemetry`, `_sdk`, `-otlp`, `-semantic-conventions`, `-appender-tracing`, `tracing-opentelemetry`), `tonic`, `hyper-rustls`, `alloy-primitives`, `clap`, `reqwest`'s transitive graph and `tracing-appender`. Four of those have no use in the crate at all:
  - `hyper-rustls` — **0 occurrences in `src/`**. Present only for the comment at `Cargo.toml:169`, "Force bundled CA roots for hyper-rustls (used by instant-acme inside cert-provider)" (`Cargo.toml:169-170`), and `cert-provider` is a **dev-dependency**. A published crate carries a full TLS/HTTP client stack for the benefit of its own test suite.
  - `tonic` (`Cargo.toml:180`) — **0 code uses**; the only match in `src/` is the string literal `"tonic"` in a log-level list (`libs/log/level_filter.rs:69`). The comment calls it "Direct deps for header configuration".
  - `derive_more` (`Cargo.toml:113`) — **0 uses in `src/`**; used only by `examples/ws-echo/main.rs:4`. Belongs in `[dev-dependencies]`.
  - `alloy-primitives` (`Cargo.toml:131`) — one line, a re-export of `Address`/`H256`/`U256` in `libs/types.rs:5`. Every non-blockchain backend (api.support.cafe is a chat service; nofilter.io is streaming) compiles Ethereum primitives to get it.
- **Why it matters:** Compile time and binary size on every backend and every CI run, and a supply-chain surface four crates wider than it needs to be for no functional return. The `#[features]` block reads as if it gates things it does not gate.
- **Fix:** Move `derive_more` to `[dev-dependencies]`. Delete `hyper-rustls` and `tonic` (verify the OTLP exporter still resolves its own TLS; if `hyper-rustls` is genuinely load-bearing for `opentelemetry-otlp`, say so in the comment instead of blaming a dev-dependency). Put `alloy-primitives` behind a `blockchain` feature and re-export from `libs/types.rs` conditionally. Put the OpenTelemetry group behind an `otel` feature, on by default if you like, so a `types`-only consumer can turn it off. Gate `tokio`'s `full` down to the features actually used, or make `tokio` optional under `ws-core`.
- **Effort:** M, and each removal wants its own commit so a regression is bisectable.
- **Blast radius:** Feature-gating `alloy-primitives` and OTel is breaking for any consumer relying on the default. Removing unused deps is not.

### [SEV-15] The `s3-sync` feature is documented as usable and does nothing for consumers

- **ID:** `endpoint-libs-security-and-framework-api-15`
- **Severity:** Low
- **Category:** Docs / Correctness
- **Confidence:** High
- **Location:** `Cargo.toml:105` (`s3-sync = ["cert-provider/s3-sync"]`), `Cargo.toml:188` (`cert-provider` under `[dev-dependencies]`, declared at `:182`), `README.md` "### `s3-sync`" section
- **What:** `s3-sync` forwards to a feature of `cert-provider`, which is a **dev-dependency**, git-sourced from `github.com/dVeon-loch/cert-provider.git` with no `rev` or `tag` in the manifest (`Cargo.lock:677` pins `eb2387f2` only for this repo's own lockfile). Dev-dependencies are not compiled for downstream consumers, so `cargo add endpoint-libs --features s3-sync` enables nothing. `cargo check --no-default-features --features s3-sync` succeeds, silently, which is why nobody noticed. The README documents it as "for certificate material synced from S3".
- **Why it matters:** A published feature that silently no-ops is a support ticket in waiting, and the README states it as a capability. Minor on its own; it is here because the git dependency is the crate's only non-registry source and deserves to be visible in an audit. Its dev-only status is what keeps it out of downstream supply chains, which is worth recording explicitly so a future change to `[dependencies]` gets scrutiny.
- **Fix:** Delete the `s3-sync` feature and the README section, or promote `cert-provider` to a real optional dependency pinned to a `tag`/`rev` and wire it up. Publishing a crate whose feature depends on an unpinned git ref would be a genuine supply-chain problem, so if you promote it, pin it.
- **Effort:** S
- **Blast radius:** Nothing can be depending on it, since it does nothing.

### [SEV-16] `AGENTS.md` misdescribes the repository, and it is mandatory reading for every agent

- **ID:** `endpoint-libs-security-and-framework-api-16`
- **Severity:** Low
- **Category:** Docs
- **Confidence:** High
- **Location:** `AGENTS.md:8` and `:20`; also `src/libs/ws/hooks.rs:70` vs `:13`, and `src/libs/ws/server.rs:104`
- **What:** Two independent doc defects.
  1. `AGENTS.md:8` states: "**Mixed Rust + JavaScript repository.** The JS side (`endpoint-libs-examples`, built with `npm`) is the primary surface; the Cargo workspace holds supporting Rust (e.g. integration tests)." There is no `endpoint-libs-examples` directory and **zero `.js`/`.ts` files** in the repo. `package.json` is a 10-line stub naming `wrangler` and `@cloudflare/containers`, with a 50 KB `package-lock.json` and no source. The repo is a Rust crate, ~14k lines across 50 files. Line 20 compounds it: "**`npm` is the package manager** — its lockfile is authoritative."
  2. `hooks.rs:70` documents `OnConnect` as running "once per connection, **after auth**", and `server.rs:104` repeats it. The ordering diagram eight lines up at `hooks.rs:13` says `connect ──► OnConnect ──► auth`, and the code agrees with the diagram (`server.rs:232` runs `run_on_connect` before `:260` runs `auth`). The doc comment is simply wrong.
- **Why it matters:** `CLAUDE.md` calls `AGENTS.md` "binding" and imports it into every session, and `AGENTS.md` itself says "Docs describe what is true now" and "Hit a factual error here, fix it in the same change". An agent reading it will look for a JS surface that does not exist and may run `npm install` believing it is the primary build. The `OnConnect` ordering error is smaller but security-adjacent: someone writing an attestation hook that assumes auth already ran will build on a false premise.
- **Fix:** Rewrite `AGENTS.md:8` and `:20` to describe a Rust crate with a vestigial `package.json`, or delete `package.json`/`package-lock.json`/`tsconfig.json` if the Cloudflare experiment is dead. Fix `hooks.rs:70` and `server.rs:104` to say "before auth", and add one sentence saying why (the hook cannot see identity, only the peer).
- **Effort:** S
- **Blast radius:** Docs only.

---

## Cross-cutting recommendations

1. **Give the connection a real, unique identity and a real lifecycle.** Findings 01, 02 and 04 are three faces of the same gap: `ConnectionId` is generated unsafely, registered before it is earned, and never signalled as gone. Do them as one change: a monotonic counter, an RAII slot guard, and an `OnDisconnect` hook fired from the guard's `Drop`. That single change closes a remote DoS, a cross-user delivery path, and the leak in api.support.cafe's authorization registry. Nothing else in this review has a comparable ratio of blast radius to effort. What breaks: widening `ConnectionId` to `u64` touches downstream signatures, so either do that deliberately with a major bump or keep `u32` and take the counter alone.

2. **Close the identity hole so the backends can delete their registries.** Finding 06 is the single largest source of hand-written boilerplate in the slice: 26 files and 20 duplicated authorization blocks in api.support.cafe alone, all because an `AuthController` cannot write to `WsConnection.extensions`. Make connection extensions writable, add `RequestContext::require::<T>()`, then send a one-line PR to each backend replacing its side map. This is what `Extensions` was added for in 2.0; it is 90% built and stops one step short of useful.

3. **Ship a "resource limits" release.** Finding 05 is not one bug but a category the framework has not addressed at all: message size, handshake timeout, idle timeout, in-flight cap, connection cap. Do them together in one `WsServerConfig` expansion with conservative defaults and one changelog entry, rather than trickling them out. The framed transport already demonstrates the right shape (`DEFAULT_MAX_FRAME_BYTES` plus a `_with_max_frame` constructor); the WebSocket path just needs to catch up. What breaks: a backend relying on payloads larger than the new default starts erroring, so announce the number.

4. **Make the request outcome a value, not a side effect.** Finding 09's dead `InternalErr` variant and finding 10's hot-path double-serialization both trace to `RequestHandlerErased::handle` returning `()` and pushing responses into the toolbox itself. Change it to return the encoded outcome, and `handle_mcp`'s 50-line duplicate of the response-encoding match (`handler.rs:174-222`) can share the legacy path's logic, `AfterRequest` gets real outcomes, and the double serialize disappears. One refactor, three findings.

5. **Audit what the framework says about itself.** Findings 11, 15 and 16 are all "the docs describe a thing the code does not do": MCP field descriptions that cannot exist, an `s3-sync` feature that no-ops, an `AGENTS.md` describing a JavaScript project. The repo has an executable chain check (`scripts/check-chain.sh`); consider extending it with a doc-claims pass, even a crude one that greps README feature headings against `Cargo.toml` features and asserts every documented path exists.

6. **Decide whether `endpoint-libs` owns config loading.** All four backends independently rejected `libs::config::load_config` and wrote their own (finding 13). Either delete it, dropping the `clap` dependency with it, or absorb what they converged on. Leaving a secret-printing loader in a published framework that nobody uses is the worst of both.

## What I did not cover

- **`src/libs/log*.rs` (2,101 lines across five files) beyond grepping for credential logging.** The OTel exporter setup, the error-aggregation regexes (`log/error_aggregation.rs`, 632 lines, and regexes over log lines are a catastrophic-backtracking candidate I did not evaluate), and `level_filter.rs` are a sibling slice's problem.
- **`src/libs/ws/client.rs` (717 lines)** beyond confirming that `danger_accept_invalid_certs` is opt-in and that `AcceptAllVerifier` is only reachable through it. Reconnect logic, backoff and the `last_err.unwrap()` at `:522` are unreviewed.
- **`src/libs/database/**` and `src/libs/datatable.rs`.** I checked that `DatabaseConfig.password` is a `SecretString` and that statement execution has timeouts, and stopped. SQL construction, the `data_thread` model and pool sizing are unreviewed.
- **`src/model/api_document.rs` (663 lines), the OpenAPI/AsyncAPI emitters.** Touched only where it intersects the MCP schema path (finding 11).
- **I did not run the test suite or a full `cargo test`.** I ran one `cargo check --no-default-features --features s3-sync` (5.7s) purely to determine whether that feature resolves, and `cargo tree` for the dependency counts. No source file was modified.
- **I did not verify finding 02's exploitation in a running system.** The collision arithmetic is certain; whether a production instance holds connections across the 71.58-minute boundary often enough to matter is an operational question. Treat the framework fix as unconditional and the "has this already happened" question as a separate log-forensics task.
- **`.claude/worktrees/magical-moser-b0466c/`** contains a second, slightly older copy of the tree (it lacks `peer.rs`, `hooks.rs` and `transport/`). I reviewed only the top-level `src/`. If that worktree holds uncommitted work, someone should reconcile it; I did not touch it.

## Quick-start for the follow-up agent

Read in this order:

1. `src/libs/ws/server.rs:222-330` — `serve_connection` and `handle_session_connection`. The auth-failure leak (finding 01) and the missing disconnect signal (finding 04) are both visible in these hundred lines, and it is the clearest single view of the connection lifecycle.
2. `src/libs/utils.rs:1-16` — nine lines, and finding 02 lives in two of them.
3. `src/libs/ws/toolbox.rs:146-269` — how `ConnectionId` becomes a response route. Read after (2) to see why the collision matters.
4. `src/libs/ws/session.rs` — the whole file, 421 lines. Both protocol paths, the role check, and the per-message spawn.
5. `src/libs/ws/headers.rs:180-248` — the auth-via-header implementation: the parser (finding 12), the credential log line (finding 03), the always-`Ok` return (finding 08).
6. `src/libs/ws/mcp.rs:349-404` — the MCP router. Read this one to see what the codebase looks like when it is done well; the role-gating and the no-existence-oracle property are worth preserving exactly as written.
7. `api.support.cafe/src/service/user_connection_registry.rs` (36 lines) then `api.support.cafe/src/handlers/app_admin/list_apps.rs:29-40` — the downstream consequence of finding 06, and the `// TODO: remove when elibs will be capable to have xustome context.` that names it.

Commands that were useful:

```bash
cargo tree --no-default-features --features types -e normal --prefix none | sort -u | wc -l   # 246
cargo tree --features full -e normal --prefix none | sort -u | wc -l                          # 344
rg -i "timeout|rate.?limit|semaphore|max_conn" src/          # only DB + log throttle
rg -n "RequestOutcome::" src/                                # 4 sites, no InternalErr
rg "user_connection_registry\.unregister" ../api.support.cafe/src   # empty
rg -c user_connection_registry ../api.support.cafe/src/handlers | wc -l   # 26
```

Repo conventions worth knowing before changing anything:

- `AGENTS.md` mandates running `./scripts/check-chain.sh` before calling a change done: endpoint-gen, honey_id-types, endpoint-validator and six backends all break silently when this crate moves. Release order is constrained (`docs/release-order.md`): `honey_id-types` re-exports the `WsRequest`/`WsResponse` traits and must publish *after* endpoint-libs, or consumers get two incompatible copies in one graph.
- The version in every downstream `Cargo.toml` is `"2.0"`, which resolves to 2.1.3. Anything shipped as 2.1.x reaches all four backends on their next `cargo update` with no manifest change. That cuts both ways: a fix propagates for free, and so does a regression.
- Do not add AI attribution to commits, PRs or comments; the rule is in `AGENTS.md` and enforced socially, not mechanically.
- `cargo check` on this crate is fast (under 6 seconds warm), so there is no excuse for not compiling a proposed fix.
- The four `.unwrap()`/`expect()` sites in non-test server code (`server.rs:115,117,478`, `upgrader.rs:59`) are all startup-time and are fine as-is; do not "fix" them into runtime errors. The panics worth caring about are covered in finding 12.

<details>
<summary>Nits</summary>

- `session.rs:390` `check_roles` and `mcp.rs:399` `roles_allowed` are byte-identical functions with different names, one tested in each file. Keep one, in `basics.rs`, and re-export.
- `mcp.rs:397` calls its version "Same semantics as the legacy role check" instead of just calling the legacy one.
- `push.rs:1` is `#![allow(dead_code)]` for the whole module; that hides real drift in a file that handles subscription routing.
- `push.rs:72-103` holds the `DashMap` shard write guard across the entire publish loop; with many subscribers on one topic this serializes publishers unnecessarily.
- `database.rs:121-128` exports `pub fn drop_and_recreate_database()` which shells out to `bash scripts/drop_and_recreate_database.sh`. A published framework should not export a database-destroying `pub fn` in a non-test module.
- `database.rs:111-119` `database_test_config()` hardcodes `password: "123456"`. Harmless as a fixture, but it is in the non-test API surface under the `database` feature.
- `server.rs:520-537` `dump_schemas` writes to a relative `docs/` path with `let _ = std::fs::create_dir_all("docs")`, silently swallowing the error and depending on the process's CWD.
- `json_schema.rs:127` emits `additionalProperties: false` in every tool schema, but the server does not enforce it: serde ignores unknown fields, so the server is more permissive than the schema it publishes.
- `handler.rs:100` `// TODO: find a better way to avoid double parsing or serialization` has a straightforward answer (finding 10); either do it or delete the TODO.
- `mcp.rs:41` comment says an explicit `"id": null` is "only ever legal in error responses per spec, so conflating it costs nothing" — correct, and a nice example of a comment that earns its place. More of these, fewer of the `// increment i` kind.
- `docs/mcp-migration.md:1` is still titled "1.7.x → 1.9.x" on a 2.1.3 crate; the version matrix inside it references `endpoint-gen ≥1.9.0` while `README.md:89` says endpoint-gen 1.13. One of them is stale.
- `Cargo.toml:172` comments `# OpenTelemetry dependencies (default feature)` above a block that is not gated on any feature.

</details>
