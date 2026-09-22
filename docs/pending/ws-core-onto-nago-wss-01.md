# `ws-core`, `ws` and `ws-client` onto `nago-wss`: 01

Date: 2026-09-22
Revision: 2. Bump this line on every edit, and say which revision a claim was
checked against. Revision 2 detached this file from the cross-repo queue: it no
longer takes its identity, its item name or its bucket rules from another
document, and it became the first file of a numbered series.

## How this series works

This file is the queue for the port, and it is self-contained. Everything
needed to work it is in it: the state table, the corrections, the decisions and
their criteria, the lanes, and the instructions for carrying it.

**The series is `ws-core-onto-nago-wss-NN.md` in this directory.** This is 01.
When section 6 says to roll, write `ws-core-onto-nago-wss-02.md` as a complete
document, put a `SUPERSEDED by <successor>` banner at the top of this one, and
leave the old text alone from then on. Work the highest number. Do not open a
second live file at the same number, and do not mine a superseded revision for
line numbers: they drift, and in the predecessor series they drifted in some
files by 78 lines.

**Update this file in place.** There is no locking, so two agents in one turn
will collide. Stage it by name.

The decision to move is taken. What is open is the cost: which parts are a
primitive swap, which are a redesign, and which are an owner's call. A
**DECISION** item below may not be started as code. Everything else in lane 1
may.

Every claim was checked against the trees on 2026-09-22. Where the scoping
draft disagreed with the tree, the tree won and the correction is marked
**WAS WRONG**.

## 1. Where this sits

| Bucket | Location | Rule |
| --- | --- | --- |
| 1 | this file | Blocking for the port. Decided and executable, or a **DECISION** with a criterion. Rolls to `ws-core-onto-nago-wss-02.md`. |
| 2 | `endpoint-libs/TODO.md` | Decided work that waits on this file. Keep the port out of it; one pointer line back here is enough. |
| record | `endpoint-libs/docs/decisions/` | A settled P0, including "decided not to". Append only. |

A cross-repo queue may record that this port exists and name this series. It
does not work it and it does not hold its items. Nothing in this file is copied
back out: one pointer in each direction, and this file is the one that moves.

**Do not fold this port into endpoint-libs PR #52.** That PR is 3.2.0 and
tokio-optional, and merging it publishes irreversibly. Whether taking `otel`
out of `default` is a 3.2.0 or a 4.0.0 is the owner's call and is a different
question from this one. Nothing in this file waits on it, and nothing in this
file may be smuggled into it.

## 2. State

Read on 2026-09-22 with `git rev-parse` and `git status --short`. Re-read
before trusting the table. Nothing here is pushed for this work.

| Repo | Branch | HEAD | Version | Tree |
| --- | --- | --- | --- | --- |
| `~/code/endpoint-libs` | `fix/restore-release-trigger` | `27e75e7` | 3.2.0 | dirty only by `docs/pending/` (this file) |
| `~/code/nagoya` | `fix/taskset-push-order` | `a0dd8ad` | 0.1.10 | clean |
| `~/code/nago-wss` | `bench/nagoya-write-path` | `3d069ed` | 0.2.0 | clean |
| `~/code/nago-rustls` | `master` | `91a55f0` | 0.1.1 | clean |

**The pins do not match the checkouts.** endpoint-libs depends on
`nagoya = "0.1.9"` (`Cargo.toml:266`). The sibling checkout is 0.1.10. Cite
0.1.10 only for the sibling tree. When a change compiles against the manifest
pin, re-read that crate, not the sibling.

**nago-wss cannot be released from this checkout.** `nago-wss/Cargo.toml`
takes `nago-rustls` and `nagoya` by path and says those lines must not reach
master. `nago-rustls` likewise path-depends on nagoya. A published nago-wss,
with version pins restored, is the gate on merging any endpoint-libs commit
that names it. Writing against the path checkouts on a branch is fine.

