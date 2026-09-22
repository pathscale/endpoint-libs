# `ws-core`, `ws` and `ws-client` onto `nago-wss`: 06

Date: 2026-09-22
Revision: 1. Successor of `ws-core-onto-nago-wss-05.md`. Completed work is
one line plus the proof commit. Do not copy line numbers out of 05.

This roll exists because 05 was audited against the trees and against the
published crates, and it was wrong in four ways that would have cost the next
turn real time. Each correction is marked **WAS WRONG IN 05** below. One of
them was a live defect in this repository, one was a live defect in nagoya,
and two were claims that would have blocked work that is not actually
blocked.

## How this series works

This file is the queue. Work the highest number. When section 6 says to roll,
write `ws-core-onto-nago-wss-07.md` as a complete document, banner this file
`SUPERSEDED by <successor>`, and leave the old text alone. Do not open a
second live file at the same number.

**Update this file in place.** There is no locking. Stage it by name.

The end state is that endpoint-libs does not depend on tokio, on any feature.
There is no accepted leftover. The replacements are published crates: nagoya,
nago-wss, nago-rustls, and the other Pathscale forks. Versions are written in
`Cargo.toml`. This repository does not have a `Cargo.lock`. A sibling path
(`../nagoya`, `../nago-wss`, `../nago-rustls`, `../parking_lot_lite_hack`) is
not a way to get an API that crates.io does not have yet.

A **DECISION** item may not be started as code. P0.3, P0.4 and P0.5 have
rows. The "keep tokio" half of P0.5 is withdrawn; see the decision file.
P0.1 is half done and P0.2 is **not** done; see the queue.

**Re-derive before you trust.** Every claim in this file was checked against
the tree or the published `.crate` on 2026-09-22. Where a claim could not be
checked by reading, it says so. A claim you cannot re-derive is a claim to
re-check, not one to build on.

## 1. Where this sits

| Bucket | Location | Rule |
| --- | --- | --- |
| 1 | this file | Blocking for the port. Rolls to `ws-core-onto-nago-wss-07.md`. |
| 2 | `endpoint-libs/TODO.md` | Decided work that waits on this file. One pointer line back here is enough. |
| record | `endpoint-libs/docs/decisions/ws-core-onto-nago-wss.md` | A settled P0, including "decided not to". Append only. |

**Do not fold this port into endpoint-libs PR #52.** That PR is
`fix/restore-release-trigger`, 3.2.0 and tokio-optional, and merging it
publishes. endpoint-libs work stays on `fix/ws-core-lane1`, branched from
that branch at `8ccb574`. Nothing here waits on PR #52, and nothing here may
be smuggled into it.

## 2. State

Read on 2026-09-22 with `git rev-parse` and `git status --short`. Re-read
before trusting the table. Nothing here is pushed. No repository in this
table was given a path dependency by this port.

| Repo | Branch | HEAD | Version | Tree |
| --- | --- | --- | --- | --- |
| `~/code/endpoint-libs` | `fix/ws-core-lane1` | `1393176` | 3.2.0 | clean, apart from this file when it is the edit in progress |
| `~/code/nagoya` | `feat/resolve` | `ee8fca0` | 0.1.10 | see the P0.2 row: the signal waiter is being corrected |
| `~/code/nago-wss` | `feat/nagoya-resolve` | `6e52935` | 0.2.0 | clean |
| `~/code/nago-rustls` | `master` | `91a55f0` | 0.1.1 | clean |

**WAS WRONG IN 05.** 05's table gave the endpoint-libs HEAD as `3531fd6`,
which was the commit before the one that added 05. Read `git rev-parse`
rather than copying this cell: the commit that adds a roll sits on top of the
row it describes, so this cell is one commit behind as soon as it is written.

`feat/resolve` is `fix/taskset-push-order` at `a0dd8ad`, plus name resolution
at `e0ff612`, plus the signal waiter at `ee8fca0`. `feat/nagoya-resolve` is
`bench/nagoya-write-path` at `3d069ed` plus the caller commit. Neither new
branch may be merged while nago-wss path-depends on nagoya or nago-rustls.

