# endpoint-libs Review: Performance, Internals, AI Smell, Docs

**Date:** 2026-07-27
**Scope:** `src/**` (all 56 `.rs`), `tests/transport_seam.rs`, `examples/**`, `Cargo.toml`, `Cargo.lock`, all 14 in-repo `.md` files plus `docs/`, and read-only checks of `/Users/revenge/code/endpoint-libs-mcp-migration-guide.md` and `/Users/revenge/code/api.support.cafe-mcp-plan.md`
**Commit:** `b9f7563` (working tree clean)
**Reviewer slice:** `perf-internals-ai-smell-docs`. Sibling slices cover security and framework API design; I deliberately skipped auth/authz analysis, crypto, and public-API ergonomics except where they collide with performance or documentation accuracy.

## Summary

- **The 2.0/2.1 work is good.** `peer.rs`, `ws/transport/framed.rs`, `ws/mcp.rs` and `tests/transport_seam.rs` are genuinely well-built: documented invariants, real assertions, bounded frames, honest comments that explain *why*. The transport seam does what it claims.
- **The older strata are not.** `libs/database/`, `libs/log/error_aggregation.rs`, `libs/deserializer_wrapper.rs`, `libs/ws/push.rs` and `libs/utils.rs` predate that care and carry real defects, including two that silently corrupt behaviour: `ConnectionId` is a truncated wall-clock timestamp (collides, so responses reach the wrong connection), and `LogLevel::Off` builds a TRACE-everything filter.
- **The per-request hot path pays for avoidable work on every request of every downstream backend**: a full `serde_json::to_string` of the request params that is thrown away on the success path, a deep `EndpointSchema` clone, and (when MCP is enabled) an entire second JSON DOM parse of every legacy frame. All three are mechanical fixes.
- **Docs are the weakest area and the brief was right to weight them.** `AGENTS.md`, the file every agent in this repo loads first, declares this a "Mixed Rust + JavaScript repository" whose "primary surface" is JS built with npm. There is not one `.js` or `.ts` file in the tree. `examples/README.md` documents an example and a feature that do not exist. The README's OpenTelemetry example does not compile and names a type that was never written. Both external migration playbooks are pinned to 1.9.1 while the crate is 2.1.3, across a breaking 2.0.
- **Release hygiene is poor.** The README advertises `types` as a minimal default; `cargo tree -e normal --no-default-features --features types` returns **246 crates**, versus 344 for `full`. `tonic` and `hyper-rustls` are non-optional direct dependencies that no source file uses. The `s3-sync` feature points at a dev-dependency, so it is a no-op for anyone consuming the published crate.
- **Top three things to do:** (1) fix `get_conn_id` to an atomic counter; (2) fix the `LogLevel::Off` inversion and the `AfterRequest` outcome that is always `Ok`; (3) do a docs truth pass starting with `AGENTS.md` and `examples/README.md`.

## Findings

### [SEV-1] `ConnectionId` is a truncated wall-clock timestamp, so connections collide

- **ID:** `endpoint-libs-perf-internals-01`
- **Severity:** High
- **Category:** Correctness
- **Confidence:** High (mechanism), Medium (production collision rate, which depends on connection arrival rate)
- **Location:** `src/libs/utils.rs:7-9`, consumed at `src/libs/ws/server.rs:243`, keyed at `src/libs/ws/toolbox.rs:171-185`
- **What:** `pub fn get_conn_id() -> u32 { chrono::Utc::now().timestamp_micros() as _ }`. A 64-bit microsecond timestamp is truncated to `u32`. `ConnectionId` is the key into the `DashMap<ConnectionId, Arc<WsStreamState>>` that `Toolbox::send`/`send_raw` use to find a connection's outbound channel (`toolbox.rs:172`, `toolbox.rs:195`). `states.insert` (`server.rs:258`) overwrites on a duplicate key.
- **Why it matters:** Two failure modes. (a) Two connections accepted in the same microsecond get the same id. With `listen_impl` sharding accepts across N threads this is not hypothetical: at 10k conn/s, birthday collisions over a one-second window are roughly 50. (b) The truncated value wraps every 2^32 microseconds, about **71.6 minutes**, so any WebSocket session living longer than that (the normal case for a persistent RPC socket) can be collided by a fresh connection landing on the same wrapped microsecond. Either way, one connection's `WsStreamState` replaces another's in the map, and every response addressed to the victim's id is written into an unrelated client's socket. That is cross-connection data delivery in a framework every backend uses. `get_log_id` has the same construction but is `u64`, so it only degrades log correlation.
- **Fix:** Mechanical.
  ```rust
  use std::sync::atomic::{AtomicU32, Ordering};
  static CONN_ID: AtomicU32 = AtomicU32::new(1);
  pub fn get_conn_id() -> u32 { CONN_ID.fetch_add(1, Ordering::Relaxed) }
  ```
  Wrapping is still possible after 4.29e9 connections; if that matters, widen `ConnectionId` to `u64` (breaking) or have `WebsocketStates::insert` reject a live duplicate rather than overwrite. At minimum, make the overwrite loud: `if states.insert(...).is_some() { error!(...) }`.
- **Effort:** S for the counter; M if you also widen the type.
- **Blast radius:** `utils.rs`, `server.rs`, `subs.rs`, `push.rs`. Not a breaking API change if you keep `u32`.

### [SEV-2] `LogLevel::Off` builds a TRACE-everything filter

- **ID:** `endpoint-libs-perf-internals-02`
- **Severity:** High
- **Category:** Correctness
- **Confidence:** High
- **Location:** `src/libs/log/level_filter.rs:112` (`LogLevel::Off => Level::TRACE`), used by `build_env_filter` at `level_filter.rs:39-42`
- **What:** `build_env_filter` converts `LogLevel` to `tracing::Level`, and that conversion maps `Off` to `TRACE` because `Level` has no off variant. The base directive therefore becomes `trace`. Worse, the crate-capping block is gated on `log_level > LogLevel::Info` (`level_filter.rs:47`); with the derived `Ord` on the enum, `Off` is the *smallest* variant, so the guard is false and even the noise caps for `h2`/`rustls`/`opentelemetry` are skipped. Setting `Off` produces the single loudest configuration the function can emit. A correct `From<LogLevel> for LevelFilter` mapping `Off` to `LevelFilter::OFF` exists sixteen lines above (`level_filter.rs:96`) and is not used here.
- **Why it matters:** `LogLevel` is `#[derive(Default)]` with `#[default] Off` (`level_filter.rs:27-29`). An operator who sets `Off` to silence a noisy service gets full-firehose TRACE, including tungstenite frame logs and the `debug!("Handling request {}", t)` at `session.rs:84` that prints every request body. That is a throughput cliff and a data-exposure event triggered by the config value that means the opposite.
- **Fix:** Make `build_env_filter` branch on `Off` before doing anything else and return an `EnvFilter` built from `LevelFilter::OFF`; keep the `Level` conversion for the other variants. Add a test that asserts the *behaviour* rather than `is_ok()`.
- **Effort:** S
- **Blast radius:** `level_filter.rs` only. Behaviour change for anyone currently relying, accidentally, on `Off` being verbose.

### [SEV-3] `AfterRequest` can never observe a failed request

- **ID:** `endpoint-libs-perf-internals-03`
- **Severity:** High
- **Category:** Design / Correctness
- **Confidence:** High
- **Location:** `src/libs/ws/session.rs:198-202` and `:297-299`; enum at `src/libs/ws/hooks.rs:34-42`
- **What:** After awaiting the erased handler, the session unconditionally reports `RequestOutcome::Ok`:
  ```rust
  // The erased handler reports its own outcome through the toolbox, so
  // AfterRequest observes completion rather than the specific result here.
  hooks.run_after(&context, &schema, &RequestOutcome::Ok).await;
  ```
  `rg "RequestOutcome::" src/` returns four sites, all in `session.rs`: two `Ok`, two `PublicErr` (both on the *hook*-rejection path). **`RequestOutcome::InternalErr` is never constructed anywhere in the crate.** The comment is honest about the mechanism but the type is not: `RequestOutcome` advertises three outcomes and delivers one.
