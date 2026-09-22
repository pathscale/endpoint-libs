SUPERSEDED by `ws-core-onto-nago-wss-06.md`. Work that file. Do not mine this one for line numbers.

# `ws-core`, `ws` and `ws-client` onto `nago-wss`: 05

Date: 2026-09-22
Revision: 1. Successor of `ws-core-onto-nago-wss-04.md`. Completed work is
one line plus the proof commit. Do not copy line numbers out of 04.

## How this series works

This file is the queue. Work the highest number. When section 6 says to roll,
write `ws-core-onto-nago-wss-06.md` as a complete document, banner this file
`SUPERSEDED by <successor>`, and leave the old text alone. Do not open a second
live file at the same number.

**Update this file in place.** There is no locking. Stage it by name.

The end state is that endpoint-libs does not depend on tokio, on any feature.
There is no accepted leftover. The replacements are published crates: nagoya,
nago-wss, nago-rustls, and the other Pathscale forks. Versions are written in
`Cargo.toml`. This repository does not have a `Cargo.lock`. A sibling path
(`../nagoya`, `../nago-wss`, `../nago-rustls`, `../parking_lot_lite_hack`) is
not a way to get an API that crates.io does not have yet.

A **DECISION** item may not be started as code. P0.2, P0.3, P0.4 and P0.5
have rows. The "keep tokio" half of P0.5 is withdrawn; see the decision file.
P0.1 is only half done. crates.io nagoya `0.1.9` has no `Signal`, `resolve`,
or `connect_any`, so item 12 and the endpoint-libs caller for P0.1 wait on
a published nagoya that contains them.

## 1. Where this sits

| Bucket | Location | Rule |
| --- | --- | --- |
| 1 | this file | Blocking for the port. Rolls to `ws-core-onto-nago-wss-06.md`. |
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
No repository in this table was given a new path dependency by these commits.
nago-wss, nagoya and nago-rustls were not edited.

| Repo | Branch | HEAD | Version | Tree |
| --- | --- | --- | --- | --- |
| `~/code/endpoint-libs` | `fix/ws-core-lane1` | `3531fd6` | 3.2.0 | clean, apart from this file when it is the edit in progress |
| `~/code/nagoya` | `feat/resolve` | `ee8fca0` | 0.1.10 | clean |
| `~/code/nago-wss` | `feat/nagoya-resolve` | `6e52935` | 0.2.0 | clean |
| `~/code/nago-rustls` | `master` | `91a55f0` | 0.1.1 | clean |

The endpoint-libs row is the code. The commit that adds this file sits
directly on it and changes only the queue, the decision row and the TODO
pointer.

`feat/resolve` is `fix/taskset-push-order` at `a0dd8ad`, plus name resolution
at `e0ff612`, plus the signal waiter at `ee8fca0`. `feat/nagoya-resolve` is
`bench/nagoya-write-path` at `3d069ed` plus the caller commit. Neither new
branch may be merged while nago-wss path-depends on nagoya or nago-rustls.

**crates.io, not the checkout.** `cargo search` on this date: nagoya `0.1.9`,
nago-wss `0.2.0`, nago-rustls `0.1.1`. endpoint-libs depends on
`nagoya = "0.1.9"` from crates.io. The sibling checkout is `0.1.10` and is
the only tree with `resolve`, `connect_any` and `Signal`. Cite those against
`feat/resolve`. Do not depend on the checkout to get them.
`Notify::notify_waiters` is on the published `0.1.9`.

**nago-wss is a path dependency in its own manifest, and this port did not
make it one.** `feat/nagoya-resolve` has `nagoya = { path = "../nagoya" }`
and `nago-rustls = { path = "../nago-rustls" }`. Commit `3d069ed` put them
there, replacing crates.io nagoya `0.1.6` and nago-rustls `0.1.1`, so a
benchmark could see an unreleased nagoya. The message on that commit says
the pins go back to versions once nagoya `0.1.9` is released, and that a
path cannot be published. `6e52935` then calls `nagoya::reactor::resolve`
and `connect_any`, which `0.1.9` does not export, so the path is still
load-bearing for that branch. Leave it. Do not copy it.