**crates.io, not the checkout.** Verified against the published `.crate`
tarballs, not against docs.rs prose. Latest on this date: nagoya `0.1.9`,
nago-wss `0.2.0`, nago-rustls `0.1.1`, `parking_lot_lite_hack` `0.12.8`.
endpoint-libs depends on `nagoya = "0.1.9"` from crates.io. The sibling
checkout is `0.1.10` and is the only tree with `resolve`, `connect_any` and
`Signal`. Cite those against `feat/resolve`. Do not depend on the checkout to
get them.

What published nagoya `0.1.9` does export, confirmed by reading the crate:
`Reactor`, `Reactor::local`, `reactor::TcpListener`, `TaskSet`,
`block_on_with`, `Sharded`, `Addr` with exactly `V4`/`V6`/`Path`, and
`Notify::notify_waiters`. It does not export `reactor::Signal`,
`reactor::resolve` or `reactor::connect_any`. The only `Signal` in `0.1.9`
is a private struct in the private `block_on` module and is the parker's
waker, not a unix signal.

**WAS WRONG IN 05.** Two API paths in 05's replacement column do not exist.
`nagoya::time::sleep` is not a path: `time` is a private module and `sleep`
is re-exported at the crate root, so it is `nagoya::sleep`. There is no free
`nagoya::spawn`; there is `Executor::spawn` and `Runtime::spawn`, reached as
`nagoya::runtime::background().spawn(..)`. Anything that writes those calls
from 05's table will not compile.

`nagoya`'s `reactor` feature is **not** a default. The dependency line here
already names `features = ["reactor"]`, which is what makes
`nagoya::reactor` exist at all. Keep it there.

**nago-wss is a path dependency in its own manifest, and this port did not
make it one.** `feat/nagoya-resolve` has `nagoya = { path = "../nagoya" }`
and `nago-rustls = { path = "../nago-rustls" }`, in both `[dependencies]` and
`[dev-dependencies]`. Commit `3d069ed` put them there so a benchmark could
see an unreleased nagoya. The manifest carries its own warning above the
`nago-rustls` line; the `nagoya` line has no equivalent warning and is the
same hazard. The published `nago-wss 0.2.0` has no path dependency in it:
every dependency in the published manifest carries a version requirement.
The path is a property of that unpublished branch only. Leave it. Do not copy
it.

**No lock file in endpoint-libs.** `.gitignore` ignores `Cargo.lock`. It is
not in git. nago-wss and nago-rustls each commit one. Do not add one here.

`parking_lot` in endpoint-libs is the published fork: `package = "parking_lot_lite_hack"`,
`version = "^0.12"`, `default-features = false`. Call sites stay
`use parking_lot::...`. dashmap and async-trait have no fork under `~/code`.
They are not tokio. Leave them.

The nagoya dependency enables `reactor` on the dependency line, not on the
`nagoya-transport` feature. `signal` names `dep:nagoya`, so a `signal`-only
build compiles the reactor. `ws-core` requires `signal`.

Release order in `docs/release-order.md` is unchanged. No commit in this
series has bumped endpoint-libs off 3.2.0.

## 3. Corrections still true

Re-derive a line from the identifier before editing. Do not copy a number
from 05.

- `tokio::select!` is gone under `src/`. The replacement is
  `futures::future::select`.
- **`futures::future::select` is not fairness preserving, and the session
  loop's arm order is load bearing.** It takes its left arm the moment that
  arm is ready and never polls the right one. `tokio::select!` chose at
  random. The order is `session::priority4`: a finished handler, then
  outbound, then inbound, then the policy flag. Handlers are first because
  they are the only arm a peer cannot keep ready; anywhere below inbound they
  starve. Three tests pin this. Do not renest it casually, and do not assume
  a `select` chain inherits `select!`'s fairness anywhere else in the tree.
- The per-connection `WsMessage` queue is `ws::outbound` (`82f64d3`). The
  bound is the number of queued messages. A cloned sender does not reserve
  a slot. `try_send` returns `Full` and `Closed`, and `Closed` wins when both
  are true. `recv` returns `None` when every sender is gone and the queue is
  drained. `Outbound::Closed` and `WsStreamState::end` stay. `ps-spsc` is not
  this queue.
