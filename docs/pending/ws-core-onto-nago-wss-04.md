# `ws-core`, `ws` and `ws-client` onto `nago-wss`: 04

Date: 2026-09-22
Revision: 1. Successor of `ws-core-onto-nago-wss-03.md`. Completed work is
one line plus the proof commit. Do not copy line numbers out of 03.

## How this series works

This file is the queue. Work the highest number. When section 6 says to roll,
write `ws-core-onto-nago-wss-05.md` as a complete document, banner this file
`SUPERSEDED by <successor>`, and leave the old text alone. Do not open a second
live file at the same number.

**Update this file in place.** There is no locking. Stage it by name.

A **DECISION** item may not be started as code. P0.2, P0.3, P0.4 and P0.5
are settled. P0.1 is only half done. The next item that can land in
endpoint-libs is lane 3 item 10.

## 1. Where this sits

| Bucket | Location | Rule |
| --- | --- | --- |
| 1 | this file | Blocking for the port. Rolls to `ws-core-onto-nago-wss-05.md`. |
| 2 | `endpoint-libs/TODO.md` | Decided work that waits on this file. One pointer line back here is enough. |
| record | `endpoint-libs/docs/decisions/ws-core-onto-nago-wss.md` | A settled P0, including "decided not to". Append only. |

**Do not fold this port into endpoint-libs PR #52.** That PR is
`fix/restore-release-trigger`, 3.2.0 and tokio-optional, and merging it
publishes. endpoint-libs work stays on `fix/ws-core-lane1`, branched from
that branch at `8ccb574`. Nothing here waits on PR #52, and nothing here may
be smuggled into it.

## 2. State

Read on 2026-09-22 after the commits below, with `git rev-parse` and
`git status --short`. Re-read before trusting the table. Nothing here is pushed.

| Repo | Branch | HEAD | Version | Tree |
| --- | --- | --- | --- | --- |
| `~/code/endpoint-libs` | `fix/ws-core-lane1` | `04ffe23` | 3.2.0 | clean, apart from this file when it is the edit in progress |
| `~/code/nagoya` | `feat/resolve` | `ee8fca0` | 0.1.10 | clean |
| `~/code/nago-wss` | `feat/nagoya-resolve` | `6e52935` | 0.2.0 | clean |
| `~/code/nago-rustls` | `master` | `91a55f0` | 0.1.1 | clean |

The endpoint-libs row is the 03 roll. The commit that adds this file sits
directly on it and changes only the queue, the decision row and the TODO
pointer, so `git rev-parse` reads one commit ahead of that row.

`feat/resolve` is `fix/taskset-push-order` at `a0dd8ad`, plus name resolution
at `e0ff612`, plus the signal waiter at `ee8fca0`. That older branch is
unchanged. `feat/nagoya-resolve` is `bench/nagoya-write-path` at `3d069ed`
plus the caller commit. The bench branch is unchanged. Neither new branch
may be merged while nago-wss path-depends on nagoya or nago-rustls.

**The pins do not match the checkouts.** endpoint-libs depends on
`nagoya = "0.1.9"` from crates.io. The sibling checkout is 0.1.10 plus
`resolve`, `connect_any` and `Signal`, which 0.1.9 does not have. Cite
those against `feat/resolve`. `Notify::notify_waiters` is on the pinned
0.1.9.

**Do not give endpoint-libs a path dependency on the sibling.** A path
dependency must not reach master, and this branch is the one that would
carry it there. P0.1's caller and item 12 land when a published nagoya
contains the functions they call.

**The nagoya dependency enables `reactor` on the dependency line**, not on the
`nagoya-transport` feature. `signal` names `dep:nagoya`, so a `signal`-only
build compiles the reactor. `ws-core` requires `signal`.

**nago-wss cannot be released from this checkout.** It path-depends on
`nago-rustls` and `nagoya`. Its lock records nagoya 0.1.10 for that path.
A published nago-wss is the gate on merging any endpoint-libs commit that
names it.

Release order in `docs/release-order.md` is unchanged. No commit this series
has bumped endpoint-libs off 3.2.0. Nagoya was not bumped off 0.1.10.
`./scripts/check-chain.sh` was not run. `./scripts/check-features.sh` was not
re-run; the last pass was `cargo check --offline --lib` on `9f43c32`.

## 3. Corrections still true

Re-derive a line from the identifier before editing. Do not copy a number
from 03.

- `tokio::select!` is gone under `src/`. The replacement is
  `futures::future::select`, left-biased, in a block that ends before the
  arms touch `self`.