- **Why it matters:** `AfterRequest` is documented in the README (`README.md:431`) as the hook that "observes outcomes" and in `hooks.rs:8` as the seam for "quota enforcement, audit logging". An audit-log or SLO hook built on it records 100% success no matter how many handlers fail. A quota hook that only charges for successful calls charges for everything. This is the kind of defect discovered after the compliance report is already wrong.
- **Fix:** Needs a small design decision, then mechanical. The clean route is to have `RequestHandlerErased::handle`/`handle_mcp` return the outcome instead of `()`:
  ```rust
  async fn handle(&self, toolbox: &ArcToolbox, ctx: RequestContext, req: Value) -> RequestOutcome;
  ```
  The blanket impl in `handler.rs:99-138` already matches on `HandlerError::{Public, Internal, NoResponse}` (via `encode_handler_response` at `toolbox.rs:412-419`) and can map directly. This is a breaking change to a `#[doc(hidden)]` trait, so downstream impact is limited to hand-written erased handlers. Until then, the docs must say `AfterRequest` sees only hook rejections.
- **Effort:** M
- **Blast radius:** `handler.rs`, `session.rs`, `hooks.rs`, any manual `RequestHandlerErased` impl. A `#[doc(hidden)]` trait, so semver-defensible as a minor.

### [SEV-4] `AGENTS.md` describes a JavaScript repository that does not exist

- **ID:** `endpoint-libs-perf-internals-04`
- **Severity:** High
- **Category:** Docs
- **Confidence:** High
- **Location:** `AGENTS.md:7` ("Mixed Rust + JavaScript repository. The JS side (`endpoint-libs-examples`, built with `npm`) is the primary surface; the Cargo workspace holds supporting Rust (e.g. integration tests)"), `AGENTS.md:20` (npm declared the authoritative package manager), `AGENTS.md:22` ("Two toolchains live here"), `AGENTS.md:26-28` (Build & run leads with `npm install`)
- **What:** `find . -name '*.ts' -o -name '*.js' -o -name '*.tsx' -o -name '*.jsx'` (excluding `.claude/`) returns **zero files**. The repo is a single Rust crate. What exists is a vestigial `package.json` (name `endpoint-libs-examples`, deps `@cloudflare/containers` and `wrangler`), a `tsconfig.json` targeting `@cloudflare/workers-types`, and a 49 KB `package-lock.json`, with no source to build. There is no `../endpoint-libs-examples` directory either.
- **Why it matters:** `AGENTS.md` is loaded by every Claude Code session (via the `@AGENTS.md` import in `CLAUDE.md:1`) and read natively by Codex, Cursor and Gemini CLI, as the file itself states. It tells every agent that the Rust code is *supporting* material and that the primary surface is JavaScript. It instructs them to run `npm install` (pulling the wrangler tree) before `cargo build`, and forbids introducing a second lockfile in a repo that has no first one. This is the highest-leverage stale doc in the repo: it mis-frames the whole codebase for every automated contributor. It reads as a template copied from another repo and never reconciled.
- **Fix:** Delete the JS claims from `AGENTS.md` (lines 7, 20, 22, and the `npm install` block at 26-28). Separately decide whether `package.json`, `package-lock.json` and `tsconfig.json` should be deleted or whether the missing examples should be restored; if they are aspirational, say so explicitly.
- **Effort:** S
- **Blast radius:** `AGENTS.md`, and possibly three config files at the repo root.

### [SEV-5] Every request re-serializes its own params and throws the result away

- **ID:** `endpoint-libs-perf-internals-05`
- **Severity:** High
- **Category:** Performance
- **Confidence:** High
- **Location:** `src/libs/ws/handler.rs:100-101`, mirrored at `src/libs/ws/handler.rs:147` and `src/libs/ws/headers.rs:78`
- **What:**
  ```rust
  // TODO: find a better way to avoid double parsing or serialization
  let buf = serde_json::to_string(&req).unwrap();
  let data: T::Request = match serde_json::from_value(req) { ... }
  ```
  `buf` is used **only** inside the `Err` arm, to re-run the deserialization through `serde_path_to_error` for a field-path error message. On the success path it is a full JSON serialization of the entire request body, allocated and immediately dropped. `handle_mcp` (`handler.rs:147`) does the same thing differently and worse: `serde_json::from_value(req.clone())` deep-clones the whole arguments `Value` unconditionally so the original survives for the error branch.
- **Why it matters:** This runs once per request in every backend that uses this crate. For a 2 KB request body it is one 2 KB allocation plus a full DOM walk and UTF-8 encode, on the hot path, for a diagnostic that fires on the failure path only. Combined with the DOM round trip below, deserialization dominates per-request framework cost.
- **Fix:** Mechanical and obviously correct. Move the serialization into the error arm:
  ```rust
  let data: T::Request = match T::Request::deserialize(&req) {   // &Value is a Deserializer
      Ok(data) => data,
      Err(err) => {
          let buf = serde_json::to_string(&req).unwrap_or_default();
          let jd = &mut serde_json::Deserializer::from_str(&buf);
          ...
      }
  };
  ```
  Note `&serde_json::Value` already implements `serde::Deserializer`, so `T::Request::deserialize(&req)` avoids consuming `req` and removes the need for the clone in `handle_mcp` too. The structural fix is bigger: change `WsRequestValue`'s `params` from `Value` to `Box<RawValue>` (`basics.rs:36`) so the params are never materialized as a DOM at all, exactly as `WsSuccessResponse` already does for responses (`basics.rs:89`). That is a wire-compatible change but a breaking type change.
- **Effort:** S for the error-arm move; M for the `RawValue` change.
- **Blast radius:** `handler.rs`, `headers.rs`. The `RawValue` variant additionally touches `basics.rs`, `session.rs` and any consumer naming `WsRequestValue`.

### [SEV-6] Enabling MCP adds a second full JSON parse to every legacy request

- **ID:** `endpoint-libs-perf-internals-06`
- **Severity:** Medium
- **Category:** Performance
- **Confidence:** High
- **Location:** `src/libs/ws/session.rs:68-79` calling `src/libs/ws/mcp.rs:126-138`
- **What:** `try_parse_jsonrpc` does `serde_json::from_str::<Value>(payload)`, a complete DOM parse, purely to test whether a top-level `"jsonrpc": "2.0"` member exists. For a legacy frame it returns `None`, the `Value` is dropped, and `handle_message` then parses the *same bytes* again into `WsRequestValue` at `session.rs:85` and `:89`. When the frame *is* JSON-RPC, `try_parse_jsonrpc` also walks the `Value` a second time via `serde_json::from_value::<JsonRpcRequest>`.
- **Why it matters:** Every backend that adopts MCP (which the README pitches as the headline feature, and `docs/mcp-migration.md` as a one-line change) doubles the JSON parsing cost of its entire legacy traffic. For services where legacy frames are 99% of volume and MCP is a handful of agent calls, this is pure regression. It is also invisible: nothing in the docs mentions a cost.
- **Fix:** Parse once, dispatch from the single `Value`.
  ```rust
  let value: Value = match msg { Text(t) => from_str(&t), Binary(b) => from_slice(&b), ... };
  if mcp.is_some() && value.get("jsonrpc").and_then(Value::as_str) == Some("2.0") {
      // from_value::<JsonRpcRequest>(value)
  } else {
      // from_value::<WsRequestValue>(value)
  }
  ```
  This costs one DOM parse instead of two and composes with the `RawValue` change in SEV-5 (peek the raw bytes for `"jsonrpc"` before parsing at all). Keep `try_parse_jsonrpc` as a `pub` shim so the existing tests in `mcp.rs:462-489` still apply.
