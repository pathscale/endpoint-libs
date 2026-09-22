SUPERSEDED by `ws-core-onto-nago-wss-03.md`. Work that file. Do not mine this one for line numbers.

# `ws-core`, `ws` and `ws-client` onto `nago-wss`: 02

Date: 2026-09-22
Revision: 2. This edit is the superseded banner only. The queue text below is
revision 1 and is frozen.

## How this series works

This file is the queue. Work the highest number. When section 6 says to roll,
write `ws-core-onto-nago-wss-03.md` as a complete document, banner this file
`SUPERSEDED by <successor>`, and leave the old text alone. Do not open a second
live file at the same number.

**Update this file in place.** There is no locking. Stage it by name.

A **DECISION** item may not be started as code. Lane 1 and lane 2 items 4 and 5
are done. Item 6 and lanes 3 and 4 are not.

## 1. Where this sits

| Bucket | Location | Rule |
| --- | --- | --- |
| 1 | this file | Blocking for the port. Rolls to `ws-core-onto-nago-wss-03.md`. |
| 2 | `endpoint-libs/TODO.md` | Decided work that waits on this file. One pointer line back here is enough. |
| record | `endpoint-libs/docs/decisions/` | A settled P0, including "decided not to". Append only. |

**Do not fold this port into endpoint-libs PR #52.** That PR is
`fix/restore-release-trigger`, 3.2.0 and tokio-optional, and merging it
publishes. This work is on `fix/ws-core-lane1`, branched from that branch at
`8ccb574`. Nothing here waits on PR #52, and nothing here may be smuggled into
it.

## 2. State

Read on 2026-09-22 after the commits below, with `git rev-parse` and
`git status --short`. Re-read before trusting the table. Nothing here is pushed.

| Repo | Branch | HEAD | Version | Tree |
| --- | --- | --- | --- | --- |
| `~/code/endpoint-libs` | `fix/ws-core-lane1` | `b55e62b` | 3.2.0 | clean, apart from this file when it is the edit in progress |
| `~/code/nagoya` | `fix/taskset-push-order` | `a0dd8ad` | 0.1.10 | clean |
| `~/code/nago-wss` | `bench/nagoya-write-path` | `3d069ed` | 0.2.0 | clean |
| `~/code/nago-rustls` | `master` | `91a55f0` | 0.1.1 | clean |

01's table said endpoint-libs was `fix/restore-release-trigger` at `27e75e7`.
That was already stale at the start of the turn that wrote this file: HEAD was
`8ccb574` on that branch, which is `27e75e7` plus the commit that added 01.
The port then moved to `fix/ws-core-lane1`.

**The pins do not match the checkouts.** endpoint-libs depends on
`nagoya = "0.1.9"` (`Cargo.toml`, the `nagoya` dependency). The sibling
checkout is 0.1.10. Cite 0.1.10 only for the sibling tree. `Notify::notify_waiters`
was confirmed on the crates.io source of 0.1.9, `src/sync.rs` at the
`pub fn notify_waiters` item. The sibling has the same item.

**The nagoya dependency enables `reactor` on the dependency line**, not on the
`nagoya-transport` feature. `signal` now names `dep:nagoya`, so a `signal`-only
build compiles the reactor. That split was not made. `ws-core` requires
`signal`, so every `ws-core` consumer now links nagoya plus its reactor.

**nago-wss cannot be released from this checkout.** It path-depends on
`nago-rustls` and `nagoya`, and those lines must not reach master. A published
nago-wss is the gate on merging any endpoint-libs commit that names it.

Release order in `docs/release-order.md` is unchanged. Lane 1 and lane 2 did
not bump the version. It is still 3.2.0. `./scripts/check-chain.sh` was not run.

## 3. Corrections still true

Checked in 01 against the trees, not re-walked this turn except where a commit
above touched the file. Heads of nagoya, nago-wss and nago-rustls are the same
as 01. Re-derive every line from the identifier before editing. Do not copy a
number from 01.