- `nagoya::reactor::resolve` and `connect_any` are public on `feat/resolve`
  only. `resolve` returns every address, with the caller's port stamped on,
  and the byte order is correct on both families. An empty `connect_any`
  fails with `ECONNREFUSED`. nago-wss maps `Nul` to the URL error and every
  other resolve failure to `EAI_NONAME`. Two known defects, neither fixed:
  `resolve` calls `getaddrinfo`, which blocks, and nagoya has no
  `spawn_blocking`, so calling it from a task stalls every other socket on
  that reactor for the whole DNS timeout, and nothing in the docs says so;
  and `connect_any` returns the last error rather than the most informative
  one, so a dual-stack host with no v6 route reports `ENETUNREACH` instead of
  the real `ECONNREFUSED`.
- `Addr` is `V4` / `V6` / `Path`. A Unix peer has no arm on
  `PeerIdentity::Network`.
- `nagoya::reactor::TcpListener`, `Reactor::local`, `block_on_with` and
  `TaskSet` are how a server waits on sockets without tokio. `Sharded` is
  several reactors, one thread each. It is not `SO_REUSEPORT`. One listener
  can hand each accepted socket to `handle_for`. Do not invent
  `SO_REUSEPORT`.
- `Signal::recv` completes only while that reactor is polling, with one
  exception: a signal raised before the first poll is read straight out of
  the descriptor. Replacing `tokio::signal` without driving a nagoya reactor
  does not deliver the signal. The accept loop, the signal waiter and the
  connection tasks move together.
- Server handshake `Request` and client `ClientOptions` are different types.
  nago-wss `build_response` is a fixed 101. `build_rejection` is a fixed
  400, 426 or 431.
- `Connection<S>` is one owned object. Write methods take `&mut self`.
  `Toolbox::send` runs on an arbitrary task. The per-connection queue is
  the route.
- `notify_waiters` wakes the current set and leaves no permit. `Shutdown`
  stores the `AtomicBool` first. `cancelled` calls `Notified::enable` before
  it reads the flag. All four interleavings were checked against nagoya
  `0.1.9`'s `sync.rs` and close.
- `TOOLBOX` is a `thread_local` installed on poll entry and restored on
  return by a drop guard, so it survives `Ready`, `Pending`, unwind and
  scope drop. `scoped-tls` is not a dependency.
- `cargo check --all-targets` unifies the dev-dependency `tokio` with
  `features = ["full"]`. `cargo check --lib` does not. `cargo tree -e normal`
  does not either.

### What still names tokio

These are unfinished. They are not exceptions. **WAS WRONG IN 05:** 05's
version of this table named a file that does not exist, named APIs that the
code does not call, and left out two real sites. It was also contradicted by
this repository's own `Cargo.toml` comments and `README.md`, both of which
were right. This table is rebuilt from the tree. Prefer the README's feature
table over any prose if the two ever disagree again.