- **Effort:** S
- **Blast radius:** `session.rs`, `mcp.rs`. No API change if the helper is retained.

### [SEV-7] `ThreadedDbClient` funnels every database query through one serial task

- **ID:** `endpoint-libs-perf-internals-07`
- **Severity:** High if used, Low if it is vestigial
- **Category:** Performance
- **Confidence:** High on the mechanism; Low on whether any backend actually selects `DbClient::Threaded`
- **Location:** `src/libs/database/data_thread.rs:52-68`
- **What:**
  ```rust
  while let Some(x) = rx.recv().await {
      let DbExecutionQuery { request, result } = x;
      let result1 = request(&client).await;   // awaited to completion before the next recv
      let _ = result.send(result1);
  }
  ```
  The loop awaits each query fully before pulling the next off the channel. Effective database concurrency is **1**, regardless of `deadpool`'s pool size, behind a 100-slot bounded channel (`data_thread.rs:53`). Every query additionally costs two `Box<dyn Any + Sync + Send>` allocations, a `oneshot` channel, and a runtime downcast (`data_thread.rs:47-48`, `.expect("downcast failed")`). It also builds a full multi-threaded `tokio::runtime::Runtime` (`:56`) to run a strictly serial loop.
- **Why it matters:** Any service selecting `DbClient::Threaded` has its entire database throughput capped at one in-flight query. Under load the 100-slot channel fills and `execute` blocks on `tx.send`, silently converting a pool into a queue. If no backend uses it, this is 68 lines of dead public API that looks like a legitimate choice next to `Pooled`.
- **Fix:** First determine whether anything uses it (`rg 'spawn_thread_db_client|DbClient::Threaded'` across the six backends). If nothing does, delete it and the `DbClient` enum collapses to `PooledDbClient`. If something does, `tokio::spawn` each `request(&client)` inside the loop instead of awaiting it. Also replace `.map_err(|_| eyre!("send failed"))?` (`:44`), which discards the underlying error: exactly the context-loss pattern to avoid.
- **Effort:** S to delete, S to make it concurrent.
- **Blast radius:** `database.rs`, `data_thread.rs`. Deleting `DbClient::Threaded` is breaking; the enum is already `pub`.

### [SEV-8] Every database query eagerly formats an error message it usually discards

- **ID:** `endpoint-libs-perf-internals-08`
- **Severity:** Medium
- **Category:** Performance
- **Confidence:** High
- **Location:** `src/libs/database/pooled.rs:67-76`
- **What:**
  ```rust
  let rows = match tokio::time::timeout(Duration::from_secs(20), client.query(&statement, &req.params()))
      .await
      .context(format!(
          "timeout executing statement: {}, params: {:?}",
          req.statement(), req.params()
      ))? { ... }
  ```
  `eyre`'s `Context::context` takes its argument **by value**, so the `format!` is evaluated on every query, successful or not. It renders the full SQL text plus a `Debug` of every bound parameter into a `String` that is dropped microseconds later. `req.params()` is called twice (`:69` and `:74`), and `DatabaseRequest::params` returns `Vec<&dyn ToSql>` (`database.rs:80`), so that is two `Vec` allocations per query as well.
- **Why it matters:** This is the innermost loop of every data-driven endpoint. For a query with a 500-byte statement and eight parameters, it is a guaranteed multi-hundred-byte allocation plus `Debug` formatting of every value on the success path. It also means parameter values, which are exactly the fields most likely to be user data, are materialized into a string on every call.
- **Fix:** One-word change: `.with_context(|| format!(...))`. Bind `req.params()` once. While there, the two hardcoded `Duration::from_secs(20)` timeouts (`:62`, `:68`) should come from `DatabaseConfig`.
- **Effort:** S
- **Blast radius:** `pooled.rs` only.

### [SEV-9] The stale-prepared-statement recovery path clears a map that is never populated

- **ID:** `endpoint-libs-perf-internals-09`
- **Severity:** Medium
- **Category:** Correctness
- **Confidence:** Medium (I did not run it against a live Postgres with a mid-flight schema change)
- **Location:** `src/libs/database/pooled.rs:27` (field), `:85` (the only use), `:152` (initialised empty)
- **What:** `PooledDbClient.prepared_stmts: Arc<DashMap<String, Statement>>` is constructed empty at `:152` and **never written to**. `rg prepared_stmts src/` returns exactly three hits: the declaration, the `.clear()`, and the initialiser. Actual statement caching is done by `client.prepare_cached` (`:63`), which is `deadpool-postgres`'s per-connection cache. So when the retry logic detects `"cached plan must not change result type"` and does `self.prepared_stmts.clear()` (`:85`), it clears an always-empty map and invalidates nothing. The retry then calls `self.pool.get()` again, which may well hand back the *same* connection with the *same* stale cached statement.
- **Why it matters:** The code exists specifically to survive a schema migration under a running service. It logs "Database has been updated. Cleaning cache and retrying query" and then does not clean the cache. After two failed attempts it returns the original error (`:105`). The failure looks like a transient DB error rather than a stale-plan problem, so the operator's diagnosis starts in the wrong place.
- **Fix:** `deadpool-postgres` exposes the real caches. Use `client.statement_cache.clear()` on the checked-out client, or `pool.manager().statement_caches.clear()` to invalidate every pooled connection, which is what a schema change actually requires. Then delete the `prepared_stmts` field.
- **Effort:** S
- **Blast radius:** `pooled.rs`. `PooledDbClient` fields are private, so no API break.

### [SEV-10] Error aggregation: unbounded queue, a broken caller-location capture, and regex work under a write lock

- **ID:** `endpoint-libs-perf-internals-10`
- **Severity:** Medium
- **Category:** Correctness / Performance
- **Confidence:** High on all three sub-findings
- **Location:** `src/libs/log/error_aggregation.rs:104` (unbounded), `:319-329` (`find_or_first`), `:196-207` (lock scope), `:279-291` (regex chain)
- **What:** Three distinct defects in one file.
  1. **Unbounded channel on the error path.** `tokio::sync::mpsc::unbounded_channel()` (`:104`). Every `ERROR`-level tracing event allocates an `ErrorEntry` (two `String`s) and pushes it. If the aggregation task falls behind, and it will because it does regex work under a write lock, the queue grows without bound. An error storm becomes unbounded memory growth. The framework itself generates one error log per failed request (`basics.rs:161-169`), so a downstream outage is exactly the condition that triggers it.
  2. **`find_or_first` is the wrong function, and the field name is used instead of its value.**
     ```rust
     let caller_location = event.fields().find_or_first(|field| field.name() == "caller_location");
     let target = if let Some(location) = caller_location { format!("{location} ({target})") } else { target };
     ```
     `itertools::find_or_first` returns the first *matching* item, or the first item of the iterator when nothing matches, so for any error event with fields (all of them: `ws_server = true` is on every one) this is `Some(some_unrelated_field)`. And `location` is a `tracing::field::Field`, whose `Display` is its **name**, not its value. The resulting target is `"ws_server (my::target)"` or `"caller_location (my::target)"`, never the actual source location. The entire `CustomEyreHandler` to `caller_location` to aggregation pipeline documented at `log.rs:335-337` ("so that the original caller can be recorded within the displayed target") does not work, and worse, it pollutes the dedup key with an arbitrary field name.
  3. **Regex normalization inside the write lock.** `aggregation_task` takes `storage.write().await` at `:196` and then calls `normalize_message` at `:203`, which runs **seven** `replace_all` passes each producing a fresh `String` via `.to_string()` (`:279-291`): a minimum of eight allocations per error, all while holding the exclusive lock that `get_errors`, `count` and `clear` need.