- `tokio::select!` is gone under `src/`. The replacement is
  `futures::future::select`, which is left-biased. Shutdown is polled before
  accept. SIGTERM is polled before SIGINT. The session polls outbound, then
  inbound, then a finished handler, then the policy flag.
- `pin_mut` holds its borrow to the end of the scope. The session select lives
  in a block that ends before the arms touch `self`. A future left in the loop
  body does not compile: `rx.recv()` and `conn.recv()` are already borrowing it.
- `futures::future::select` requires `Unpin`. `Pin<&mut F>` satisfies that.
- `fn resolve` / `connect_any` in nago-wss were private in 01. Not re-read.
- nagoya has no unix signal module. `struct Signal` in `block_on.rs` is the
  parker's waker. Not re-read this turn.
- `Addr` is `V4` / `V6` / `Path`, not a `SocketAddr`. A Unix peer has no arm on
  `PeerIdentity::Network`. Not re-read this turn.
- Server handshake `Request` and client `ClientOptions` are different types.
- `nagoya::time::sleep` is a future constructor, not an `async fn`.
- `Connection<S>` is one owned object. `Toolbox::send` runs on an arbitrary
  task. The per-connection queue is still the only route from a handler to a
  socket. P0.3 is still open.
- `notify_waiters` wakes the current set and leaves no permit. A waiter that
  arrives later does not observe it. `Shutdown` stores an `AtomicBool` first,
  then calls `notify_waiters`. `cancelled` calls `Notified::enable` before it
  reads the flag. That method is public on nagoya 0.1.9.
- `cargo check --all-targets` and `cargo test` unify the dev-dependency
  `tokio` with `features = ["full"]`. A feature can look like it compiles
  without `tokio/macros` when only tests are built. `cargo check --lib` does
  not pull dev-dependencies. Item 5 was checked that way.

### The twelve channel sites

Still the map, minus the dead receiver. None of the live ones is a bare `()`.
Re-derive lines from the identifiers. The two edges that used to be smuggled
on the queue now have homes (item 4). The queues themselves are unchanged.

| Site | Carries | Class |
| --- | --- | --- |
| `server.rs` per-connection `mpsc::channel(config.message_buffer_size)` | `WsMessage` | Payload. Still there. |
| `conn.rs` `WebsocketStates::insert` | `WsMessage` | Payload. Public. Also stores `end: Arc<Shutdown>` now. |
| `conn.rs` `WsStreamState.message_queue` | `WsMessage` | Payload. Public field. |
| `toolbox.rs` `send_ws_msg` | `WsMessage` | Payload. Public. Passes `end = None`. |
| `toolbox.rs` `send_serialized_ws_msg` | `String` to `WsMessage` | Payload. Public. Passes `end = None`. |
| `session.rs` `WsClientSession.rx` | `WsMessage` | Payload. |
| `server.rs` shard channel, depth 256 | accepted socket | Payload. `SO_REUSEPORT` is still absent. |
| `traits.rs` `upgrade_stream` | `UpgradeEvent` | Public trait. H1 wants a value. H2 wants a stream. |
| tungstenite upgrader channel of depth 2 | `UpgradeEvent` | Producer for the row above. |
| `WebsocketServer::message_receiver` | `ConnectionId` | Deleted in `813c795`. Do not port. |

`set_ws_states` is the path that passes `Some(&state.end)` into `enqueue`.
A caller of the public `send_ws_msg` / `send_serialized_ws_msg` still only
touches the queue.

## 4. The queue

### Done

1. **Delete `message_receiver`.** `813c795`. The identifier is gone from Rust,
   `Cargo.toml`, `README.md` and `transport.rs`. The unused `ConnectionId`
   import that deletion left in `server.rs` went out in `138cdb1`.
2. **Replace `tokio::select!`.** `138cdb1`. `rg 'tokio::select!'` under `src/`
   is empty. Session tests in `tests/transport_seam.rs` passed
   (`framed-transport-tokio,ws-client`, 5 tests). This does not remove tokio.