**Who enables `ws` today.** All six backends, plus the two tools that sit on
the same traits. This is a fleet migration. Versions are the ones in each
`Cargo.toml` on 2026-09-22:

| Consumer | endpoint-libs | Features that matter |
| --- | --- | --- |
| `api.support.cafe` | `^2.0` | `ws`, `ws-http1` |
| `web3.trading-backend` | `2.0` | `ws` |
| `nofilter.io-backend` | `2.0` | `ws`, `ws-http1` |
| `pays.online-backend` | `2.0` | `ws` |
| `auth.honey.id-backend` | `^2` | `ws`, `ws-http1`, `ws-client` |
| `api.honey.id-backend` | `^2` | `ws`, `ws-http1` |
| `honey_id-types` | `^2` | `ws`, `ws-client` |
| `endpointgen` | `2.1` | the crate, no `ws` feature of its own |
| `EndpointValidator/endpoint-validator` | `2.1` | the crate |
| `karen` | `=3.1.1` | `ws-core`, `framed-transport`, `nagoya-transport` |

`ws-core` requires `wire-core`, `types` and `signal` (`Cargo.toml:68-72`).
`signal` is `tokio::signal::unix` plus a `CancellationToken`. Until signal
delivery exists outside tokio, `ws-core` cannot leave tokio, because it
requires `signal`.

Release order in `docs/release-order.md` is unchanged: endpoint-libs, then
`honey_id-types` and `endpoint-gen`, then the six backends bump both together.
`./scripts/check-features.sh` and `./scripts/check-chain.sh` are the gates.
Lane 1 items 1 to 3 do not bump a version. Anything that names nago-wss does,
and that bump waits on the publish gate above.

## 3. Corrections

Checked at revision 1, unchanged at revision 2.

- **`tokio::select!` is not a blocker.** `futures::future::select` plus
  `Either` names no runtime. This repo already uses it at
  `src/libs/ws/server.rs:191-197`. The remaining `select!` sites are
  `session.rs:377`, `server.rs:515`, `signal.rs:17` and `signal.rs:27`.
- **`lookup_host` is already written, and it is private.**
  `nago-wss/src/client.rs:174` is `fn resolve`. It returns every address.
  `connect_any` at `:233` tries them in turn. `connect_plain` is `:253`,
  `connect_secure` is `:281`. One `pub`, or a move into nagoya. The crate
  root says the move is the right one (`nago-wss/src/lib.rs:31-37`): nagoya
  has no resolver yet.
- **WAS WRONG: `nagoya/src/block_on.rs:57` is not the signal.** That line is
  `fn waker_of`. `struct Signal` is at `:16`, and it is the parker's waker
  for `block_on`. There is no unix signal anywhere in nagoya. `EVFILT_SIGNAL`,
  `signalfd` and `SO_REUSEPORT` are absent from `nagoya/src`.
- **WAS WRONG on the version in the old note.** `transport.rs:61` and
  `README.md:465` say nagoya 0.1.9 has no signal module. The sibling tree is
  0.1.10 and still has none. Both sentences are stale. The absence is real.
- **WAS WRONG: the accept sketch was a future's output, written as a
  function signature.** `TcpListener::bind` is synchronous,
  `nagoya/src/reactor/net.rs:503`, and takes `(Addr, &Handle)`. `accept` at
  `:547` returns `Accept`, whose `Output` is `Result<(TcpStream, Addr)>`
  (`:559`). A `Handle` has to already exist.
- **`Addr` is not a `SocketAddr` with a missing impl.** It is
  `nagoya/src/reactor/socket.rs:290`, variants `V4`, `V6` and `Path`, with
  no `FromStr` and no `Display`. `PeerIdentity::Network` is
  `SocketAddr` (`src/libs/peer.rs:21`). V4 and V6 convert. A Unix peer has
  no arm on that enum. Do not write a conversion that drops `Path` on the
  floor.