- **Why it matters:** (1) is a memory-exhaustion path reachable from any condition that produces error logs at volume. (2) means the feature's most useful signal is silently absent and the dedup key is wrong. (3) makes (1) more likely.
- **Fix:** (1) Use a bounded channel with `try_send` and a dropped-count metric; losing an aggregated error under storm is strictly better than OOM. (2) Change to `.find(...)` and read the field's *value*: the value is only available through a `Visit` implementation, so fold the caller-location capture into `MessageVisitor` (`:352-364`) rather than iterating `fields()`. (3) Compute `normalize_message` before acquiring the lock.
- **Effort:** M
- **Blast radius:** `error_aggregation.rs`, plus the doc comment at `log.rs:335-337` which describes behaviour that never worked.

### [SEV-11] Stream fan-out re-serializes the payload once per subscriber

- **ID:** `endpoint-libs-perf-internals-11`
- **Severity:** Medium
- **Category:** Performance
- **Confidence:** High
- **Location:** `src/libs/ws/subs.rs:106-141` (`publish_to`), `:142-154` (`publish_to_key`), `:173-206` (`publish_with_filter`)
- **What:** `publish_to_key` looks up the subscriber set, **clones the whole `HashSet<ConnectionId>`** (`:147`, `.cloned()`), then calls `publish_to` per connection. `publish_to` calls `serde_json::value::to_raw_value(msg)` (`:118`), so a message broadcast to N subscribers is serialized N times. `publish_to_keys` (`:155-172`) does the same, cloning a `HashSet` per key. Separately, `publish_to` binds `let data = to_raw_value(msg)?` at `:118` and then writes `data: data.clone()` at `:131`; `data` is never used again, so the clone is a pure extra heap allocation plus memcpy on every publish.
  The irony: the *unreachable* `push.rs` (see SEV-13) gets this right. It serializes once at `push.rs:77` and clones the cheap `Box<RawValue>` per subscriber at `:95`.
- **Why it matters:** Subscription streams are the high-volume path in a trading or chat backend. A price tick delivered to 500 subscribers costs 500 serializations of the same value instead of one, plus 500 redundant clones, plus a `HashSet` clone per publish call.
- **Fix:** Serialize once and pass the `Box<RawValue>` down.
  ```rust
  fn publish_raw(&mut self, toolbox: &ArcToolbox, conn_id: ConnectionId, data: &RawValue) { ... }
  pub fn publish_to_key<Q>(&mut self, toolbox: &ArcToolbox, key: &Q, msg: &impl Serialize) {
      let data = match to_raw_value(msg) { ... };
      let ids: Vec<_> = self.mappings.get(key).map(|s| s.iter().copied().collect()).unwrap_or_default();
      for id in ids { self.publish_raw(toolbox, id, &data); }
  }
  ```
  Delete the `data.clone()` at `:131`. Also note `unsubscribe` (`:74-79`) iterates **every** key's set to remove one connection, which is O(total keys) per disconnect, and `publish_with_filter` calls it once per dead connection.
- **Effort:** M
- **Blast radius:** `subs.rs`. Public method signatures unchanged.

### [SEV-12] `EndpointSchema` is deep-cloned per request to hand a reference to hooks

- **ID:** `endpoint-libs-perf-internals-12`
- **Severity:** Medium
- **Category:** Performance
- **Confidence:** High
- **Location:** `src/libs/ws/session.rs:167` and `:267`; type at `src/model/endpoint.rs:12-48`; comment at `src/libs/ws/server.rs:50-51`
- **What:** `let schema = endpoint.schema.clone();` runs on the dispatch path of every request, unconditionally, even with zero hooks registered. `EndpointSchema` is not a cheap struct: `name: String`, `parameters: Vec<Field>`, `returns: Vec<Field>`, `description: String`, `roles: Vec<String>`, `errors: Vec<EndpointErrorSchema>`, `meta: MetaMap` (a `BTreeMap<String, Value>`), and `json_schema: serde_json::Value`. Each `Field` itself holds two `String`s and a `Type` that can be recursive. For a ten-parameter endpoint this is on the order of 25 to 40 heap allocations per request, entirely to produce a `&EndpointSchema` for `hooks.run_before`/`run_after`, which take it by reference.
  The doc comment at `server.rs:50-51` says an empty `Hooks` "adds one branch per request and nothing else". That is not true: the schema clone is unconditional. (The `hooks.clone()` at `session.rs:166` genuinely is cheap when empty, since cloning three empty `Vec`s does not allocate.)
- **Why it matters:** Framework overhead multiplied by every request of every backend, for zero benefit. It is also the single easiest win in the file.
- **Fix:** Store `Arc<EndpointSchema>` in `WsEndpoint` (`basics.rs:133-137`) and clone the `Arc`. `add_handler_erased` (`server.rs:120-142`) wraps it once at registration. `McpState::build` (`mcp.rs:277`) reads `&endpoint.schema` and is unaffected by the `Arc` deref. Fix the `server.rs:50-51` comment in the same change.
- **Effort:** S
- **Blast radius:** `basics.rs`, `server.rs`, `session.rs`, `mcp.rs`. `WsEndpoint` is `pub` with `pub` fields, so this is a breaking change to a type that is realistically only constructed by this crate and its tests.

### [SEV-13] Two dead modules: `deserializer_wrapper` (270 lines) and `ws/push.rs` (110 lines)

- **ID:** `endpoint-libs-perf-internals-13`
- **Severity:** Medium
- **Category:** AI-smell / Maintainability
- **Confidence:** High
- **Location:** `src/libs/deserializer_wrapper.rs` (whole file), `src/libs/ws/push.rs` (whole file), `src/libs/ws.rs:12` (`mod push;` with no re-export)
- **What:**
  1. `deserializer_wrapper.rs` is 242 lines of `serde::Deserializer` implementation in which **every single method** is `self.input.deserialize_xxx(visitor)`: pure delegation to `&serde_json::Value`, which already implements `Deserializer`. The wrapper adds nothing. `rg 'deserializer_wrapper|Deserializer::from_value'` finds two hits, the `pub mod` declaration and its own test. The test (`:255-269`) calls `println!("{r:?}")` and **asserts nothing**. This is a textbook generated abstraction that was never questioned.
  2. `ws/push.rs` declares `SubscribeManager`, `Subscribers`, `SubscriberContext`: a second, near-complete subscription system that duplicates `subs.rs`. It is declared `mod push;` in `ws.rs:12` with **no** `pub use push::*;`, so nothing in it is reachable from outside the crate, and nothing inside the crate references it. It opens with `#![allow(dead_code)]`, which is how it stays quiet.
- **Why it matters:** 380 lines that every reader and every agent must classify before ignoring. Worse, `push.rs` contains the *better* fan-out implementation (serialize once, clone the `RawValue`), so a maintainer who finds it may reasonably believe the good version is live. Two divergent copies of the same concept is exactly the drift the brief calls out.
- **Fix:** Delete `deserializer_wrapper.rs` and its `pub mod` line (it is already `#[deprecated]` at `libs.rs:27`, so the removal is signposted). For `push.rs`: either delete it, or port its serialize-once loop into `subs.rs` (SEV-11) and then delete it. Note `push.rs` also holds a DashMap shard write guard across the entire publish loop including `toolbox.send` (`push.rs:76-105`), so it should not be revived as-is.
- **Effort:** S
- **Blast radius:** `libs.rs`, `ws.rs`, two file deletions. `deserializer_wrapper` is `pub` and deprecated; removing it is technically breaking but nothing can be using it meaningfully.

### [SEV-14] README's OpenTelemetry example does not compile and documents a type that does not exist