**No lock file in endpoint-libs.** `.gitignore` ignores `Cargo.lock`. It is
not in git. nago-wss and nago-rustls each commit a `Cargo.lock`. Do not add
one here, and do not start committing theirs as part of this port.

`parking_lot` in endpoint-libs is the published fork: `package = "parking_lot_lite_hack"`,
`version = "^0.12"`, `default-features = false`. That resolved to `0.12.8`.
The sibling checkout `~/code/parking_lot_lite_hack` is `d70c251` and the same
version. Call sites stay `use parking_lot::...`. It was not removed. dashmap
and async-trait have no fork under `~/code`. They are not tokio. Leave them.

The nagoya dependency enables `reactor` on the dependency line, not on the
`nagoya-transport` feature. `signal` names `dep:nagoya`, so a `signal`-only
build compiles the reactor. `ws-core` requires `signal`.

Release order in `docs/release-order.md` is unchanged. No commit this series
has bumped endpoint-libs off 3.2.0. `./scripts/check-chain.sh` was not run.
`./scripts/check-features.sh` passed with `cargo check --offline --lib` on
the `parking_lot_lite_hack` switch.

## 3. Corrections still true

Re-derive a line from the identifier before editing. Do not copy a number
from 04.

- `tokio::select!` is gone under `src/`. The replacement is
  `futures::future::select`, left-biased, in a block that ends before the
  arms touch `self`.
- The per-connection `WsMessage` queue is `ws::outbound` (`82f64d3`). The
  bound is the number of queued messages. A cloned sender does not reserve
  a slot. `try_send` returns `Full` and `Closed`. `recv` returns `None`
  when every sender is gone. `Outbound::Closed` and `WsStreamState::end`
  stay. `ps-spsc` is not this queue.
- `nagoya::reactor::resolve` and `connect_any` are public on `feat/resolve`
  only. `resolve` returns every address, with the caller's port stamped on.
  An empty `connect_any` fails with `ECONNREFUSED`. nago-wss maps `Nul` to
  the URL error and every other resolve failure to `EAI_NONAME`.
- `nagoya::reactor::Signal` is the unix waiter on `feat/resolve` (`ee8fca0`),
  not in crates.io `0.1.9`. Linux is `signalfd`. The BSDs are a pipe and a
  handler. One waiter per number (`EBUSY`). `pthread_sigmask` is per thread.
- `Addr` is `V4` / `V6` / `Path`. A Unix peer has no arm on
  `PeerIdentity::Network`.
- `nagoya::reactor::TcpListener`, `Reactor::local`, `block_on_with` and
  `TaskSet` are how a server waits on sockets without tokio. `Sharded` is
  several reactors, one thread each. It is not `SO_REUSEPORT`. One listener
  can hand each accepted socket to `handle_for`. Do not invent
  `SO_REUSEPORT`.
- `Signal::recv` completes only while that reactor is polling. Replacing
  `tokio::signal` without driving a nagoya reactor does not deliver the
  signal. The accept loop, the signal waiter and the connection tasks move
  together.
- Server handshake `Request` and client `ClientOptions` are different types.
  nago-wss `build_response` is a fixed 101. `build_rejection` is a fixed
  400, 426 or 431.
- `Connection<S>` is one owned object. Write methods take `&mut self`.
  `Toolbox::send` runs on an arbitrary task. The per-connection queue is
  the route.
- `notify_waiters` wakes the current set and leaves no permit. `Shutdown`
  stores the `AtomicBool` first. `cancelled` calls `Notified::enable` before
  it reads the flag.
- `cargo check --all-targets` unifies the dev-dependency `tokio` with
  `features = ["full"]`. `cargo check --lib` does not. `cargo tree -e normal`
  does not either.

### What still names tokio

These are unfinished. They are not exceptions.