- **The server `Request` keeps three fields.** `path`, `key`, `protocols`
  (`nago-wss/src/proto/handshake.rs:437-444`). `build_response` at `:544`
  emits a fixed 101. The **client** `ClientOptions` already carries extra
  request headers (`nago-wss/src/client.rs:132`). Those are different types.
  Do not "fix" the client while changing the server.
- **`nagoya::time::sleep` is `pub fn sleep(...) -> Sleep`** at
  `nagoya/src/time.rs:304`. It is a future constructor, not an `async fn`.
- **`WsUpgrader::upgrade_stream` returns at `traits.rs:106`.** The
  `Receiver` import is `:9`. The scoping draft said `:104`, which is the
  `cached_date` parameter.
- **Teardown is the `else` at `session.rs:386-389`**, inside the `select!`
  at `:377`. `rx.recv() -> None` means every sender dropped.
- **`message_receiver` is dead, and three texts cite it as load-bearing.**
  Field at `server.rs:43`, set to `None` at `:67`, read nowhere. Cited at
  `Cargo.toml:82`, `transport.rs:74` and `README.md:469`.
- **`spawn_local` in `nagoya/src/lib.rs:39` is a count of tokio calls in a
  trading backend, not an API.** `Executor::spawn` (`lib.rs:269`) and
  `Runtime::spawn` (`runtime.rs:106`) both require `Send`. There is no
  `spawn_local`. A `thread_local!` set on poll entry and restored on return
  is enough for `TOOLBOX`, because the session is polled in place.
- **Dropping `crossfire` was right. Putting `futures::channel::mpsc` in its
  place was not.** The public signature is
  `WsUpgrader::upgrade_stream -> Result<Receiver<UpgradeEvent>>`. The house
  rule: an edge is a `Notify` or an atomic, and only a payload keeps a
  queue. That commit put a channel back into a public trait.

### The five old blockers

| # | Claim in `transport.rs` | Verdict |
| --- | --- | --- |
| 1 | mpsc in public fields | Absent on purpose. `nagoya/src/sync.rs:16-18`: `RwLock`, `Semaphore`, `Notify`, owned guards. No mpsc in nagoya or nago-wss. `Connection<S>` (`nago-wss/src/conn.rs:152`) is owned by one task. `read` `:232`, `write` `:305`, `write_all` `:344`, `pong` `:427`, `close` `:436`, all `&mut self`. |
| 2 | `tokio::select!` | Not a blocker. See corrections. |
| 3 | `tokio::task_local!` for `TOOLBOX` (`toolbox.rs:424`) | Absent. The hand-written guard future is the replacement. `scoped-tls` does not survive the `.await` at `session.rs:244` and `:342`. |
| 4 | unix signals plus `CancellationToken` (`signal.rs`) | Split. The token is an edge: `Notify::notify_waiters` (`sync.rs:309`) plus an `AtomicBool`. Delivery is absent. It belongs in nagoya's reactor. |
| 5 | `tokio::net::lookup_host` (`server.rs:443`, `client.rs:508`) | Written, private, better on dual-stack. See corrections. |

### The twelve channel sites

A pure edge moves to a `Notify` or an atomic. A payload keeps a queue. None
of these twelve is a bare `()`.

| Site | Carries | Class |
| --- | --- | --- |
| `server.rs:298` `mpsc::channel(config.message_buffer_size)` | `WsMessage` | Payload. Per-connection outbound queue. `drop_conn_on_buffer_full` is a policy on its depth. |
| `conn.rs:30` `WebsocketStates::insert` | `WsMessage` | Payload. Public signature. |
| `conn.rs:45` `WsStreamState.message_queue` | `WsMessage` | Payload. Public field. Sender half, stored per connection in a `DashMap`. |
| `toolbox.rs:212` `send_ws_msg` | `WsMessage` | Payload. Public. |
| `toolbox.rs:229` `send_serialized_ws_msg` | `String` to `WsMessage` | Payload. Public. `try_send` and `TrySendError::{Full, Closed}` at `:237` and `:246`. |
| `session.rs:38` `WsClientSession.rx` | `WsMessage` | Payload. Receiver half. |
| `session.rs:46` `WsClientSession::new` | `WsMessage` | Payload. Public constructor. |
| `server.rs:348` `handle_session_connection` | `WsMessage` | Payload. Public method. |
| `server.rs:490` and `:541` shard channel, depth 256 | accepted socket | Payload. One accept loop fan-out. Deleting it means one listener per shard. `SO_REUSEPORT` is not in nagoya, so that delete is new reactor or socket work, not a flag that already exists. |
| `traits.rs:106` `upgrade_stream` | `UpgradeEvent` | H1 yields one event and closes. That wants a return value. H2 CONNECT wants a stream. Public trait. |
| `tungstenite/upgrader.rs:106` `channel::<UpgradeEvent>(2)` | `UpgradeEvent` | Producer for the row above. Depth 2. |
| `server.rs:43` `message_receiver` | `ConnectionId` | Dead. Delete. Do not port. |