- **ID:** `endpoint-libs-perf-internals-14`
- **Severity:** Medium
- **Category:** Docs
- **Confidence:** High
- **Location:** `README.md:461`, `:471`, `:491`; actual types at `src/libs/log/otel.rs:31-40`, `src/libs/log.rs:42-54`; exporters at `otel.rs:169` and `:206`
- **What:** Four separate errors in one 20-line block.
  1. `use endpoint_libs::libs::log::{LoggingConfig, OtelConfig, OtelProtocol};` and **`OtelProtocol` does not exist**. `rg OtelProtocol src/` returns nothing. `log.rs:35` re-exports only `otel::{OtelConfig, OtelGuards}`.
  2. `protocol: OtelProtocol::Grpc,` and `OtelConfig` has exactly four fields: `enabled`, `service_name`, `endpoint`, `headers` (`otel.rs:31-40`). There is no `protocol` field.
  3. `..Default::default()` on `LoggingConfig`, which derives only `Debug` (`log.rs:42`). There is no `Default` impl.
  4. The example's endpoint is `http://localhost:4317`, the OTLP **gRPC** port, but both exporters are hardcoded to `.with_http()` (`otel.rs:169`, `otel.rs:206`) and `opentelemetry-otlp` is compiled with only the `http-proto` feature (`Cargo.toml`). An HTTP/protobuf exporter pointed at 4317 will fail at export time; `build_otel_layer` catches init errors and logs a warning, but export failures are asynchronous and silent. Relatedly, `README.md:491` documents `OTEL_EXPORTER_OTLP_PROTOCOL` with values `grpc` or `http/protobuf`; `rg OTEL_EXPORTER_OTLP_PROTOCOL src/` returns nothing, so the variable is never read and gRPC is not reachable at all.
- **Why it matters:** This is the copy-paste starting point for enabling observability in a new backend. Each of the four errors surfaces at a different time (two at compile, one at runtime, one never), so a developer burns an afternoon on it. The gap here is not a typo; it describes a design that was planned and not built.
- **Fix:** Rewrite the block against the four real fields, use port `4318` (OTLP/HTTP), drop the `..Default::default()`, and either delete the `OTEL_EXPORTER_OTLP_PROTOCOL` row or implement it. While there: the logger endpoint fallback chain at `otel.rs:190-193` has a dead third arm (`.or_else(|| config.endpoint.clone())` after the same expression already ran first), so the README's "falls back to traces endpoint" is only true when `config.endpoint` is `None`.
- **Effort:** S
- **Blast radius:** `README.md`. Optionally `otel.rs` if you implement the protocol switch.

### [SEV-15] `examples/README.md` documents an example and a feature that do not exist

- **ID:** `endpoint-libs-perf-internals-15`
- **Severity:** Medium
- **Category:** Docs
- **Confidence:** High
- **Location:** `examples/README.md:5` ("Three small binaries"), `:15-21` (`ws_echo_native_tls`), `:27-35` (`--features native-tls`), `:41-45`; also `examples/test_ws_echo.sh` and `examples/ws_echo_ws_client.rs:4`
- **What:**
  - `ws_echo_native_tls` **does not exist**: no `examples/ws_echo_native_tls.rs`, no `[[example]]` entry in `Cargo.toml`. The doc gives a `cargo run` command for it and builds a three-step diagnostic procedure around it (`:41-45`).
  - `native-tls` **is not a feature**: `rg 'native.tls' Cargo.toml src/` returns nothing. Two of the three commands in the file pass `--features native-tls`.
  - `ws_echo_ws_client --native-tls`: the example takes exactly one positional argument and has no flags (`ws_echo_ws_client.rs:44-46`).
  - It claims "Three small binaries" while `examples/` contains five registered examples; `mcp_echo`, `uds_echo` and `ws_echo_server` are not mentioned at all.
  - `examples/ws_echo_ws_client.rs:4` documents `--features ws`, but `Cargo.toml`'s `[[example]] ws_echo_ws_client` declares `required-features = ["ws-client"]`.
  - `examples/test_ws_echo.sh` also invokes the nonexistent `ws_echo_native_tls`.
- **Why it matters:** Every command in this file except one fails. It is the first thing someone reads when trying to reproduce a TLS problem, and the entire "Comparing TLS backends" procedure is unrunnable. It also implies the crate has a native-tls option, which would change how someone diagnoses a Cloudflare fingerprinting issue.
- **Fix:** Rewrite `examples/README.md` against the five examples that exist and their real `required-features`; fix the usage line in `ws_echo_ws_client.rs:4`; delete or repair `examples/test_ws_echo.sh`. If native-tls support was removed deliberately, say so.
- **Effort:** S
- **Blast radius:** `examples/README.md`, `examples/test_ws_echo.sh`, one doc comment.

### [SEV-16] Both MCP migration playbooks are pinned to 1.9.1 across a breaking 2.0

- **ID:** `endpoint-libs-perf-internals-16`
- **Severity:** Medium
- **Category:** Docs
- **Confidence:** High
- **Location:** `docs/mcp-migration.md:1` and `:18-25` (version matrix); `/Users/revenge/code/endpoint-libs-mcp-migration-guide.md:1`, `:3`, `:20-26`; `/Users/revenge/code/api.support.cafe-mcp-plan.md:1`, `:3`, `:30-36`; contradicted by `README.md:87-89` and `docs/chain.md:82-84`
- **What:** All three documents target `endpoint-libs` **1.9.1**, `honey_id-types` **1.14.0**, `endpoint-gen` **1.9.0**. The crate is **2.1.3** (`Cargo.toml:3`), and 2.0 was breaking: `WsConnection.address: SocketAddr` became `peer: PeerIdentity`, schema model types became `#[non_exhaustive]`, `WsClient` futures became non-`Send`, and the `ws-wtx` backend was removed (all per `docs/2.0-migration.md`). None of the three guides mentions 2.0 exists. The external guide states as fact that "local reference clones live at `~/code/endpoint-libs` (v1.9.1)"; that clone is now 2.1.3, so an agent that reads the source there will see APIs contradicting the guide it is following.
  Meanwhile `README.md:87-89` says "as of this writing endpoint-libs 2.1, endpoint-gen 1.13 and honey_id-types 2.0 interoperate in production", and `README.md:260` points readers of that same README at `docs/mcp-migration.md`, which then tells them to pin 1.9. The two documents directly contradict each other about which versions to use.
- **Why it matters:** The brief is explicit that stale framework docs mislead every downstream agent, and the external guide is *written for* "a Claude Code session working on a repo it hasn't seen before". An agent following it will pin a backend to 1.9.1 and produce a lockfile that `docs/chain.md`'s check 1 (one `endpoint-libs` per graph) will flag against the 2.1 backends. `docs/chain.md:82-89` already records that the six backends are stuck declaring `[libs] 2.0.0`; adding 1.9 pins makes that worse.
- **Fix:** Add a dated "superseded" banner at the top of `docs/mcp-migration.md` pointing to `docs/2.0-migration.md`, and rewrite the version matrix to today's chain state. For the two files in `~/code`, either mark them superseded or move their durable content into this repo's `docs/`; `AGENTS.md:60-64` already says learned constraints belong in repo docs, not loose files. Note `docs/chain.md:78-89` is dated 2026-07-26 and cites EndpointValidator at 2.1.1, now two patches behind; that block needs a refresh cadence.
- **Effort:** M
- **Blast radius:** Three markdown files, two of them outside this repo (read-only for this review; flagging for the owner).

### [SEV-17] `types`-only pulls 246 crates; `tonic` and `hyper-rustls` are unused; `s3-sync` is a no-op for consumers