| Site | Names | Replacement |
| --- | --- | --- |
| `libs/signal.rs` | `tokio::signal::unix` | `nagoya::reactor::Signal`, once a published nagoya has it |
| `ws/listener.rs` | `tokio::net::{TcpListener, TcpStream}`, `tokio::io` | `nagoya::reactor::TcpListener` |
| `ws/server.rs` `listen` | `tokio::net::lookup_host` | `nagoya::reactor::resolve`, same publish gate |
| `ws/server.rs` `listen_impl` | `tokio::sync::mpsc` of `(T::Channel1, SocketAddr)`, depth 256 | gone when one nagoya reactor accepts and `TaskSet` runs the connections. Note the element is the listener's channel type, not a socket |
| `ws/server.rs` `run_shard` | `tokio::runtime::Builder::new_current_thread`, `LocalSet` | `Reactor::local` / `block_on_with` / `TaskSet` |
| `ws/traits.rs` `RawStream` | `tokio::io::{AsyncRead, AsyncWrite}` | the stream nago-wss already reads |
| `ws/client.rs` | `tokio::net::TcpStream`, `lookup_host`, `tokio::spawn`, tokio-tungstenite, tokio-rustls | nago-wss `connect_plain` / `connect_secure`; `connect_h2` does not keep tokio either |
| `ws/tungstenite/upgrader.rs` | `tokio::task::spawn_local`, tokio-tungstenite | item 11. This is the only `spawn_local` left and it is why `LocalSet` survives |
| `ws/tls.rs` | `tokio_rustls::{TlsAcceptor, server::TlsStream}` | nago-rustls |
| `ws/tungstenite/message.rs` | `tokio_tungstenite::tungstenite::Message` and frame types | `nago_wss::proto::message::Message` |
| `transport/framed.rs` (`framed-transport-tokio`) | `tokio::io`, `tokio_util::codec` | the neutral framed path, which already exists |
| `scheduler.rs` | `tokio::spawn`, `tokio::task::spawn`, `tokio::time::sleep`, tokio-cron-scheduler | `nagoya::sleep` and `nagoya::runtime::background().spawn` |
| `log.rs` (`log_throttling`) | `tokio::spawn`, `tokio::time::sleep` | same. There is no `log_throttling.rs` |
| `log_reader.rs` | `tokio::task::spawn_blocking` only | nagoya has no `spawn_blocking`. This one needs an answer before it can move |
| `log/error_aggregation.rs` | `tokio::sync::RwLock`, `tokio::sync::mpsc` unbounded, `tokio::spawn`, `tokio::task::JoinHandle` | nagoya `sync`, plus a queue. `tokio::time` here is test-only |
| `otel` | **no source names tokio.** It enters through `opentelemetry_sdk`'s `rt-tokio` batch exporter | an exporter that does not pull a runtime, or the feature stays off |

Under `--features ws-core,framed-transport,nagoya-transport`, `cargo tree
-e normal -i tokio` still prints tokio, with features `net`, `rt`, `sync`
and `signal`. Measured on `82f64d3`; no commit since has changed it.

`log_reader`'s `spawn_blocking` is called out because it is the one site with
no nagoya counterpart at all. It is not hard, but it is not a swap either,
and 05 did not distinguish it from the sites that are.

## 4. The queue

### Done

1. **Delete `message_receiver`.** `813c795`.
2. **Replace `tokio::select!`.** `138cdb1`, corrected by `1393176`.
3. **Replace `CancellationToken`.** `5d6ae8e`. The flag is `Shutdown`. Delivery is still `tokio::signal` until item 12.
4. **Teardown and policy-close.** `1a35b49`. Four edges, four tests.
5. **`TOOLBOX` per poll.** `b55e62b`.
6. **Drop `Date`.** `9f43c32`.
10. **Per-connection outbound queue.** `82f64d3`. `ws::outbound`.

**`parking_lot` is `parking_lot_lite_hack` from crates.io.** `3531fd6`.

**Session loop arm priority.** `1393176`. See the correction in section 3.
This was a liveness regression introduced by item 2 and shipped in 05 as
done. A saturated inbound socket starved the handler set, so queued requests
never completed and never answered. `session::priority4` is the order now,
and `a_ready_handler_beats_a_ready_inbound` fails on the previous nesting.

P0.3, P0.4 and P0.5 are rows in `docs/decisions/ws-core-onto-nago-wss.md`.
The "keep tokio" sentence in P0.5 is withdrawn by the "End state" row.

### Still open

- **P0.1. Where resolution lives.** Half done. The functions are on nagoya
  `e0ff612`, which is not published. nago-wss `6e52935` calls them through
  its existing path dependency. endpoint-libs still calls
  `tokio::net::lookup_host`. The endpoint-libs caller lands against a
  published nagoya. The two defects in section 3 should be fixed before that
  publish, not after.