Two edges ride on the payload queue and are easy to lose:

1. **Teardown.** `session.rs:386-389`: `recv() -> None` shuts the session
   down. A replacement queue reproduces closure-as-edge, or carries a
   separate `Notify`.
2. **Close-on-policy.** `toolbox.rs:243` and `:254` push `Message::Close(None)`
   for `drop_conn_on_buffer_full` and for `header_only`. It is ordered behind
   whatever is already queued. For the buffer-full case, that is the state
   the policy exists to escape.

`Toolbox::send` (`toolbox.rs:164-184`, installed by `set_ws_states`) reaches
any connection from any task by id. `Connection<S>` has no split and no
clonable writer. The per-connection queue is the only route from a handler
to a socket. Deleting it requires the P0.3 answer.

`nago_wss::proto::message::Message` (`message.rs:17`) is
`Text/Binary/Ping/Pong/Close` over `Bytes`. That matches `WireMessage`. The
conversion is total in both directions.

The hyper upgrader additionally does HTTP/2 extended CONNECT
(`tungstenite/upgrader.rs` around `:192-208`), CORS and `OPTIONS`/`HEAD`
(`:37-88`, `:140-190`), `Origin`, `Server`, `Cache-Control`, and `Date` from
`WebsocketServer::cached_date` (`server.rs:46`, `:506`). nago-wss is RFC
6455 over HTTP/1.1. H2 has no path there. TLS session constructors exist on
both sides: `nago-rustls/src/session.rs:88` (`client`) and `:98` (`server`).

## 4. The queue

Lane 1 is independent of the decisions and independent of a published
nago-wss. Lane 2 is independent of the decisions and touches behaviour that
must be pinned before the queue changes shape. Lane 3 waits on every open
P0 it names. Lane 4 is last, because lanes 1 and 2 do not move
`cargo tree -e normal -i tokio`.

### P0. Decisions. Do not write the lane 3 code while any of these is open

- **P0.1. Where resolution lives.** `fn resolve` and `connect_any` are
  private in nago-wss. The crate root says they belong in nagoya.
  **Settled by:** a public function in one of those two crates, and a caller
  in endpoint-libs that names it. Exporting it from nago-wss is the small
  answer. Moving it is the one the crate already asks for.
- **P0.2. Signal delivery in nagoya.** Nobody has written `EVFILT_SIGNAL` or
  `signalfd`. It is new platform code in the reactor, and it is `unsafe`.
  **Settled by:** a nagoya waiter that observes `SIGINT`, `SIGTERM` and
  `SIGHUP` without tokio. `ws-core` cannot drop `tokio/signal` before this
  exists. The `CancellationToken` half does not wait on it. That is lane 1
  item 3.
- **P0.3. The writer.** `Connection<S>` is one owned object. `Toolbox::send`
  runs on an arbitrary task.
  **Settled by one of:** a clonable writer in nago-wss, or a bounded
  `WsMessage` queue owned by this crate, with `try_send` `Full` and `Closed`
  semantics, and not a third channel crate. A `futures` bounded channel
  reserves a slot per sender, so `drop_conn_on_buffer_full` would fire at a
  different depth than it does today (`transport.rs:76-78`). If the queue
  stays, it has to be ours.