- **ID:** `endpoint-libs-perf-internals-17`
- **Severity:** Medium
- **Category:** Maintainability / Release hygiene
- **Confidence:** High
- **Location:** `Cargo.toml` `[dependencies]` (non-optional block), `Cargo.toml` `s3-sync = ["cert-provider/s3-sync"]` with `cert-provider` under `[dev-dependencies]`; `README.md:110` ("The default feature set is `types` only")
- **What:** Measured, not estimated:
  ```
  cargo tree -e normal --no-default-features --features types  -> 246 crates
  cargo tree -e normal --features full                          -> 344 crates
  ```
  71% of the maximal dependency graph is unavoidable. The non-optional set includes the entire OpenTelemetry stack (`opentelemetry`, `_sdk`, `-otlp`, `-semantic-conventions`, `-appender-tracing`, `tracing-opentelemetry`), `tonic`, `hyper-rustls`, `clap`, `alloy-primitives`, `rust_decimal`, and `tokio` with `features = ["full"]`. Three specific problems:
  - **`tonic 0.14`** is a direct dependency commented "Direct deps for header configuration". `rg tonic src/` returns exactly one hit: the string literal `"tonic"` in a log-filter list (`level_filter.rs:69`). No `use tonic` anywhere. It is already reachable transitively via `opentelemetry-proto`, so the direct declaration buys nothing.
  - **`hyper-rustls 0.27`** is non-optional with the comment "used by instant-acme inside cert-provider", but `cert-provider` is a **dev-dependency**, so this forces the dependency on every downstream consumer to satisfy a build-time need of this crate's own test suite. `rg hyper_rustls src/ examples/ tests/` returns nothing.
  - **`s3-sync = ["cert-provider/s3-sync"]`** references a dev-dependency. `cargo tree -e normal --features types | grep cert-provider` returns **0**, so cert-provider is not in any consumer's normal graph and enabling `s3-sync` from a downstream crate changes nothing at all. It compiles (verified: `cargo check --no-default-features --features s3-sync` succeeds) and does nothing. It is also the one feature `README.md:337-339` documents as a working capability.
- **Why it matters:** A consumer that wants only the schema model (`Type`, `Field`, `EndpointSchema`) to share types with `endpoint-gen`, the exact use case `README.md:112-118` describes for `types`, compiles a full OTLP exporter, a gRPC stack and an argument parser. That is build time and audit surface for every one of the six backends and both tool repos. The `s3-sync` flag is worse than useless: documented, reachable, and inert.
- **Fix:** Gate the OpenTelemetry block behind an `otel` feature and make `libs::log`'s OTel layer conditional (`log.rs:209-244` is the only integration point, and it already degrades to `Box::new(subscriber)` when the tracer is `None`). Drop the direct `tonic` dependency. Move `hyper-rustls` to `[dev-dependencies]` where its justification lives. Either make `cert-provider` a real optional dependency or delete the `s3-sync` feature and its README section. Also worth confirming: `Cargo.toml`'s `cargo-all-features` denylist excludes both `full` and `log_throttling`, so no CI combination ever builds `full`, which is what `README.md:402` tells users to run for the `uds_echo` example.
- **Effort:** M for the OTel gating, S for the rest.
- **Blast radius:** `Cargo.toml`, `log.rs`, `README.md`. Making OTel opt-in is a breaking change for anyone relying on it under the default features.

### [SEV-18] `BASE_HEADERS` caches the first server's config for the process lifetime

- **ID:** `endpoint-libs-perf-internals-18`
- **Severity:** Medium
- **Category:** Correctness
- **Confidence:** Medium (mechanism is certain; impact depends on whether any process runs two `WebsocketServer`s)
- **Location:** `src/libs/ws/tungstenite/upgrader.rs:31` (the static), `:42-82` (`get_or_init` reading `config`)
- **What:** `static BASE_HEADERS: OnceLock<HeaderMap>` is initialised inside `build_response` from the `config` of whichever request arrives first. Two config-derived values are baked in: `config.server_name` (via `option_env!("WS_SERVER_NAME").unwrap_or(&config.server_name)`, `:55`) and, critically, the branch at `:63`, `if config.allow_cors_urls.is_none()`, which decides whether the wildcard CORS header block is inserted. Every subsequent response, on every server instance in the process, gets a `.clone()` of that first map (`:82`).
- **Why it matters:** A process hosting two `WebsocketServer`s, one public with `allow_cors_urls: None` (wildcard) and one internal with a restricted list, will serve whichever config initialised first to both. If the wildcard server warms the cache, the restricted server emits `access-control-allow-origin: *`. `add_cors_headers` (`:382-435`) then correctly *also* appends the per-origin headers with `allow-credentials: true`, producing a response carrying both a wildcard and a specific origin. The `server_name` config field is similarly first-wins, making it effectively unconfigurable in a multi-server process. Separately, cloning a roughly seven-entry `HeaderMap` on every HTTP request is a per-request allocation that a `&'static HeaderMap` plus targeted `insert`s would avoid.
- **Fix:** Move the header template to a per-server field computed once in `WebsocketServer::new` (next to `cached_date`, `server.rs:68`) and thread it through `upgrade_stream`. If a global must stay, key it on the config rather than on first-caller-wins.
- **Effort:** M
- **Blast radius:** `upgrader.rs`, `traits.rs` (`WsUpgrader::upgrade_stream` signature), `server.rs`.

### [SEV-19] `WsClient` has no request timeout

- **ID:** `endpoint-libs-perf-internals-19`
- **Severity:** Medium
- **Category:** Design / Robustness
- **Confidence:** High
- **Location:** `src/libs/ws/client.rs:296-299` (`request`), `:242-294` (`recv_resp`)
- **What:** `request` is `send_req` then `recv_resp`, and `recv_resp` loops on `stream_next()` with no deadline. If the server accepts the frame and never answers (handler hangs, `NoResponse` returned by mistake, response dropped because the send buffer was full at `toolbox.rs:237-245`), the caller waits forever. The crate's own tests know this: every call site in `tests/transport_seam.rs` wraps it in `tokio::time::timeout` (`:171`, `:211`, `:228`, `:246`, `:479`).
- **Why it matters:** `endpoint-validator` and the backends' integration tests are the primary consumers. A hung request in a test suite is an unattributed CI timeout rather than a named failure. It also interacts badly with the drop-on-buffer-full path: `send_serialized_ws_msg` logs and discards when the queue is full (`toolbox.rs:237-245`), so a slow client legitimately produces a response that never arrives.
  Two adjacent design constraints worth naming: `recv_resp` matches only `resp.seq == self.seq` and `bail!`s on any other immediate response (`:256-258`), so the client cannot pipeline and a single stray frame kills it; and `send_req`/`recv_raw` log the **full body** at `debug!` (`:218`, `:237`), mirrored on the server at `session.rs:84`.
- **Fix:** Add a `timeout: Option<Duration>` to `WsClientBuilder` with a sane default, applied inside `recv_resp`. Non-breaking if the field defaults to `None` initially, though a default timeout is the better end state.
- **Effort:** S
- **Blast radius:** `client.rs`.

### [SEV-20] The error-response shape is hand-written 13 times; `ErrorCode` keeps three parallel lists with no consistency test

- **ID:** `endpoint-libs-perf-internals-20`
- **Severity:** Low
- **Category:** Design / Maintainability
- **Confidence:** High
- **Location:** `WsResponseError { ... }` literals: `src/libs/ws/session.rs` x4, `src/libs/ws/toolbox.rs` x4, `src/libs/ws/basics.rs` x2, `src/libs/ws/handler.rs` x1, `src/libs/ws/headers.rs` x1 (13 total per `rg -c "WsResponseError \{" src/`); `log_id.to_string()` appears 14 times; the `json!({"kind": ..., "message": ...})` payload appears 13 times. `ErrorCode` lists: `src/libs/error_code.rs:13-127` (39 consts), `:145-188` (39 `kind()` arms), `:197-238` (39 `from_name()` arms).
- **What:** Two related duplication problems.
  1. Every place that needs to send an error writes the same six-field literal plus the same `json!` payload. `Toolbox::encode_ws_response` (`toolbox.rs:329-372`) and `Toolbox::encode_handler_response` (`:374-422`) are near-identical 48-line functions differing only in how they destructure the error.
  2. `error_code.rs` maintains three hand-written lists of 39 entries each that must agree. Nothing enforces it: **there is no `#[cfg(test)]` block in `error_code.rs` at all**. Adding a constant without a `kind()` arm silently yields `"CustomError"`; a typo in `from_name` silently makes a code unparseable from RON.