| Site | Names | Replacement |
| --- | --- | --- |
| `libs/signal.rs` | `tokio::signal` | `nagoya::reactor::Signal`, once a published nagoya has it |
| `ws/listener.rs` | `tokio::net::TcpListener`, `tokio::io` | `nagoya::reactor::TcpListener` |
| `ws/server.rs` `listen` | `tokio::net::lookup_host` | `nagoya::reactor::resolve`, same publish gate |
| `ws/server.rs` shard channel | `tokio::sync::mpsc` of accepted sockets | gone when one nagoya reactor accepts and `TaskSet` runs the connections |
| `ws/server.rs` `run_shard` | `tokio::runtime`, `LocalSet` | `Reactor::local` / `block_on_with` / `TaskSet` |
| `ws/traits.rs` `RawStream` | `tokio::io::{AsyncRead, AsyncWrite}` | the stream nago-wss already reads |
| `ws/client.rs` | `tokio::net`, `lookup_host`, `tokio::spawn` | nago-wss `connect_plain` / `connect_secure`; `connect_h2` does not keep tokio either |
| `ws` / `ws-client` | hyper, tokio-tungstenite, tokio-rustls | nago-wss and nago-rustls, from crates.io |
| `framed-transport-tokio` | `tokio::io`, tokio-util | the neutral framed path, which already exists |
| `scheduler`, `log_reader`, `error_aggregation`, `log_throttling`, `otel` | `tokio::spawn`, `time`, `sync` | nagoya's `spawn`, `time` and `sync`, from the published crate |

Under `--features ws-core,framed-transport,nagoya-transport`, `cargo tree
-e normal -i tokio` still prints tokio. Measured on `82f64d3`: features
`net`, `rt`, `sync`, `signal`. The parking-lot commit did not change that.

## 4. The queue

### Done

1. **Delete `message_receiver`.** `813c795`.
2. **Replace `tokio::select!`.** `138cdb1`.
3. **Replace `CancellationToken`.** `5d6ae8e`. The flag is `Shutdown`. Delivery is still `tokio::signal` until item 12, and item 12 waits on a published nagoya.
4. **Teardown and policy-close.** `1a35b49`.
5. **`TOOLBOX` per poll.** `b55e62b`.
6. **Drop `Date`.** `9f43c32`.
10. **Per-connection outbound queue.** `82f64d3`. `ws::outbound`. Not tokio, not `futures` mpsc, not `ps-spsc`.

**`parking_lot` is `parking_lot_lite_hack` from crates.io.** `3531fd6`. Not removed. Not a path. Not a lane number.

**P0.2. Signal delivery in nagoya.** `ee8fca0`. Not in the published crate. See the decision row.

P0.3, P0.4 and P0.5 are rows in `docs/decisions/ws-core-onto-nago-wss.md`. The "keep tokio" sentence in P0.5 is withdrawn by the "End state" row on this roll.

### Still open

- **P0.1. Where resolution lives.** Half done. The functions are on nagoya `e0ff612`, which is not published. nago-wss `6e52935` calls them through its existing path dependency. endpoint-libs still calls `tokio::net::lookup_host`. The endpoint-libs caller lands against a published nagoya. No path dependency. No lock file.

### Lane 3

Do not merge nago-wss to master while it path-depends on nagoya or
nago-rustls. Do not take that path into endpoint-libs to get around it.