3. **Replace `CancellationToken`.** `5d6ae8e`. `signal` no longer names
   `tokio-util`. `CANCELLATION_TOKEN` is a `Shutdown` (`AtomicBool` plus
   `nagoya::sync::Notify`). `cancel` / `is_cancelled` / `cancelled` are the
   methods. Unix delivery is still `tokio::signal`. In-tree callers use
   `init_signals` and `wait_for_signals`, not the old token type.
4. **Teardown and policy-close.** `1a35b49`. `classify_outbound(None)` is
   `Outbound::Closed`, and dropping every sender ends the session.
   `WsStreamState::end` is an `Arc<Shutdown>` created in `insert`. The session
   gets it through `bind_end`. `WsClientSession::new` is unchanged and holds a
   private flag nobody else can set. A policy close still `try_send`s
   `Message::Close(None)` behind payloads, and also `cancel`s `end`. The
   session drains the queue before it honours the flag, and the flag is the
   last select arm. Tests: `dropping_the_outbound_sender_ends_the_session`,
   `policy_flag_ends_a_session_blocked_on_recv`,
   `buffer_full_policy_cancels_without_a_free_slot`,
   `header_only_cancels_even_when_the_frame_fits`,
   `a_full_buffer_without_the_policy_does_not_cancel`.
5. **`TOOLBOX` task-local.** `b55e62b`. `tokio::task_local!` is gone.
   `ToolboxKey` installs a `thread_local` for one poll and restores it on
   return, including panic and drop. `scope`, `with` and `try_with` are the
   surface. The rest of tokio's `LocalKey` (`sync_scope` and friends) was not
   reimplemented. `scoped-tls` was not added. Tests:
   `scope_sees_the_toolbox_on_every_poll_and_clears_it_after`,
   `dropping_a_pending_scope_leaves_the_slot_empty`,
   `scope_is_the_same_toolbox_after_an_await`. `cargo check --lib --features
   ws-core --offline` passed, so this does not depend on the dev-dependency's
   `tokio/macros`.

`./scripts/check-features.sh` passed on `b55e62b` with
`CHECK_CMD="cargo check --all-targets --offline"`. Every feature, the default,
and `--no-default-features`. `./scripts/check-chain.sh` was not run.
`cargo tree -e normal -i tokio` was not run. Do not report this branch as the
tokio removal. `ws-core` still names `dep:tokio`.

### P0. Decisions. Do not write the lane 3 code while any of these is open

No row was added to `docs/decisions/`. All five are open.

- **P0.1. Where resolution lives.** `fn resolve` and `connect_any` are private
  in nago-wss. The crate root says they belong in nagoya. Settled by a public
  function in one of those two crates, and a caller in endpoint-libs that
  names it.
- **P0.2. Signal delivery in nagoya.** `EVFILT_SIGNAL` / `signalfd` are unwritten.
  New `unsafe` reactor code. Settled by a nagoya waiter for `SIGINT`,
  `SIGTERM` and `SIGHUP` without tokio. The flag half does not wait on it.
  That half is item 3 above.
- **P0.3. The writer.** `Connection<S>` is one owned object. `Toolbox::send`
  runs on an arbitrary task. Settled by a clonable writer in nago-wss, or a
  bounded `WsMessage` queue owned by endpoint-libs with `try_send` `Full` and
  `Closed`, and not a third channel crate. A `futures` bounded channel reserves
  a slot per sender, so `drop_conn_on_buffer_full` would fire at a different
  depth. The public `send_*` functions still do not take the `Shutdown`.
- **P0.4. Which HTTP extras survive.** `Date`, `Server`, `Cache-Control`, CORS,
  `OPTIONS`, `HEAD`, `Origin`. Settled by a row per header in `docs/decisions/`.
  A drop of `Date` deletes `cached_date` and its one-second `tokio::spawn`. A
  keep means `build_response` gains extra headers. Both are upstream of item 6.
- **P0.5. HTTP/2 extended CONNECT.** nago-wss has no path to it. Settled by
  dropping multiplexing, or keeping hyper on its own feature and accepting
  tokio on that feature. "Port it later" is not a settlement.