- **Why it matters:** The error shape is the crate's public wire contract. Thirteen independent copies means a change to it (adding a `traceId`, say) is a thirteen-site edit with no compiler help for the one that is missed. The `ErrorCode` triple-list is a silent-drift generator on the enum that `endpoint-gen` emits `From` impls against.
- **Fix:** (1) One constructor: `WsResponseError::new(ctx, code, params)` taking `&RequestContext`, doing the `log_id.to_string()` and the `kind` insertion once; then collapse the two `encode_*` functions into one generic over the error branch. (2) A single `macro_rules!` table in `error_code.rs` emitting the const, the `kind()` arm and the `from_name()` arm from one row, or, minimally, a test that iterates a `&[(ErrorCode, &str)]` table and asserts `from_name(kind(c)) == Some(c)` for every constant.
- **Effort:** M
- **Blast radius:** `basics.rs`, `toolbox.rs`, `session.rs`, `handler.rs`, `headers.rs`, `error_code.rs`. Additive; existing constructors can stay.

### [SEV-21] Test coverage: what is strong, and the four specific gaps that matter

- **ID:** `endpoint-libs-perf-internals-21`
- **Severity:** Low
- **Category:** Maintainability
- **Confidence:** High
- **Location:** see per-item references
- **What:** The test suite is better than most of this codebase. `tests/transport_seam.rs` (549 lines) drives real handlers over a real duplex pipe and asserts on actual frames; `mcp.rs:413-668` has golden-frame assertions including the no-existence-oracle property (`:571-595`); `framed.rs:259-373` covers malformed and oversized frames in both directions. Those are real tests. The gaps:
  1. **Tautological tests.** `level_filter.rs:139-157` iterates all seven `LogLevel`s and asserts only `result.is_ok()`; it passes today while `LogLevel::Off` produces a TRACE filter (SEV-2). `level_filter.rs:160-168` asserts `is_ok()` then `drop(filter)` under the comment "The filter should exist and be usable". `deserializer_wrapper.rs:255-269` `println!`s its result and asserts nothing. `log.rs:717-744` ends with `let _ = guard.otel_guards;`.
  2. **A test whose name does not match what it checks.** `transport_seam.rs:505-549` is named `on_connect_hook_can_refuse_a_peer`, but `spawn_server` always supplies an attested peer (`:132-141`), so only the *admit* branch runs. The refusal path, the whole point of the hook and the path that skips `states.insert` (`server.rs:231-240`), is never exercised.
  3. **A test that locks in a bug.** `headers.rs:311-318` (`parse_comma_in_value`) asserts that `"0method,1val,ue"` parses to `{"0": "method", "1": "val", "u": "e"}`. That is `parse_protocol_header` splitting on `,` with no escaping, so any auth parameter containing a comma silently truncates and injects a garbage key. The test documents the breakage as intended behaviour rather than flagging it.
  4. **Untested high-risk framework behaviour.** In priority order: `ErrorCode` const / `kind()` / `from_name()` consistency (SEV-20, no test file at all); the `WsResponseValue::Error` frame shape for `NOT_IMPLEMENTED` and `FORBIDDEN` on the legacy path (`session.rs:130-162`), which is the wire contract every frontend parses and is asserted nowhere; `Toolbox::send` behaviour when the outbound buffer is full, including the `drop_conn_on_buffer_full` branch (`toolbox.rs:235-256`); and `WebsocketStates` insert-on-duplicate-id, which is where SEV-1 becomes visible.
- **Why it matters:** (1) and (2) create false confidence in exactly the two places where a real bug lives. (4) names the specific untested branches rather than asking for "more tests".
- **Fix:** Add a `refuses_an_unattested_peer` case that constructs `PeerIdentity::Local` with `Attestation::None` and asserts the client's first request fails. Replace the `is_ok()` assertions in `level_filter.rs` with assertions on the built filter's behaviour. Add the `ErrorCode` round-trip table test. Turn `parse_comma_in_value` into either a documented limitation with a `// KNOWN LIMITATION` comment or a fix.
- **Effort:** M
- **Blast radius:** Test files only.

## Cross-cutting recommendations

1. **Do a docs truth pass, starting with `AGENTS.md`.** Three of the six documentation findings above (SEV-4, SEV-14, SEV-15) are cases where the doc describes something that was never built or was removed. The repo already has the right instinct: `AGENTS.md:66-70` says "Docs describe what is true now" and `docs/chain.md:78-89` maintains an explicit known-red list. The gap is that nothing verifies it. Concretely: add a `scripts/check-docs.sh` alongside `check-chain.sh` that greps every `cargo run --example` and `--features` string out of the markdown and asserts each one is declared in `Cargo.toml`. That alone catches SEV-15 and half of SEV-17. *What would break:* nothing; it is additive and read-only.

2. **Make the hot path allocate once.** SEV-5, SEV-6 and SEV-12 are three independent copies of the same mistake: doing work eagerly that is only needed on a rare branch. Together they are roughly one full serialization, one full DOM parse and about 30 allocations per request, all removable without changing the wire protocol. Sequence: (a) move the `to_string` into the error arm and drop the `req.clone()` in `handle_mcp` (30 minutes, zero risk); (b) `Arc<EndpointSchema>` in `WsEndpoint`; (c) single-parse dispatch in `handle_message`. Then, if you want the real win, (d) `WsRequestGeneric<Box<RawValue>>` so params are never a DOM. *What would break:* (b) and (d) are breaking type changes to `pub` items; both are realistically only constructed by this crate.