- **P0.2. Signal delivery in nagoya. NOT DONE. WAS WRONG IN 05.** 05 listed
  this as done on `ee8fca0` and the decision file records it as settled. The
  four behavioural claims hold as written, and the kqueue path really did run
  7 tests on macOS, but the safety argument behind them does not survive a
  multithreaded process, and a review found four defects:
  1. **Blocker, BSD/macOS.** `arm` blocks the signal with `pthread_sigmask`,
     which is per thread, and only then installs the handler. In the window
     between the two -- a `pipe()`, four `fcntl()`s and a mutex -- every
     other already-running thread still has the signal unblocked and still
     has `SIG_DFL`, so a `kill(pid, SIGTERM)` in that window terminates the
     process. That is the exact outcome the commit message says the blocking
     prevents. The mask is not what makes this safe; the installed handler
     is.
  2. **Linux.** `arm`'s fast path returns the cached fd without calling
     `block_one`, so a second waiter created on a different thread never
     blocks the signal there. signalfd only delivers while the signal stays
     blocked, so that thread keeps `SIG_DFL` and a process-directed signal
     kills the process while a live waiter sits on the descriptor.
  3. **Race.** `Signal` declares `claim` before `registration`, and fields
     drop in declaration order, so the one-waiter slot is released before the
     fd is removed from the poller. A re-register that wins that gap has its
     registration torn down by the outgoing waiter: on kqueue it parks
     forever, on epoll it gets a spurious `EEXIST`. The comment claiming the
     claim is released last is false.
  4. **Async-signal-safety.** The handler calls `write` without saving and
     restoring `errno`, and `write` will set it once the pipe fills. The
     module header claims this requirement is met. It is not.

  A fifth, minor: the previous signal mask is never preserved, so creating
  or dropping a `Signal` unblocks a signal the program had deliberately
  blocked on that thread.

  These are being fixed on `feat/resolve`. **Until that is committed and the
  macOS tests rerun, treat P0.2 as open and do not cite `ee8fca0` as its
  settlement.** The decision file's P0.2 row needs a correction appended; do
  not edit the existing row, the file is append only.

### Lane 3

Do not merge nago-wss to master while it path-depends on nagoya or
nago-rustls. Do not take that path into endpoint-libs to get around it.

7. **`MessageStream` over `nago_wss::Connection<S>`.** **WAS WRONG IN 05.**
   05 said Cargo "will try to unify" nago-wss's nagoya requirement with this
   crate's pin and that `3d069ed` exists because `TcpStream` was not the same
   type across that gap, and told the next turn to read both crates before
   adding the dependency. The first half is a non-problem and the second half
   misreads `3d069ed`.

   Published nago-wss `0.2.0` requires `nagoya = "0.1.6"`. For a `0.1.z`
   requirement, `^0.1.6` means `>=0.1.6, <0.2.0`, so this crate's `0.1.9`
   satisfies it and both land in one compatibility range. The resolver picks
   a single nagoya, `0.1.9`, for the whole graph. nago-rustls `0.1.1` asks
   `^0.1.3` and joins the same range. One nagoya means one
   `nagoya::reactor::TcpStream`, so there is no type-identity hazard on the
   registry route. The method signatures nago-wss uses are unchanged from
   `0.1.6` to `0.1.9`.

   `3d069ed`'s nineteen errors were real, but they were between a **path**
   nagoya and a **registry** nagoya. A path source and a registry source
   never unify, whatever the versions say. That is a property of the
   unpublished branch, not of the versions, and it does not apply to the
   registry route this port is required to take.

   So this item is not blocked on a version question. Add
   `nago-wss = "0.2.0"` and write the newtype. What it is still blocked on is
   `nagoya`'s `reactor` feature, which is off by default: nago-wss asks for
   it, and features are additive, so a graph containing nago-wss turns it on
   for everyone. This crate's dependency line already names it. Keep it.
8. **`Addr` to `SocketAddr` for `V4` and `V6` only.** `Path` is an error that
   names the family. `Addr` is in crates.io nagoya `0.1.9`.
9. **Listener and connections on the published reactor.** crates.io nagoya
   `0.1.9` exports `Reactor`, `TcpListener`, `TaskSet`, `block_on_with`,
   `Sharded` and `Addr`. It does not export `Signal` or `resolve`. The shard
   `mpsc`, the per-shard tokio runtime and `LocalSet` go when this lands,
   except that `LocalSet` also survives for the hyper upgrader's
   `spawn_local` until item 11. Hostname lookup stays
   `tokio::net::lookup_host` until `resolve` is published. `tokio::signal`
   stays until `Signal` is published. Those two are not a reason to keep the
   accept loop on tokio.
