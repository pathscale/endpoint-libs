# Decisions for the ws-core port onto nago-wss

Append only. A row here is a settlement of one P0 in
`docs/pending/ws-core-onto-nago-wss-02.md` (or its successor). The queue
names the open items; this file does not restate them.

Checked against endpoint-libs `fix/ws-core-lane1` at `488fa5f`, nagoya
`feat/resolve` at `e0ff612`, nago-wss `feat/nagoya-resolve` at `6e52935`.

## P0.3. The writer

The per-connection `WsMessage` queue stays in endpoint-libs.

`Connection<S>` is one owned object. Its write methods take `&mut self`, and
the nago-wss checkout that has the write-path measurements is
`bench/nagoya-write-path`. A clonable writer would be a second owner of that
socket. `Toolbox::send` runs on an arbitrary task and reaches a connection by
id; the queue is that route. `WsMessage` is a payload, and a payload keeps a
queue.

The queue has to be this crate's. A `futures` bounded channel reserves a slot
per sender, so `drop_conn_on_buffer_full` would fire at a different depth
than it does with `tokio::sync::mpsc`. `try_send` keeps `Full` and `Closed`.
`Outbound::Closed` stays `recv() -> None`. `WsStreamState::end` stays the
policy flag. The public `send_ws_msg` and `send_serialized_ws_msg` still do
not take the `Shutdown`. `set_ws_states` is what passes it into `enqueue`.

Lane 3 item 10 is the replacement of `tokio::sync::mpsc` with that channel.
It does not wait on a published nago-wss. It does not add a channel crate.

## P0.4. HTTP extras

`agencyproxy`'s proxy crate enables `ws-http1` and sets
`WsServerConfig::allow_cors_urls`. `server_name` is a public field on the
same config and the hyper upgrader already emits it. Those are the consumers
the keep rows are about. nago-wss `build_response` is a fixed 101
(`Upgrade`, `Connection`, `Sec-WebSocket-Accept`, and `Sec-WebSocket-Protocol`
when one was selected) and `build_rejection` is a fixed 400, 426 or 431.
Item 11 is where a keep row becomes bytes on that path. This row does not
edit nago-wss.

| Extra | Decision | Why |
| --- | --- | --- |
| `Date` | drop | Nothing reads it. RFC 6455 does not require it, and the nago-wss 101 does not send it. The only implementation is `WebsocketServer::cached_date` and the one-second `tokio::spawn` in `listen_impl`. |
| `Server` | keep | `WsServerConfig::server_name`, default `RustWebsocketServer/1.0`. The hyper upgrader emits it. Item 11 adds it to the nago-wss 101 from that field. |
| `Cache-Control` | keep | `no-store`, and only on 4xx and 5xx, so a browser does not cache a refused handshake. Item 11 adds it to `build_rejection`. |
| CORS | keep | `allow_cors_urls`. `None` means `*`. Item 11 carries the same list. |
| `OPTIONS` | keep | CORS preflight, answered by the hyper upgrader today. Item 11 answers it on the H1 front. |
| `HEAD` | keep | The upgrader answers it and advertises it in `Allow`. Item 11 does the same. |
| `Origin` | keep | Read from the request to apply `allow_cors_urls`. Item 11's `Request` retains it. It is not a response header of its own. |

The drop of `Date` deletes `cached_date`, the spawn, the `cached_date`
parameter of `WsUpgrader::upgrade_stream`, and the `Date` header. `ws-core`
stops naming `tokio/time` and `httpdate`. The other rows leave the hyper
upgrader as it is until item 11.

## P0.5. HTTP/2 extended CONNECT

HTTP/2 extended CONNECT stays on the `ws` and `ws-client` features, which
already name hyper and tokio. It is not ported to nago-wss.

Karen enables `ws-core`, `framed-transport` and `nagoya-transport`. It does
not enable `ws` or `ws-client`. `agencyproxy`'s proxy enables `ws-http1`,
which is HTTP/1. Dropping multiplexing would remove `connect_h2` and the H2
arm of the hyper upgrader from features that still expose them. Keeping
those features, and the tokio they pull in, is the settlement. A consumer
that wants no tokio does not enable them.

Item 11 replaces the hyper upgrader for H1 only. The H2 arm stays on hyper,
and `upgrade_stream`'s `Receiver<UpgradeEvent>` stays with it. Item 13 moves
`connect_plain` and `connect_secure`. `connect_h2` stays where it is.