3. **Delete the pre-2.0 stratum rather than maintaining it.** `deserializer_wrapper` (dead), `ws/push.rs` (unreachable), `ThreadedDbClient` (serial, probably unused), `database_test_config()` (a hardcoded `postgres` / `123456` in a published crate's public API, `database.rs:111-119`), `drop_and_recreate_database()` (shells out to `scripts/drop_and_recreate_database.sh`, which does not exist in this repo, `database.rs:121-128`). `TODO.md:6` already says "Remove or repurpose all deprecated code after confirming it is unused across dependent projects"; the confirmation step is one `rg` across the six backends plus the two tool repos, and `check-chain.sh` already knows where they are. *What would break:* each removal is a breaking change to a `pub` item, so batch them into one release.

4. **Make `types` actually minimal.** SEV-17's 246-versus-344 measurement is the argument. Gate the OpenTelemetry stack behind an `otel` feature; `log.rs:234-244` already has the exact branch point (`match (otel_tracer, &otel_guards)`), so the change is mostly `#[cfg]` plumbing plus removing four `use` lines. Drop `tonic`, move `hyper-rustls` to dev-deps, and resolve `s3-sync` one way or the other. *What would break:* any consumer relying on OTel under default features; that is a documented major, or a loud minor with the feature enabled in `full`.

5. **Give `AfterRequest` a real outcome, then say so in the docs.** SEV-3 is the finding most likely to cause an incorrect business decision downstream, because it fails silently and the type system actively suggests otherwise. Change `RequestHandlerErased`'s return type (it is `#[doc(hidden)]`), have the blanket impl in `handler.rs` map its existing three-way match, and add a test asserting a handler returning `HandlerError::Internal` produces `RequestOutcome::InternalErr`. *What would break:* hand-written `RequestHandlerErased` impls, which `handler.rs:75-77` implies exist ("keeps manual `RequestHandlerErased` impls source-compatible").

6. **Add a benchmark before optimizing further.** There is no `benches/` directory and no way to demonstrate any of the perf findings above numerically. A single criterion bench that drives `WsClient` against a `serve_connection` over `tokio::io::duplex`, using the harness that already exists at `tests/transport_seam.rs:143-154`, would turn "this allocates per request" into a number. It would also protect the fixes from regressing. *What would break:* nothing; a new dev-dependency.

## What I did not cover

- **Security.** No auth/authz analysis, no review of `check_roles`/`roles_allowed` semantics beyond noting they are duplicated (`session.rs:390-395` and `mcp.rs:399-404`), no crypto or TLS configuration review, no secrets audit. A sibling slice owns this. Two things I noticed in passing that the security reviewer should confirm rather than assume I covered: `config.rs:43` `println!("App config {config:#?}")` prints the entire deserialized config, including any `SecretString` fields, to stdout on every startup; and `connect_to_database` (`pooled.rs:149`) passes `NoTls` unconditionally while `DatabaseConfig.ssl_mode` (`database.rs:34`) is accepted and forwarded, so a configured `SslMode::Require` cannot work as intended.
- **Framework API design and ergonomics.** I did not evaluate whether `RequestHandler`, `SubAuthController` or the toolbox pattern are the right shapes; sibling slice.
- **`src/model/json_schema.rs` (545 lines) and `src/model/api_document.rs` (663 lines)** got a structural skim only. I verified the README's 2.1 claims name real functions (`SchemaComponents::collect`, `relocate_refs`, `apply_meta` all exist at `api_document.rs:67`, `:197`, `:248`) but did not verify their *behaviour* against the OpenAPI 3.1 or AsyncAPI 3.0 specs, nor the `$defs` self-containment claim at `README.md:323-325`.
- **`src/libs/ws/client.rs` connection setup** (lines 1-200 and 310-717: `WsClientBuilder`, TLS, HTTP/2 CONNECT). I reviewed the request/response path only.
- **`log_reader`, `scheduler`, `warn`, `datatable`, `pg_func`, `service`, `tls.rs`, `listener.rs`, `conn.rs`** were read for dead-code purposes, not audited.
- **I did not run the test suite.** `cargo check --offline` succeeded from cache; I did not run `cargo test`, `cargo clippy`, `./scripts/check-chain.sh`, or any feature-matrix build. Every claim above is from reading source plus `cargo tree` and `cargo check` metadata. The performance claims are structural ("this code runs per request", "this allocation is unconditional"), **not measured**. See cross-cutting recommendation 6.
- **The two `~/code` markdown files** were read read-only as instructed; I did not modify them and did not verify their claims against `api.support.cafe`'s current state.
- **`.claude/worktrees/magical-moser-b0466c/`** contains an older full copy of the crate (it still has `ws/wtx/`, removed in 2.1). I ignored it entirely; it is not part of the build.

## Quick-start for the follow-up agent

**Read in this order:**

1. `src/libs/ws/session.rs` (421 lines): the request dispatch loop. Everything per-request happens here or one call away. Findings 3, 5, 6 and 12 all land in `handle_message`.
2. `src/libs/ws/handler.rs` (224 lines): the erased-handler blanket impl. Small, and the `TODO` at line 100 is the thread to pull for the deserialization findings.
3. `src/libs/ws/toolbox.rs` (426 lines): `RequestContext`, the send closures, and the two near-duplicate `encode_*` functions.
4. `src/libs/ws/basics.rs` (185 lines): the wire types. `WsRequestValue = WsRequestGeneric<Value>` at line 36 is the root cause of the DOM round trip.
5. `tests/transport_seam.rs` (549 lines): read this before changing anything in 1 through 4; it is the acceptance test and the best documentation of intended behaviour.
6. `docs/chain.md`: before touching any public type. This crate breaks eight other repos silently.
7. `AGENTS.md`, but discount its first 25 lines per SEV-4.

**Commands (all verified to work on this machine):**

```bash
cargo check --offline --no-default-features --features types      # fast, cached
cargo check --offline --features full
cargo test --features full,framed-transport,ws-client             # includes the seam test
cargo tree --offline -e normal --no-default-features --features types --prefix none | sort -u | wc -l   # -> 246
./scripts/check-chain.sh --quick                                  # metadata only, read-only
```

Note `cargo test --all-features` is *claimed* fixed in 2.1 (`docs/2.0-migration.md`, gotcha 4) but nothing in CI exercises it: `Cargo.toml`'s `cargo-all-features` denylist excludes both `full` and `log_throttling`, and `max_combination_size = 3` caps the matrix at 232 of 2048 combinations. Verify before trusting.

**Surprises about the layout:**

- `src/libs/ws.rs` is the module list for `src/libs/ws/`. It declares `mod push;` (line 12) with **no** re-export, so `push.rs` is dead. Do not be misled into thinking it is the live subscription code; `subs.rs` is.
- `transport` is both `pub mod transport;` and `pub use transport::*;`, so every transport item has two public paths.
- `cargo` is at `/opt/homebrew/bin/cargo`; `~/.cargo/env` does not exist and every shell command in this repo prints a harmless `.zshenv` error about it. Ignore it.
- `.claude/worktrees/magical-moser-b0466c/` is a stale full copy of the crate. Exclude it from every `rg` and `find` (`--glob '!.claude'`) or your counts will be roughly double.
- `TODO.md` is short, current, and accurate, unusually for this repo. It already lists the `log_throttling` breakage and the deprecated-code removal.

## Nits

<details>
<summary>One line each, no action required individually.</summary>

- `src/libs/signal.rs:25-26`: `signal(SignalKind::terminate()).expect("")` panics with an empty message.
- `src/libs/log/level_filter.rs:55` and `:68`: `"h2"` is listed twice in `DIRECTIVES` with conflicting levels (INFO then WARN); the second wins.
- `src/libs/log/otel.rs:190-193`: the logger-endpoint fallback chain has a dead third arm repeating `config.endpoint.clone()`.
- `src/libs/ws/toolbox.rs:157`: `Toolbox::new()` returns `Arc<Self>`, which leads `subs.rs:232` to write `Arc::new(Toolbox::new())`, an `Arc<Arc<Toolbox>>` that only compiles via deref coercion.
- `src/libs/ws/server.rs:520-537`: `dump_schemas` creates and writes to a `docs/` directory relative to the process CWD.
- `src/libs/ws/server.rs:433-442`: the once-per-second date-cache task is spawned in `listen_impl` and never cancelled on shutdown; `handle_ws_handshake_and_connection:151` clones the cached `String` per connection.
- `src/model/types.rs:74`: `Field.meta` is documented "Empty in 2.0" but 2.1 applies field-level meta (commit `8dddad7`); the sibling comment at `endpoint.rs:44-45` was updated and this one was not.
- `src/libs/error_code.rs:136-143`: `to_u32()` and `code()` are exact duplicates.
- `src/libs/utils.rs:31-36`: `align_precision` computes precision by `format!` then `parse::<usize>()` then `format!` then `parse::<f64>()`, four string round trips with two `unwrap()`s.
- `src/libs/ws/headers.rs:185`: `parse_protocol_header` slices at byte index 1 (`&x[..1]`); safe for HTTP headers (hyper's `to_str` rejects non-visible-ASCII) but `serve_connection`'s `auth_protocol` argument is caller-supplied on local transports and is not.
- `src/libs/ws/headers.rs:212`: `splits.get(&index.to_string().as_str())` allocates a `String` per parameter per auth.
- `src/libs/ws/subs.rs:112` and `:138`: `let mut dead_connection = None;` declared before an early return and used trivially; defensive scaffolding.
- `src/model/types.rs:262-275`: `add_default_enum_derives` / `add_default_struct_derives` emit Rust source strings from the runtime crate, a codegen concern in the wrong layer.
- `src/model/types.rs:174-177` and `:213-219`: commented-out `DataTable` variant and constructor.
- `docs/chain.md:78-89`: "Known-red, as of 2026-07-26" cites EndpointValidator at 2.1.1; the crate is now 2.1.3.
- `README.md:341-345` lists `log_throttling` inside `full`, while `README.md:366-370` says the feature is non-functional and `Cargo.toml`'s all-features denylist excludes it; the `uds_echo` example command at `README.md:402` therefore enables a feature the README says not to use.

</details>