7. **`MessageStream` over `nago_wss::Connection<S>`.** crates.io `nago-wss` `0.2.0` exports `Connection` and `connect_plain`. Its nagoya requirement is `0.1.6`. endpoint-libs pins nagoya `0.1.9`. Cargo will try to unify those. `3d069ed` exists because `TcpStream` was not the same type across that gap. Read both published crates before adding the dependency. No path.
8. **`Addr` to `SocketAddr` for `V4` and `V6` only.** `Path` is an error that names the family. `Addr` is in crates.io nagoya `0.1.9`. Read that crate.
9. **Listener and connections on the published reactor.** crates.io nagoya `0.1.9` exports `Reactor`, `TcpListener`, `TaskSet`, `block_on_with`, `Sharded` and `Addr`. It does not export `Signal` or `resolve`. The shard `mpsc`, the per-shard tokio runtime and `LocalSet` go when this lands. Hostname lookup stays `tokio::net::lookup_host` until `resolve` is published. `tokio::signal` stays until `Signal` is published. Those two are not a reason to keep the accept loop on tokio.
11. **H1 upgrade.** `upgrade_stream`'s receiver becomes a plain value for H1. The kept extras (`Server`, `Cache-Control` on errors, CORS, `OPTIONS`, `HEAD`, `Origin`) have to be carried onto nago-wss `build_response` / `build_rejection`, and `Request` has to retain `Origin`. `Date` does not come back. The H2 arm does not keep tokio either. Edit nago-wss only when the change is against a published API; the path branch is not the place this port writes.
12. **`signal` delivery.** Rewrite `libs/signal.rs` against `nagoya::reactor::Signal`. Drop `tokio/signal` from the `signal` feature. The `Shutdown` flag stays. `Signal` is not in crates.io nagoya `0.1.9`. Not started. No path.
13. **`ws-client`.** `connect_plain` and `connect_secure` are on crates.io nago-wss `0.2.0`. Same nagoya `0.1.6` pin as item 7. `connect_h2` does not remain a reason to keep tokio.

### Lane 4

14. **`ws-core` stops naming `dep:tokio`.** Not true on `3531fd6`. The whole crate stops naming it, not only `ws-core`. See the table in section 3.
15. **`./scripts/check-features.sh` and `./scripts/check-chain.sh`.** The `--lib` form passed on the parking-lot switch. `check-chain.sh` has not. This lane must not add another silent feature break.

## 5. Definition of done

- Lane 1, item 6, item 10 and the parking-lot switch are committed, not merged, and not on PR #52. P0.2 is nagoya `ee8fca0` and is not in crates.io `0.1.9`.
- P0.2, P0.3, P0.4 and P0.5 have rows. The end-state row withdraws "keep tokio". P0.1 does not have a row.
- No path dependency on nago-wss, nago-rustls, nagoya or `parking_lot_lite_hack` has been added to endpoint-libs. The nago-wss branch still has the path it had at `6e52935`. endpoint-libs pins crates.io nagoya `0.1.9` and crates.io `parking_lot_lite_hack` `^0.12`.
- endpoint-libs still has no committed `Cargo.lock`.
- Under the feature set karen uses, `cargo tree -e normal -i tokio` still prints tokio. That is unfinished.

## 6. Carrying this file

A turn ends when the lane it took is empty, when a measurement refutes the
item, or when the next item is waiting on a publish. The next item that can
be written on `fix/ws-core-lane1` against crates.io nagoya `0.1.9` is item 9's
accept loop (`Reactor`, `TcpListener`, `TaskSet`). Item 12 and the
endpoint-libs half of P0.1 wait on a published `Signal` and `resolve`.
Item 7 waits on the nagoya `0.1.6` / `0.1.9` unification, not on a path.
Read the published crate before writing the call. Do not point `Cargo.toml`
at a sibling to make the call compile.

At the start: read the highest-numbered file in this series,
`endpoint-libs/AGENTS.md`, and `git status --short` plus HEAD for the four
repos in section 2. Resume the first unfinished item whose API is in a
published crate. Do not redo a done item unless the tree contradicts its
proof commit.

At the end: bump the revision, prune finished items to one line, and record
what was not run.

**Roll this file** when the listen path no longer names tokio, or when
`signal` no longer names tokio. The successor is the next number and is a
complete document. Banner this file. Leave the old text. Do not rename P0.1
through P0.5 or the lane numbers.

Line numbers in 04 were read at older HEADs. Re-derive a line from the
identifier before editing it.

### Not run, this roll

- `./scripts/check-chain.sh`.
- The Linux `signalfd` path. It typechecked on `ee8fca0`. It did not run.
- A version bump, a publish, and any push.
- Any edit under `~/code/nago-wss`, `~/code/nagoya`, or `~/code/nago-rustls`.