- `nagoya::reactor::resolve` and `connect_any` are public on `feat/resolve`.
  `resolve` returns every address, with the caller's port stamped on. An
  empty `connect_any` fails with `ECONNREFUSED`. nago-wss maps `Nul` to the
  URL error and every other resolve failure to `EAI_NONAME`.
- `nagoya::reactor::Signal` is the unix waiter (`ee8fca0`). Linux is
  `signalfd`, and the block outlives the waiter. The BSDs are a pipe and a
  handler; drop restores the previous disposition; the pipe stays open for
  the process. `EVFILT_SIGNAL` does not fit `Registration`. One waiter per
  number (`EBUSY`). `pthread_sigmask` is per thread: later threads inherit
  the block, existing threads do not.
- `Addr` is `V4` / `V6` / `Path`. A Unix peer has no arm on
  `PeerIdentity::Network`.
- Server handshake `Request` (`path`, `key`, `protocols`) and client
  `ClientOptions` are different types. nago-wss `build_response` is a fixed
  101. `build_rejection` is a fixed 400, 426 or 431.
- `nagoya::time::sleep` is a future constructor, not an `async fn`.
- `Connection<S>` is one owned object. Write methods take `&mut self`.
  `Toolbox::send` runs on an arbitrary task. The per-connection queue is the
  route. P0.3 kept it.
- `notify_waiters` wakes the current set and leaves no permit. `Shutdown`
  stores the `AtomicBool` first. `cancelled` calls `Notified::enable` before
  it reads the flag.
- `cargo check --all-targets` unifies the dev-dependency `tokio` with
  `features = ["full"]`. `cargo check --lib` does not. `cargo tree -e normal`
  does not either.

### Channels

The dead receiver stays deleted (`813c795`). None of the live ones is a bare
`()`. The queues themselves are unchanged. `set_ws_states` is what passes
`Some(&state.end)` into `enqueue`. The public `send_ws_msg` /
`send_serialized_ws_msg` pass `end = None`.

| Site | Carries | Class |
| --- | --- | --- |
| `server.rs` per-connection `mpsc::channel` | `WsMessage` | Payload. Still tokio. |
| `conn.rs` `WebsocketStates::insert` and `WsStreamState.message_queue` | `WsMessage` | Payload. Public. Also stores `end: Arc<Shutdown>`. |
| `toolbox.rs` `send_ws_msg` / `send_serialized_ws_msg` | `WsMessage` | Payload. Public. |
| `session.rs` `WsClientSession.rx` | `WsMessage` | Payload. |
| `server.rs` shard channel, depth 256 | accepted socket | Payload. `SO_REUSEPORT` is still absent. |
| `traits.rs` `upgrade_stream` | `UpgradeEvent` | Public trait. H1 wants a value. H2 stays on this receiver (P0.5). |
| tungstenite upgrader channel of depth 2 | `UpgradeEvent` | Producer for the row above. |

## 4. The queue

### Done

1. **Delete `message_receiver`.** `813c795`.
2. **Replace `tokio::select!`.** `138cdb1`.
3. **Replace `CancellationToken`.** `5d6ae8e`. The flag is `Shutdown`. The unix waiter is nagoya's; `libs/signal.rs` still calls `tokio::signal` until item 12.
4. **Teardown and policy-close.** `1a35b49`.
5. **`TOOLBOX` per poll.** `b55e62b`.
6. **Drop `Date`.** `9f43c32`.

**P0.2. Signal delivery in nagoya.** `ee8fca0`. See the decision row.

P0.3, P0.4 and P0.5 are rows in `docs/decisions/ws-core-onto-nago-wss.md`, commit `8f08cfe`. The queue stays in this crate. `Date` is the only dropped HTTP extra. HTTP/2 extended CONNECT stays on `ws` and `ws-client`.

### Still open

- **P0.1. Where resolution lives.** Half done. The public functions are
  nagoya `e0ff612`. nago-wss `6e52935` is the caller there. endpoint-libs
  still calls `tokio::net::lookup_host` from `listen` and from `connect_h2`.
  Settled only when a caller in endpoint-libs names `nagoya::reactor::resolve`,
  against a published nagoya. No path dependency.

### Lane 3. Not started

Do not merge this lane to master while nago-wss path-depends on nagoya or
nago-rustls. A branch against the path checkout is allowed. Item 10 does not
touch nago-wss. Item 12 does not either, and it also does not get a path
dependency on nagoya: `Signal` is not in the pinned 0.1.9.