## Follow-up

`Date`'s deletion landed in `9f43c32` on `fix/ws-core-lane1`. Under
`--features ws-core,framed-transport,nagoya-transport`, `cargo tree -e normal
-i tokio` prints tokio with features `net`, `rt`, `sync` and `signal`.
`time` is absent. `./scripts/check-features.sh` with
`CHECK_CMD="cargo check --offline --lib"` passed on that commit.
`./scripts/check-chain.sh` was not run. The `--all-targets` form of the
feature script was not re-run; it pulls the dev-dependency's `tokio` with
`features = ["full"]` and would hide a missing `tokio/time`.

## P0.2. Signal delivery

The waiter is `nagoya::reactor::Signal` on `feat/resolve` at `ee8fca0`.

`Signal::new(kind, &Handle)` registers one descriptor. `recv` completes
once per delivery. `SignalKind::interrupt`, `terminate` and `hangup` are
`SIGINT`, `SIGTERM` and `SIGHUP`. A second waiter for the same number gets
`EBUSY`. A number outside `1..32`, and `SIGKILL` or `SIGSTOP`, gets
`EINVAL`.

Linux uses `signalfd`. The signal is blocked on the calling thread and
stays blocked after drop. A thread created afterwards inherits the block.
A thread that already existed does not, and a signal delivered there takes
the default action. `pthread_sigmask` is per thread.

The BSDs have no `signalfd`. `EVFILT_SIGNAL` names a signal number, and
`Registration` names a file descriptor, so it is not used. A pipe written
from a handler is the descriptor. The handler is installed while the
signal is blocked, and the write end is published before the block is
lifted. Both ends stay open for the process: the handler may already have
loaded the write end. Drop restores the previous disposition. A signal
still pending at that moment is discarded, because restoring the default
and then unblocking would terminate the process on `SIGINT`.

The kqueue path was executed on macOS. `cargo test --features reactor
--lib` ran 42 tests, 7 of them this waiter: delivery of `SIGHUP`,
`SIGINT` and `SIGTERM` without killing the process, a parked `SIGTERM`,
a cross-thread `SIGHUP`, two waiters not completing each other, and drop
restoring the previous disposition. `cargo check --target
x86_64-unknown-linux-gnu --features reactor --lib` typechecked the
`signalfd` path. That path was not executed.

endpoint-libs still depends on crates.io nagoya `0.1.9`, which has no
`Signal`. Item 12 is the caller in `libs/signal.rs`, which still uses
`tokio::signal`. It does not get a path dependency.

## End state. No tokio, and no path

Appended 2026-09-22, after endpoint-libs `3531fd6`. This withdraws the
part of P0.5 that treated hyper's tokio as something the port keeps.

Every feature of endpoint-libs loses its dependency on tokio. Nothing is
left on purpose: not `signal`, not the shard channel, not `TcpListener`,
not hyper, not the per-shard runtime, not `scheduler`, `log_reader`,
`error_aggregation`, `log_throttling`, or `otel`. A feature that still
names tokio is unfinished.

The replacements are published crates: nagoya, nago-wss, nago-rustls, and
the other Pathscale forks (`parking_lot_lite_hack` is already the
`parking_lot` dependency). Versions go in `Cargo.toml`. There is no
`Cargo.lock` in this repository, and no path dependency on a sibling
checkout. `ps-spsc` is a single-producer queue and is not the
per-connection outbound queue.

crates.io nagoya is `0.1.9`. It has `Notify::notify_waiters` and no
`reactor::Signal`, `resolve`, or `connect_any`. Those three are on the
sibling `feat/resolve` at `ee8fca0` (version `0.1.10`) and nowhere else.
A caller that needs them waits until a published nagoya contains them.
Pointing `Cargo.toml` at `../nagoya` is not how this branch builds.

nago-wss `feat/nagoya-resolve` at `6e52935` already path-depends on
`../nagoya` and `../nago-rustls`. That was `3d069ed` ("Benchmark against
the local nagoya, not the published one"), which replaced crates.io
nagoya `0.1.6` and nago-rustls `0.1.1`. This port did not add it and did
not edit nago-wss, nagoya, or nago-rustls. nago-wss and nago-rustls each
commit a `Cargo.lock`. endpoint-libs does not. Do not copy the path or
the lock into endpoint-libs.