- **P0.4. Which HTTP extras survive.** List, and decide each one: `Date`,
  `Server`, `Cache-Control`, CORS, `OPTIONS`, `HEAD`, `Origin`.
  **Settled by:** a row per header in `docs/decisions/`, keep or drop. A
  drop of `Date` deletes `cached_date` (`server.rs:46`, `:69`, `:164`,
  `:506`) and its one-second `tokio::spawn`. A keep means `build_response`
  gains an extra-headers parameter and `Request` retains the request
  headers. Both are upstream.
- **P0.5. HTTP/2 extended CONNECT.** nago-wss has no path to it.
  `ws-client`'s `connect_h2` is `client.rs:494`.
  **Settled by:** drop multiplexing, or keep hyper on its own feature and
  accept tokio on that feature. "Port it later" is not a settlement.

### Lane 1. Mechanical. Start now. No nago-wss dependency

These land on `fix/restore-release-trigger` or a branch off it. They do not
name nago-wss. They do not empty the tokio tree. Say that in the commit.

1. **Delete `WebsocketServer::message_receiver`.** `server.rs:43` and `:67`.
   Remove the three citations in the same change: `Cargo.toml:82`,
   `transport.rs:74`, `README.md:469`. Done when the identifier occurs
   nowhere in the repo.
2. **Replace `tokio::select!`.** `session.rs:377`, `server.rs:515`,
   `signal.rs:17`, `signal.rs:27`. Use `futures::future::select` and
   `Either`, which is already the idiom at `server.rs:191`. The three-way
   select in `session.rs` nests. Done when `rg 'tokio::select!'` under
   `src/libs/ws` and `src/libs/signal.rs` is empty, and the shutdown and
   session tests that exist still pass.
3. **Replace `CancellationToken`.** `signal.rs:3-6`. A `Notify` plus an
   `AtomicBool` for `is_cancelled`. `notify_waiters` is the broadcast.
   Independent of P0.2. Confirm `notify_waiters` on the nagoya version the
   manifest pins before writing the call. Done when `signal` no longer
   names `tokio-util`'s `CancellationToken`.

### Lane 2. Before any queue changes shape

4. **Give the two smuggled edges a home before touching the queue.**
   Teardown at `session.rs:386-389`. Close-on-policy at `toolbox.rs:243`
   and `:254`. Each gets a `Notify` or an explicit variant, and a test that
   fails if the edge is dropped. Do this even if P0.3 later deletes the
   queue.
5. **Hand-rolled `TOOLBOX` task-local.** Replace `tokio::task_local!` at
   `toolbox.rs:424` with a `thread_local!` and a guard future that sets on
   poll entry and restores on return, including on cancel. The awaits that
   motivated it are `session.rs:244` and `:342`. Done when those two still
   observe the toolbox after the await, by a test, and `scoped-tls` has not
   been added.
6. **`Date` cache, only after P0.4.** If `Date` is dropped, delete
   `cached_date` and the spawn. If it is kept, the refresh is
   `nagoya::time::sleep` on a shard thread, not `tokio::spawn`.

### Lane 3. Redesign. Blocked on the P0 it names, and on a published nago-wss

Do not merge this lane to master while nago-wss path-depends on nagoya or
nago-rustls.

7. **`MessageStream` over `nago_wss::Connection<S>`.** Newtype,
   `async_trait(?Send)`, `Message` to `WsMessage` over `Bytes`. This is the
   piece the rest of the lane is tested through. Blocked on the publish
   gate for a merge. A branch against the path checkout is allowed.
8. **`Addr` to `SocketAddr` for `V4` and `V6` only**, next to the existing
   nagoya transport. `Path` is an explicit error that names the address
   family. It touches `PeerIdentity`, which is public.