### Lane 2. Still open

6. **`Date` cache, only after P0.4.** If `Date` is dropped, delete `cached_date`
   and the spawn. If it is kept, the refresh is `nagoya::time::sleep` on a shard
   thread, not `tokio::spawn`. Do not start this while P0.4 is open.

### Lane 3. Redesign. Blocked on the P0 it names, and on a published nago-wss

Do not merge this lane to master while nago-wss path-depends on nagoya or
nago-rustls. A branch against the path checkout is allowed. Not started.

7. **`MessageStream` over `nago_wss::Connection<S>`.** Newtype, `async_trait(?Send)`,
   `Message` to `WsMessage` over `Bytes`. Blocked on the publish gate for a merge.
8. **`Addr` to `SocketAddr` for `V4` and `V6` only.** `Path` is an error that
   names the family. It touches `PeerIdentity`, which is public.
9. **Listener and shards.** A second implementation over `nagoya::reactor::TcpListener`,
   not a swap of `tokio::net::TcpListener`. One listener per shard deletes the
   accept fan-out. That option does not exist in nagoya yet. Do not invent
   `SO_REUSEPORT` at the call site.
10. **Outbound path, per P0.3.** Cloned writer and the per-connection queue goes
    away, or the queue stays, owned here, bounded, with `Full` and `Closed`.
    `Outbound::Closed` and `WsStreamState::end` are still delivered.
11. **H1 upgrade.** `upgrade_stream`'s `Receiver<UpgradeEvent>` becomes a plain
    value for H1. H2 follows P0.5.
12. **`signal` delivery, after P0.2.** Rewrite `libs/signal.rs` against nagoya.
    Drop `tokio/signal` from the `signal` feature. The `Shutdown` flag stays.
13. **`ws-client`.** `connect_plain` and `connect_secure` cover the H1 client.
    `connect_h2` is P0.5.

### Lane 4. After the tree can actually change

14. **`ws-core` stops naming `dep:tokio`.** Not true on `b55e62b`.
15. **`./scripts/check-features.sh` and `./scripts/check-chain.sh`.** The feature
    script passed on `b55e62b` (see Done). `check-chain.sh` has not. The
    `framed_json` break that requires `framed-transport-tokio` already shipped
    on the 3.2.0 branch. This lane must not add another silent one.

Removing `parking_lot`, `dashmap` and `async-trait` from `ws-core` is a
different change. Do not bundle it.

## 5. Definition of done

- Lane 1 is on `fix/ws-core-lane1`, not merged, and not on PR #52.
- P0.1 through P0.5 each have a row in `docs/decisions/`. They do not.
- The two smuggled edges have a test each. They do, on `1a35b49`.
- Under the feature set karen uses, `cargo tree -e normal -i tokio` prints
  nothing, or P0.5 decided that the hyper feature keeps tokio and karen does
  not enable that feature. Not run. The tree still contains tokio.
- `check-features.sh` passed on `b55e62b`. `check-chain.sh` did not.
- No path dependency on nago-wss, nago-rustls or nagoya has reached master.

## 6. Carrying this file

A turn ends when the lane it took is empty, when a measurement refutes the
item, or when the next item is a P0 nobody has settled. The next unfinished
item is P0.1, or item 6 once P0.4 has a row. Do not start lane 3 while a P0
it names is open.

At the start: read the highest-numbered file in this series,
`endpoint-libs/AGENTS.md`, and `git status --short` plus HEAD for the four
repos in section 2. Resume the first unfinished item. Do not redo a done item
unless the tree contradicts its proof commit.

At the end: bump the revision, prune finished items to one line, and record
what was not run.

**Roll this file** when item 6 is a stub, or when a P0 settlement changes the
shape of lane 3. The successor is the next number and is a complete document.
Banner this file. Leave the old text. Do not rename P0.1 through P0.5 or the
lane numbers.

Line numbers in 01 were read at older HEADs and have drifted. Re-derive a line
from the identifier before editing it.