7. **`MessageStream` over `nago_wss::Connection<S>`.** Blocked on the publish gate for a merge.
8. **`Addr` to `SocketAddr` for `V4` and `V6` only.** `Path` is an error that names the family. `PeerIdentity` is public. `listen` needs this before it can bind a tokio listener from `resolve`.
9. **Listener and shards.** A second implementation over `nagoya::reactor::TcpListener`. One listener per shard deletes the accept fan-out, and that option does not exist in nagoya. Do not invent `SO_REUSEPORT` at the call site.
10. **Outbound path.** P0.3 settled this: the queue stays, owned here, bounded, `try_send` `Full` and `Closed`, not a third channel crate and not `futures` (a slot per sender would move `drop_conn_on_buffer_full`). `Outbound::Closed` and `WsStreamState::end` stay. Unblocked. Not started. This is the next item that can land on `fix/ws-core-lane1`.
11. **H1 upgrade.** `upgrade_stream`'s receiver becomes a plain value for H1. H2 stays on hyper (P0.5). The kept extras in P0.4 (`Server`, `Cache-Control` on errors, CORS, `OPTIONS`, `HEAD`, `Origin`) have to be carried onto nago-wss `build_response` / `build_rejection`, and `Request` has to retain `Origin`. `Date` does not come back.
12. **`signal` delivery, now that P0.2 is settled.** Rewrite `libs/signal.rs` against `nagoya::reactor::Signal`. Drop `tokio/signal` from the `signal` feature. The `Shutdown` flag stays. The waiter takes `&Handle`. Blocked on a published nagoya that contains `Signal`. No path dependency. Not started.
13. **`ws-client`.** `connect_plain` and `connect_secure` cover the H1 client. `connect_h2` stays on hyper (P0.5).

### Lane 4

14. **`ws-core` stops naming `dep:tokio`.** Not true on `04ffe23`. The karen tree still prints tokio.
15. **`./scripts/check-features.sh` and `./scripts/check-chain.sh`.** The `--lib` form of the feature script passed on `9f43c32`. `check-chain.sh` has not. The `framed_json` break that requires `framed-transport-tokio` already shipped on the 3.2.0 branch. This lane must not add another silent one.

Removing `parking_lot`, `dashmap` and `async-trait` from `ws-core` is a
different change. Do not bundle it.

## 5. Definition of done

- Lane 1, item 6 and P0.2 are committed, not merged, and not on PR #52. P0.2 is nagoya `ee8fca0`. The endpoint-libs commits are on `fix/ws-core-lane1`.
- P0.2, P0.3, P0.4 and P0.5 have rows in `docs/decisions/ws-core-onto-nago-wss.md`. P0.1 does not.
- The two smuggled edges have a test each, on `1a35b49`. The signal waiter has its tests on `ee8fca0`.
- Under the feature set karen uses, `cargo tree -e normal -i tokio` prints tokio (`net`, `rt`, `sync`, `signal`). That was measured on `9f43c32` and no endpoint-libs dependency has changed since. P0.5 kept hyper on `ws` / `ws-client`, which karen does not enable.
- `check-features.sh` passed on `9f43c32` with `cargo check --offline --lib`. It was not re-run. `check-chain.sh` has not been run.
- No path dependency on nago-wss, nago-rustls or nagoya has reached master. The nago-wss branch still has its path dependencies. endpoint-libs still pins crates.io nagoya 0.1.9.

## 6. Carrying this file

A turn ends when the lane it took is empty, when a measurement refutes the
item, or when the next item is a P0 nobody has settled. The next item that
can be written on `fix/ws-core-lane1` is item 10. Do not start item 12
against the 0.1.9 pin, and do not start item 11's H2 half.

At the start: read the highest-numbered file in this series,
`endpoint-libs/AGENTS.md`, and `git status --short` plus HEAD for the four
repos in section 2. Resume the first unfinished item that is not waiting on
a publish. Do not redo a done item unless the tree contradicts its proof
commit.

At the end: bump the revision, prune finished items to one line, and record
what was not run.

**Roll this file** when item 10 lands, or when item 12 lands. The successor
is the next number and is a complete document. Banner this file. Leave the
old text. Do not rename P0.1 through P0.5 or the lane numbers.

Line numbers in 03 were read at older HEADs. Re-derive a line from the
identifier before editing it.

### Not run, this roll

- `./scripts/check-features.sh` and `./scripts/check-chain.sh`. No endpoint-libs Rust changed.
- The Linux `signalfd` path. It typechecked. It did not run.
- A version bump, a publish, and any push.