9. **Listener and shards.** `listener.rs` is `tokio::net::TcpListener`.
   `ConnectionListener`'s associated types are bounded on `tokio::io`. This
   is a second implementation over `nagoya::reactor::TcpListener`, not a
   swap. One listener per shard deletes the channel at `server.rs:490` and
   `:541`. That option does not exist in nagoya yet, so either nagoya grows
   it or this item keeps the fan-out. Do not invent `SO_REUSEPORT` at the
   call site and call it done.
10. **Outbound path, per P0.3.** Cloned writer, and the per-connection queue
    goes away. Or the queue stays, owned here, bounded, with `Full` and
    `Closed`. The two edges from lane 2 item 4 are still delivered.
11. **H1 upgrade.** `nago_wss::upgrade::accept` (`upgrade.rs:65`) replaces
    the hyper upgrader for HTTP/1. `upgrade_stream`'s
    `Receiver<UpgradeEvent>` becomes a plain value for H1, which removes
    `futures::channel::mpsc` from the public trait. H2 follows P0.5.
12. **`signal` delivery, after P0.2.** Rewrite `libs/signal.rs` against
    nagoya. Drop `tokio/signal` from the `signal` feature.
13. **`ws-client`.** `client.rs` is `tokio-tungstenite` and hyper.
    `connect_plain` and `connect_secure` cover the H1 client. `connect_h2`
    at `:494` is P0.5.

### Lane 4. After the tree can actually change

14. **`ws-core` stops naming `dep:tokio`.** Lanes 1 and 2 alone leave
    `cargo tree -e normal -i tokio` under
    `--features ws-core,framed-transport,nagoya-transport` still printing
    tokio. That is expected. Do not report those lanes as the tokio removal.
15. **`./scripts/check-features.sh` passes with each feature alone, and
    `./scripts/check-chain.sh` passes** for endpoint-gen, honey_id-types,
    endpoint-validator and the six backends. The feature-surface change is
    breaking for anyone on `framed_json` who must now ask for
    `framed-transport-tokio`. That break already shipped in 3.2.0's branch.
    This lane must not add another silent one.

Removing `parking_lot`, `dashmap` and `async-trait` from `ws-core` is a
different change. Do not bundle it.

## 5. Definition of done

- Lane 1 is merged, and the three `message_receiver` citations are gone.
- P0.1 through P0.5 each have a row in `docs/decisions/`.
- The two smuggled edges have a test each.
- Under the feature set karen uses,
  `cargo tree -e normal -i tokio` prints nothing, or P0.5 decided that the
  hyper feature keeps tokio and karen does not enable that feature.
- `check-features.sh` and `check-chain.sh` passed on the commit that claims
  this, and the commands are in the commit message's notes or in this file
  with the revision they were run against.
- No path dependency on nago-wss, nago-rustls or nagoya reached master.

## 6. Carrying this file

A turn ends when the lane it took is empty, when a measurement refutes the
item, or when the next item is a P0 nobody has settled. Finish an item, prune
it to one line with the proof commit, and take the next item in the same
lane if it is unblocked.

At the start: read the highest-numbered file in this series,
`endpoint-libs/AGENTS.md`, and `git status --short` plus HEAD for the four
repos in section 2. Resume the first unfinished item in the lane you were
given. Do not redo a done item unless the tree contradicts its proof.

At the end: bump the revision, prune finished items to one line, and record
what was not run. `check-chain.sh` is part of done for lane 4, not for a
lane 1 commit that does not touch the feature graph. Say which one you ran.

**Roll this file** when lane 1 and lane 2 are pruned stubs, or when a P0
settlement changes the shape of lane 3. The successor is the next number in
this series and is a complete document: state table, the corrections that are
still true, the unfinished items, and these instructions. Banner this file as
superseded and name the successor. Leave the old text in git. Do not rename
P0.1 through P0.5 or the lane numbers. They are the stable references across
the whole series.

Line numbers in this revision were read at the HEADs in section 2. They
will drift. Re-derive a line from the identifier before editing it. Do not
mine a superseded revision for lines.