11. **H1 upgrade.** `upgrade_stream`'s receiver becomes a plain value for H1.
    The kept extras (`Server`, `Cache-Control` on errors, CORS, `OPTIONS`,
    `HEAD`, `Origin`) have to be carried onto nago-wss `build_response` /
    `build_rejection`, and `Request` has to retain `Origin`. `Date` does not
    come back. The H2 arm does not keep tokio either. This is also what
    removes the last `spawn_local`. Edit nago-wss only when the change is
    against a published API; the path branch is not where this port writes.
12. **`signal` delivery.** Rewrite `libs/signal.rs` against
    `nagoya::reactor::Signal`. Drop `tokio/signal` from the `signal` feature.
    The `Shutdown` flag stays. Blocked on P0.2 being genuinely finished and
    then published, not merely on `ee8fca0` existing.
13. **`ws-client`.** `connect_plain` and `connect_secure` are on crates.io
    nago-wss `0.2.0`. Same unification answer as item 7: it is not a blocker.
    `connect_h2` does not remain a reason to keep tokio.

### Lane 4

14. **`ws-core` stops naming `dep:tokio`.** Not true on `1393176`. The whole
    crate stops naming it, not only `ws-core`. See the table in section 3.
    `log_reader`'s `spawn_blocking` and `otel`'s exporter are the two that
    need an answer rather than a swap.
15. **`./scripts/check-features.sh` and `./scripts/check-chain.sh`.** The
    feature script passes; see below. `check-chain.sh` has not been run in
    this series at all. This lane must not add another silent feature break.

## 5. Definition of done

- Lane 1, item 6, item 10, the parking-lot switch and the arm-priority fix
  are committed, not merged, and not on PR #52.
- P0.3, P0.4 and P0.5 have rows. The end-state row withdraws "keep tokio".
  P0.1 and P0.2 do not have settled rows: P0.2's existing row needs a
  correction appended.
- No path dependency on nago-wss, nago-rustls, nagoya or
  `parking_lot_lite_hack` has been added to endpoint-libs. endpoint-libs pins
  crates.io nagoya `0.1.9` and crates.io `parking_lot_lite_hack` `^0.12`.
- endpoint-libs still has no committed `Cargo.lock`.
- Under the feature set karen uses, `cargo tree -e normal -i tokio` still
  prints tokio. That is unfinished.

## 6. Carrying this file

A turn ends when the lane it took is empty, when a measurement refutes the
item, or when the next item is waiting on a publish.

The next items that can be written on `fix/ws-core-lane1` against crates.io
nagoya `0.1.9` are item 9's accept loop (`Reactor`, `TcpListener`, `TaskSet`)
and item 8's `Addr` conversion. Items 7 and 13 are **not** blocked on the
version question 05 raised, only on someone writing them. Item 12 and the
endpoint-libs half of P0.1 wait on a published `Signal` and `resolve`, and
`Signal` additionally waits on P0.2's defects being fixed.

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

Line numbers in 05 were read at older HEADs. Re-derive a line from the
identifier before editing it.

### Run, this roll

- `cargo test --offline --lib --features ws-core`: 110 passed, 0 failed.
- `./scripts/check-features.sh`: every one of the 18 features compiles
  standalone.
- `cargo clippy --offline --lib --features ws-core`: one warning, a missing
  `Default` for `Shutdown`, pre-existing from `5d6ae8e`.
- The arm-priority regression test was confirmed to fail on the previous
  nesting before the fix was kept.

### Not run, this roll

- `./scripts/check-chain.sh`. It has not been run anywhere in this series.
- The Linux `signalfd` path. It typechecked on `ee8fca0`. It has never been
  executed, and the P0.2 defects above include a Linux-only one.
- A version bump, a publish, and any push.
- Any edit under `~/code/nago-wss` or `~/code/nago-rustls`.
