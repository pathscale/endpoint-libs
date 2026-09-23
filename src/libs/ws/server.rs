use super::WsMessage as Message;
#[cfg(feature = "ws")]
use eyre::eyre;
use eyre::{ContextCompat, Result, WrapErr, bail};
use itertools::Itertools;
use nagoya::reactor::{Handle, Reactor, block_on_with, resolve};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::*;

// Used by serve_connection, which is transport-agnostic (ws-core), so these must
// not be gated on the tungstenite backend.
use crate::libs::error_code::ErrorCode;
use crate::libs::handler::{RequestHandler, RequestHandlerErased};
use crate::libs::peer::{Extensions, PeerIdentity};
use crate::libs::signal::Shutdown;
use crate::libs::toolbox::{ArcToolbox, RequestContext, TOOLBOX, Toolbox};
use crate::libs::utils::{get_conn_id, get_log_id};
#[cfg(feature = "ws")]
use crate::libs::ws::NagoWssUpgrader;
use crate::libs::ws::mcp::{McpServerInfo, McpState};
use crate::libs::ws::{
    AfterRequest, BeforeRequest, BoxedStream, ConnectionListener, Hooks, MessageStream, OnConnect,
    OnDisconnect, SessionListener, TcpListener, WsClientSession, WsConnection, WsRequest,
    WsUpgrader,
};
use crate::model::{EndpointSchema, TypeRegistry};

use super::outbound;
use super::{AuthController, SimpleAuthController, WebsocketStates, WsEndpoint};

pub struct WebsocketServer {
    pub auth_controller: Arc<dyn AuthController>,
    pub handlers: HashMap<u32, WsEndpoint>,
    pub toolbox: ArcToolbox,
    pub config: WsServerConfig,
    pub upgrader: Option<Arc<dyn WsUpgrader>>,
    /// MCP surface state; `None` (the default) disables MCP entirely and the
    /// server behaves exactly as before. See [`WebsocketServer::enable_mcp`].
    pub mcp: Option<Arc<McpState>>,
    /// Interception hooks. Empty by default — an empty `Hooks` adds one branch per
    /// request and nothing else.
    pub hooks: Hooks,
}

impl WebsocketServer {
    pub fn new(config: WsServerConfig) -> Self {
        // The `insecure = true` warning is gone with the TLS it was about. It
        // would now fire on the only supported configuration and stay silent on
        // the one that is actually wrong, which is a cert configured against a
        // server that cannot serve one; `listen` refuses that outright.
        Self {
            auth_controller: Arc::new(SimpleAuthController),
            handlers: Default::default(),
            toolbox: Toolbox::new(),
            config,
            upgrader: default_upgrader(),
            mcp: None,
            hooks: Hooks::default(),
        }
    }

    /// Enables the MCP (JSON-RPC 2.0) surface on this server.
    ///
    /// Call after all `add_handler()` calls and before `listen()`: tool
    /// metadata is built once from the handlers registered so far. Fails on
    /// unresolved type references or duplicate tool names.
    pub fn enable_mcp(&mut self, registry: &TypeRegistry, info: McpServerInfo) -> Result<()> {
        let state = McpState::build(&self.handlers, registry, info)?;
        debug!(
            ws_server = true,
            "MCP enabled with {} tools",
            state.tools.len()
        );
        self.mcp = Some(Arc::new(state));
        Ok(())
    }

    fn validate_protocol_mode(&self) -> Result<()> {
        if self.config.mcp_only && self.mcp.is_none() {
            bail!("mcp_only requires enable_mcp before serving connections");
        }
        Ok(())
    }
    /// Register a hook that runs before every request, on both the legacy and MCP
    /// paths. Hooks run in registration order; the first error rejects the request.
    pub fn add_before_hook(&mut self, hook: impl BeforeRequest + 'static) {
        self.hooks.before.push(Arc::new(hook));
    }

    /// Register a hook that observes every completed request.
    pub fn add_after_hook(&mut self, hook: impl AfterRequest + 'static) {
        self.hooks.after.push(Arc::new(hook));
    }

    /// Register a hook that runs once per connection, after auth. Returning `Err`
    /// refuses the connection.
    pub fn add_on_connect_hook(&mut self, hook: impl OnConnect + 'static) {
        self.hooks.on_connect.push(Arc::new(hook));
    }

    /// Register a hook that runs once when a connection's session loop ends.
    pub fn add_on_disconnect_hook(&mut self, hook: impl OnDisconnect + 'static) {
        self.hooks.on_disconnect.push(Arc::new(hook));
    }

    pub fn set_auth_controller(&mut self, controller: impl AuthController + 'static) {
        self.auth_controller = Arc::new(controller);
    }
    pub fn set_upgrader(&mut self, upgrader: Arc<dyn WsUpgrader>) {
        self.upgrader = Some(upgrader);
    }
    pub fn add_handler<T: RequestHandler + 'static>(&mut self, handler: T) {
        let schema = serde_json::from_str(T::Request::SCHEMA).expect("Invalid schema");
        let roles: &[u32] = T::Request::ROLES;
        check_handler::<T>(&schema).expect("Invalid handler");
        self.add_handler_erased(schema, roles, Arc::new(handler))
    }
    pub fn add_handler_erased(
        &mut self,
        schema: EndpointSchema,
        roles: &[u32],
        handler: Arc<dyn RequestHandlerErased>,
    ) {
        let roles_set = roles.iter().cloned().collect::<HashSet<u32>>();

        let old = self.handlers.insert(
            schema.code,
            WsEndpoint {
                schema,
                handler,
                allowed_roles: roles_set,
            },
        );
        if let Some(old) = old {
            panic!(
                "Overwriting handler for endpoint {} {}",
                old.schema.code, old.schema.name
            );
        }
    }

    #[cfg(feature = "ws")]
    async fn handle_ws_handshake_and_connection(
        self: Arc<Self>,
        addr: SocketAddr,
        states: Arc<WebsocketStates>,
        stream: BoxedStream,
    ) -> Result<()> {
        let upgrader = self.upgrader.as_ref().ok_or_else(|| {
            eyre!("No WS backend configured; call set_upgrader() before listen()")
        })?;

        // One TCP connection, at most one WebSocket. The loop, the
        // `FuturesUnordered` of sessions and the `Receiver` they drained all
        // existed for the HTTP/2 arm, where one connection multiplexed a CONNECT
        // per stream. With no server TLS there is no ALPN, with no ALPN there is
        // no h2, and HTTP/1.1 upgrades exactly once and then the socket is
        // WebSocket frames to the end. So the upgrader hands back the connection
        // directly and this is a straight line.
        let Some(event) = upgrader.upgrade_stream(stream, addr, &self.config).await? else {
            // The request was answered in HTTP — a preflight, a HEAD, a health
            // check, or a refusal. Not an error and not a session: the response
            // is already on the wire and the socket is finished.
            debug!(
                ws_server = true,
                ?addr,
                "request answered in HTTP without upgrading"
            );
            return Ok(());
        };

        debug!(
            ws_server = true,
            ?addr,
            protocol = %event.protocol,
            "WsServer: upgrade succeeded, protocol received"
        );

        self.post_upgrade_connection(addr, states, event.stream, event.protocol)
            .await;

        debug!(
            ws_server = true,
            ?addr,
            "Connection handler finished (TCP connection closed)"
        );
        Ok(())
    }

    #[cfg(not(feature = "ws"))]
    async fn handle_ws_handshake_and_connection(
        self: Arc<Self>,
        _addr: SocketAddr,
        _states: Arc<WebsocketStates>,
        _stream: BoxedStream,
    ) -> Result<()> {
        bail!(
            "WebSocket upgrades require a backend that can create WS streams (enable the `ws` feature)"
        )
    }

    /// Run auth and the session loop for one already-established connection.
    ///
    /// This is the transport-agnostic server entry point. It knows nothing about TCP,
    /// TLS or HTTP upgrades: give it a [`MessageStream`] and a [`PeerIdentity`] and it
    /// does the rest. The WebSocket path reaches it through
    /// `post_upgrade_connection`; local transports (Unix socket, named pipe, XPC) call
    /// it directly.
    ///
    /// `auth_protocol` is whatever the transport uses to carry credentials at connect
    /// time — the WebSocket subprotocol string today, a handed-over token for local
    /// transports. It is passed to [`AuthController::auth`] unchanged.
    ///
    /// [`MessageStream`]'s futures are not `Send`, so this must be polled on
    /// the thread that owns the stream. [`Self::serve_with`] does that by
    /// driving connections with `FuturesUnordered` rather than `spawn_local`.
    pub async fn serve_connection(
        self: Arc<Self>,
        peer: PeerIdentity,
        states: Arc<WebsocketStates>,
        stream: Box<dyn MessageStream>,
        auth_protocol: Option<String>,
    ) {
        // OnConnect runs before the connection is registered, so a refused peer
        // never gets a slot in `states` and cannot be sent to.
        let mut extensions = Extensions::new();
        if let Err(err) = self.hooks.run_on_connect(&peer, &mut extensions).await {
            warn!(
                ws_server = true,
                peer = %peer,
                error_code = ?err.code,
                "connection refused by OnConnect hook"
            );
            return;
        }

        let conn = Arc::new(WsConnection {
            connection_id: get_conn_id(),
            user_id: Default::default(),
            roles: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            peer,
            extensions,
            log_id: get_log_id(),
        });
        debug!(
            ws_server = true,
            peer = %conn.peer,
            "New connection established {:?}",
            conn
        );

        let (tx, rx) = outbound::channel(self.config.message_buffer_size);
        states.insert(conn.connection_id, tx, conn.clone());

        let auth_result = Arc::clone(&self.auth_controller)
            .auth(
                &self.toolbox,
                auth_protocol.unwrap_or_default(),
                Arc::clone(&conn),
            )
            .await;
        let raw_ctx = RequestContext::from_conn(&conn);
        if let Err(err) = auth_result {
            self.toolbox
                .send_request_error(&raw_ctx, ErrorCode::BAD_REQUEST, err.to_string());
            error!(
                ws_server=true,
                error_code=?ErrorCode::BAD_REQUEST,
                peer=%conn.peer,
                user_id=raw_ctx.user_id,
                conn_id=raw_ctx.connection_id,
                roles=?raw_ctx.roles,
                error=%err,
                "Error while handling connection"
            );
            return;
        }

        self.handle_session_connection(conn, states, stream, rx)
            .await;
    }

    /// The WebSocket-specific wrapper: everything TCP and upgrade-shaped stops here,
    /// and the generic path continues in [`Self::serve_connection`].
    #[cfg(feature = "ws")]
    async fn post_upgrade_connection(
        self: Arc<Self>,
        addr: SocketAddr,
        states: Arc<WebsocketStates>,
        stream: Box<dyn MessageStream>,
        protocol: String,
    ) {
        self.serve_connection(PeerIdentity::Network(addr), states, stream, Some(protocol))
            .await;
    }

    pub async fn handle_session_connection(
        self: Arc<Self>,
        conn: Arc<WsConnection>,
        states: Arc<WebsocketStates>,
        stream: Box<dyn MessageStream>,
        rx: outbound::Receiver<Message>,
    ) {
        let addr = conn.peer.display();
        let context = RequestContext::from_conn(&conn);
        let conn_id = context.connection_id;
        let disconnect_hooks = self.hooks.clone();

        debug!(
            ws_server = true,
            ?addr,
            ?conn_id,
            "Starting websocket session"
        );
        let end = states
            .get_state(conn.connection_id)
            .map(|state| Arc::clone(&state.end));
        let mut session = WsClientSession::new(conn, stream, rx, self);
        if let Some(end) = end {
            session.bind_end(end);
        }
        session.run().await;

        states.remove(context.connection_id);
        disconnect_hooks
            .run_on_disconnect(context.connection_id, &context.peer)
            .await;
        info!(
            ws_server = true,
            ?addr,
            ?conn_id,
            "Connection closed and removed from states (check logs above for any errors)"
        );
    }

    /// Accept connections from any [`SessionListener`] and serve each one.
    ///
    /// The transport-agnostic counterpart to [`Self::listen`]. Both are one thread
    /// now, but for different reasons: a 1:1 sidecar channel has nothing to spread,
    /// and `listen` is one thread because a nagoya reactor owns the sockets it
    /// accepted (see [`Self::listen`]).
    ///
    /// Connections are polled in place with `FuturesUnordered`. That is the same
    /// one-thread model `spawn_local` had, without tying the method to a runtime, so
    /// a nagoya reactor can drive it. This one stays `async`: it takes an already
    /// built listener and no reactor of its own, so the caller decides what polls it.
    pub async fn serve_with<L>(self, listener: L) -> Result<()>
    where
        L: SessionListener + 'static,
    {
        use futures::StreamExt;
        use futures::future::FutureExt;
        use futures::stream::FuturesUnordered;

        self.validate_protocol_mode()?;
        let this = Arc::new(self);
        let states = Arc::new(WebsocketStates::new());
        this.toolbox.set_ws_states(
            states.clone_states(),
            this.config.header_only,
            this.config.drop_conn_on_buffer_full,
        );

        let mut connections = FuturesUnordered::new();
        loop {
            let accepted = if connections.is_empty() {
                listener.accept().await
            } else {
                let accept = listener.accept();
                futures::pin_mut!(accept);
                match futures::future::select(accept, connections.next()).await {
                    futures::future::Either::Left((accepted, _)) => accepted,
                    futures::future::Either::Right(_) => continue,
                }
            };
            let (stream, peer) = match accepted {
                Ok(accepted) => accepted,
                Err(err) => {
                    error!(ws_server = true, error = %err, "listener accept failed; stopping");
                    return Err(err);
                }
            };
            debug!(ws_server = true, peer = %peer, "accepted connection");

            let this = Arc::clone(&this);
            let states = Arc::clone(&states);
            connections.push(
                async move {
                    // Local transports carry credentials out of band (an inherited fd is
                    // already a capability), so there is no subprotocol string to pass.
                    this.serve_connection(peer, states, stream, None).await;
                }
                .boxed_local(),
            );
        }
    }

    /// Bind the configured address and serve until SIGTERM or SIGINT.
    ///
    /// **Breaking change: this is no longer `async`.** It owns the calling thread
    /// for the life of the server, because it now owns the reactor that drives it.
    /// A caller that used to `server.listen().await` inside a runtime calls
    /// `server.listen()` and gets its thread back when the server stops.
    ///
    /// Two things forced that, and both are properties of nagoya rather than of
    /// style. The name has to be resolved before any reactor is driven —
    /// [`resolve`] is `getaddrinfo`, a blocking `fn`, and nagoya has no
    /// `spawn_blocking`, so calling it from inside a running reactor would stall
    /// every socket on that reactor for the length of a DNS timeout. And a reactor
    /// has to exist before the listener can be bound, because a descriptor is
    /// registered with one specific reactor at birth. So the order is: resolve,
    /// create the reactor, bind, and only then start polling.
    ///
    /// # Shards
    ///
    /// A socket accepted by a nagoya listener is registered with that listener's
    /// reactor, and only that reactor will ever report its readiness, so the
    /// connection must be driven on the thread polling it. Handing accepted sockets
    /// to other threads would put every connection's readiness through one poller
    /// and add a cross-thread wake per message. So each shard is a thread with its
    /// own reactor and its own listening socket, all bound to one port with
    /// `SO_REUSEPORT`, and the kernel picks the shard per connection. This thread
    /// is the first shard. [`shard_count`] reads `WS_SHARDS`, then the cgroup CPU
    /// quota, then the CPU count. Linux balances across the sockets; macOS accepts
    /// the binds without balancing.
    ///
    /// # Signals
    ///
    /// This stops on SIGTERM or SIGINT, and claims both for the process while it
    /// runs: nagoya allows one waiter per signal number, so a second server in the
    /// same process fails with `EBUSY`. A process that owns its signals, or runs
    /// more than one server, calls [`Self::listen_until`] instead.
    pub fn listen(self) -> Result<()> {
        self.listen_on(None::<std::future::Pending<()>>)
    }

    /// [`Self::listen`], stopping when `stop` resolves instead of on a signal.
    ///
    /// No signal is registered. Signals belong to a process, not to a library
    /// server inside it, and taking them here is what made a second server in one
    /// process fail: an application that serves twice, and every test harness
    /// that starts a server per test. `stop` is polled on this server's own
    /// thread, so it has to be a future that needs no particular runtime, such as
    /// a oneshot receiver or [`crate::libs::signal::Shutdown::cancelled`].
    pub fn listen_until<F>(self, stop: F) -> Result<()>
    where
        F: std::future::Future<Output = ()> + 'static,
    {
        self.listen_on(Some(stop))
    }

    fn listen_on<F>(self, stop: Option<F>) -> Result<()>
    where
        F: std::future::Future<Output = ()> + 'static,
    {
        self.validate_protocol_mode()?;
        self.refuse_tls_config()?;
        debug!(ws_server = true, "Listening on {}", self.config.address);

        // Resolved here, while this is still an ordinary blocking function and no
        // reactor exists to stall. `resolve` returns every address the name has,
        // not the first; `bind_any` decides what to do with them.
        let (host, port) = split_host_port(&self.config.address)?;
        let addrs = resolve(host, port)
            .wrap_err_with(|| format!("Failed to lookup host to bind: {}", self.config.address))?;

        // This thread is the first shard: `block_on_with` below waits for readiness
        // and polls the futures on this thread, so its accept loop, the signal
        // waiters and its connections share one reactor. The signal waiters in
        // particular only fire while their reactor is polled, which is why they are
        // created inside this reactor's future rather than anywhere else.
        let reactor = Reactor::local()?;
        let handle = reactor.handle();
        let shards = shard_count();
        if shards <= 1 {
            let listener = TcpListener::bind_any(&addrs, &handle)?;
            return block_on_with(&reactor, async move {
                self.listen_impl(Arc::new(listener), &handle, stop).await
            });
        }

        // One listening socket per shard, each on its own reactor and thread, all
        // bound to one port with SO_REUSEPORT. The kernel gives each connection to
        // one socket, so a connection is accepted, registered and served by the same
        // reactor and never crosses a thread. The first bind picks the address; the
        // others take it exactly, which matters when the port asked for was zero.
        let listener = TcpListener::bind_any_shared(&addrs, &handle)?;
        let addr = listener.local_addr()?;
        let (this, states) = self.prepare();
        let shutdown = Arc::new(Shutdown::default());
        let mut threads = Vec::with_capacity(shards - 1);
        for shard in 1..shards {
            let this = Arc::clone(&this);
            let states = Arc::clone(&states);
            let stop = Arc::clone(&shutdown);
            let spawned = std::thread::Builder::new()
                .name(format!("ws-shard-{shard}"))
                .spawn(move || {
                    let served = (|| {
                        let reactor = Reactor::local()?;
                        let handle = reactor.handle();
                        let listener = TcpListener::bind_shared(addr, &handle)?;
                        let stop = async move { stop.cancelled().await };
                        block_on_with(
                            &reactor,
                            Self::serve_shard(this, states, Arc::new(listener), &handle, Some(stop)),
                        )
                    })();
                    if let Err(err) = &served {
                        error!(ws_server = true, shard, error = %err, "shard stopped with an error");
                    }
                    served
                });
            match spawned {
                Ok(thread) => threads.push(thread),
                Err(err) => {
                    shutdown.cancel();
                    for thread in threads {
                        let _ = thread.join();
                    }
                    return Err(eyre::eyre!("failed to start shard {shard}: {err}"));
                }
            }
        }
        info!(
            ws_server = true,
            shards, "serving on {shards} reactor threads"
        );

        let served = block_on_with(&reactor, async {
            let served = Self::serve_shard(this, states, Arc::new(listener), &handle, stop).await;
            // Whatever stopped this shard, a signal or an error, stops them all.
            shutdown.cancel();
            served
        });
        for thread in threads {
            if thread.join().is_err() {
                error!(ws_server = true, "a shard thread panicked");
            }
        }
        served
    }

    /// Refuse to start when the configuration expects TLS this server cannot serve.
    ///
    /// A certificate in the config used to mean "serve `wss://` here". It means
    /// nothing now, and the failure mode of ignoring it is the bad one: an
    /// operator who configured a key gets plaintext on the wire and no
    /// indication of it. So it is a startup error, which is loud, rather than a
    /// warning in a log nobody reads. Removing the fields outright would be the
    /// end state; they are deprecated instead so a consumer gets a compiler
    /// warning naming the fix before their build breaks.
    #[allow(deprecated)]
    fn refuse_tls_config(&self) -> Result<()> {
        if self.config.pub_certs.is_some() || self.config.priv_key.is_some() {
            bail!(
                "pub_certs/priv_key are set, but this server no longer serves TLS. \
                 Terminate TLS at the edge (fly.io `[http_service]` with \
                 force_https = true, plain to internal_port) and remove both fields."
            );
        }
        Ok(())
    }

    /// Accept, handshake and serve, all on the reactor `handle` names.
    ///
    /// One loop, three things to wait on, and no channel between them: a shard
    /// serves what its own listener accepts, so nothing is handed between threads.
    /// See [`Self::listen`] for how shards share a port.
    ///
    /// Connections are held in a `FuturesUnordered` and polled in place, the same
    /// model `serve_with` uses. nagoya's `TaskSet` is the purpose-built runner for
    /// non-`Send` tasks on one thread and would otherwise be the right choice, but
    /// it never removes a finished task's entry — the slot stays so a late wake
    /// still has a link to follow — so a set fed by an unbounded connection churn
    /// grows one entry per connection ever accepted and panics past four billion.
    /// It fits a fixed population of tasks, which a server's connections are not.
    /// `FuturesUnordered` drops what finishes and wakes in O(woken) just the same.
    async fn listen_impl<T, F>(
        self,
        listener: Arc<T>,
        handle: &Handle,
        stop: Option<F>,
    ) -> Result<()>
    where
        T: ConnectionListener + 'static,
        F: std::future::Future<Output = ()> + 'static,
    {
        let (this, states) = self.prepare();
        Self::serve_shard(this, states, listener, handle, stop).await
    }

    /// Share the server and one connection table across every shard.
    ///
    /// Once per server, not per shard: the toolbox reaches a connection through
    /// these states whichever reactor it lives on.
    fn prepare(self) -> (Arc<Self>, Arc<WebsocketStates>) {
        let states = Arc::new(WebsocketStates::new());
        let this = Arc::new(self);
        this.toolbox.set_ws_states(
            states.clone_states(),
            this.config.header_only,
            this.config.drop_conn_on_buffer_full,
        );
        (this, states)
    }

    /// The accept loop of one shard, on the reactor `handle` names.
    async fn serve_shard<T, F>(
        this: Arc<Self>,
        states: Arc<WebsocketStates>,
        listener: Arc<T>,
        handle: &Handle,
        stop: Option<F>,
    ) -> Result<()>
    where
        T: ConnectionListener + 'static,
        F: std::future::Future<Output = ()> + 'static,
    {
        use futures::StreamExt;
        use futures::future::{Either, FutureExt, select};
        use futures::stream::FuturesUnordered;

        // The caller's stop, or the process signals when it gave none. Signals are
        // registered on this reactor, and awaited on it below: a `Signal` only
        // fires while its own reactor is polled, so creating one anywhere else
        // would be creating a wait that never ends.
        let mut stop: futures::future::LocalBoxFuture<'static, ()> = match stop {
            Some(stop) => stop.boxed_local(),
            None => {
                let (mut sigterm, mut sigint) = crate::libs::signal::init_signals(handle)?;
                async move { crate::libs::signal::wait_for_signals(&mut sigterm, &mut sigint).await }
                    .boxed_local()
            }
        };
        let mut connections = FuturesUnordered::new();
        loop {
            // Shutdown is the outermost left arm, so a pending stop is not stuck
            // behind an accept or a live connection that is also ready. The one
            // stop future is polled across iterations rather than rebuilt, so a
            // caller's future is never dropped half-way.
            let shutdown = stop.as_mut();
            let accepted = listener.accept();
            futures::pin_mut!(accepted);
            let accepted = if connections.is_empty() {
                match select(shutdown, accepted).await {
                    Either::Left(_) => break,
                    Either::Right((accepted, _)) => accepted,
                }
            } else {
                let progress = select(accepted, connections.next());
                futures::pin_mut!(progress);
                match select(shutdown, progress).await {
                    Either::Left(_) => break,
                    Either::Right((Either::Left((accepted, _)), _)) => accepted,
                    // A connection finished. There is nothing to place, and the
                    // set has already dropped it, so go round again.
                    Either::Right((Either::Right(_), _)) => continue,
                }
            };
            let (stream, addr) = match accepted {
                Ok(x) => x,
                Err(err) => {
                    error!(ws_server = true, "Error while accepting stream: {:?}", err);
                    continue;
                }
            };

            let this = Arc::clone(&this);
            let states = Arc::clone(&states);
            let listener = Arc::clone(&listener);
            connections.push(
                async move {
                    let stream = match listener.handshake(stream).await {
                        Ok(channel) => {
                            debug!(ws_server = true, "Accepted stream from {}", addr);
                            channel
                        }
                        Err(err) => {
                            error!(
                                ws_server = true,
                                "Error while handshaking stream: {:?}", err
                            );
                            return;
                        }
                    };
                    if let Err(err) = TOOLBOX
                        .scope(
                            this.toolbox.clone(),
                            this.handle_ws_handshake_and_connection(addr, states, Box::new(stream)),
                        )
                        .await
                    {
                        error!(
                            ws_server = true,
                            ?addr,
                            "Failed to handle WS connection: {err}"
                        );
                    }
                }
                .boxed_local(),
            );
        }

        // Breaking out drops `connections`, which closes every live session at
        // once. That was already the observable behaviour: the shard threads were
        // detached and died with the process, and nothing waited for them either.
        Ok(())
    }

    pub fn dump_schemas(&self) -> Result<()> {
        let _ = std::fs::create_dir_all("docs");
        let file = format!("docs/{}_alive_endpoints.json", self.config.name);
        let available_schemas: Vec<String> = self
            .handlers
            .values()
            .map(|x| x.schema.name.clone())
            .sorted()
            .collect();
        debug!(
            ws_server = true,
            "Dumping {} endpoint names to {}",
            available_schemas.len(),
            file
        );
        serde_json::to_writer_pretty(File::create(file)?, &available_schemas)?;
        Ok(())
    }
}

/// Split `host:port` for [`resolve`], which takes the two separately.
///
/// `tokio::net::lookup_host` took the whole string and did this itself. nagoya's
/// resolver does not, on purpose: the port never goes to `getaddrinfo` as a service
/// string, it is stamped onto every address the name resolves to, so it has to be a
/// number before the lookup rather than after.
///
/// The host is split from the right and unbracketed, which is what makes a literal
/// IPv6 address work: `[::1]:8080` has a colon in the host and brackets exist to say
/// where it ends.
fn split_host_port(address: &str) -> Result<(&str, u16)> {
    let (host, port) = address
        .rsplit_once(':')
        .with_context(|| format!("address has no port: {address}"))?;
    let port: u16 = port
        .parse()
        .wrap_err_with(|| format!("address has no usable port: {address}"))?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    Ok((host, port))
}

/// Determine the number of WebSocket shards the operator asked for.
///
/// Read but not yet honoured: [`WebsocketServer::listen`] serves on one thread
/// because a nagoya listener cannot be shared between reactors, and says so when
/// this returns more than one. The detection is kept because it is the part that
/// was right, and the fan-out is the part that has to come back.
///
/// Resolution order (first match wins):
/// 1. `WS_SHARDS` environment variable — explicit operator override.
/// 2. cgroup v1 CPU quota — handles older Docker/k8s deployments that set
///    `cpu.cfs_quota_us` / `cpu.cfs_period_us` but do not use cgroup v2.
/// 3. `std::thread::available_parallelism` — reads cgroup v2 `cpu.max` and
///    `sched_getaffinity` on Linux (Rust 1.74+), logical CPU count elsewhere.
/// 4. Hard fallback of 1 if all detection fails.
fn shard_count() -> usize {
    // 1. Explicit override.
    if let Ok(val) = std::env::var("WS_SHARDS") {
        if let Ok(n) = val.trim().parse::<usize>()
            && n > 0
        {
            return n;
        }
        warn!(
            ws_server = true,
            "WS_SHARDS env var set but invalid, ignoring: {:?}", val
        );
    }

    // 2. cgroup v1 quota (common in older Docker / k8s).
    if let Some(n) = read_cgroup_v1_quota() {
        return n.max(1);
    }

    // 3. stdlib — cgroup v2 + affinity-aware on Linux (Rust 1.74+).
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Read the cgroup v1 CPU quota and convert it to a thread count.
/// Returns `None` if the files are absent, unparseable, or the quota is
/// unlimited (quota == -1).
fn read_cgroup_v1_quota() -> Option<usize> {
    let quota: i64 = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if quota <= 0 {
        return None; // -1 means no limit
    }
    let period: i64 = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if period <= 0 {
        return None;
    }
    // Ceiling division: round up so a 1.5-CPU quota gives 2 shards.
    Some(((quota + period - 1) / period) as usize)
}

pub fn check_name(cat: &str, be_name: &str, should_name: &str) -> Result<()> {
    if !be_name.contains(should_name) {
        bail!("{} name should be {} but got {}", cat, should_name, be_name);
    } else {
        Ok(())
    }
}

pub fn check_handler<T: RequestHandler + 'static>(schema: &EndpointSchema) -> Result<()> {
    let handler_name = std::any::type_name::<T>();
    let should_handler_name = format!("Method{}", schema.name);
    check_name("Method", handler_name, &should_handler_name)?;
    let request_name = std::any::type_name::<T::Request>();
    let should_req_name = format!("{}Request", schema.name);
    check_name("Request", request_name, &should_req_name)?;

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsServerConfig {
    #[serde(default)]
    pub name: String,
    pub address: String,
    /// Certificate chain for the TLS this server no longer serves.
    ///
    /// Kept only so a consumer's config struct still compiles; setting it is a
    /// startup error, see [`WebsocketServer::listen`]. Deserialising is
    /// unaffected either way — serde ignores fields it does not know — so a
    /// deployment's config file can carry the key until someone removes it.
    #[deprecated(note = "TLS is terminated at the edge; this server serves plain ws:// only")]
    #[serde(default)]
    pub pub_certs: Option<Vec<PathBuf>>,
    /// Private key for the TLS this server no longer serves. See [`Self::pub_certs`].
    #[deprecated(note = "TLS is terminated at the edge; this server serves plain ws:// only")]
    #[serde(default)]
    pub priv_key: Option<PathBuf>,
    /// Formerly "bind without TLS". Ignored: there is no other mode to select.
    ///
    /// It is neither read nor warned about now. A `false` here used to mean
    /// "serve `wss://`", and honouring that would mean refusing to start every
    /// server whose config predates this change — including every one that never
    /// set the field, since `false` is its default.
    #[deprecated(
        note = "ignored; this server serves plain ws:// only and TLS terminates at the edge"
    )]
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub debug: bool,
    /// Per-connection outbound message buffer. Default: 256.
    #[serde(default = "WsServerConfig::default_message_buffer_size")]
    pub message_buffer_size: usize,
    /// When true, send a Close frame and drop the connection when the send
    /// buffer is full instead of silently discarding the message.
    #[serde(default)]
    pub drop_conn_on_buffer_full: bool,
    #[serde(skip)]
    pub header_only: bool,
    #[serde(skip)]
    pub allow_cors_urls: Arc<Option<Vec<String>>>,
    #[serde(default = "WsServerConfig::default_server_name")]
    pub server_name: String,
    /// Reject every non-JSON-RPC frame. Requires [`WebsocketServer::enable_mcp`].
    #[serde(default)]
    pub mcp_only: bool,
}

impl Default for WsServerConfig {
    // The three TLS fields are deprecated and still have to be initialised here;
    // the allow is about naming them, not about using them for anything.
    #[allow(deprecated)]
    fn default() -> Self {
        Self {
            name: Default::default(),
            address: Default::default(),
            pub_certs: None,
            priv_key: None,
            insecure: false,
            debug: false,
            message_buffer_size: Self::default_message_buffer_size(),
            drop_conn_on_buffer_full: false,
            header_only: false,
            allow_cors_urls: Arc::new(None),
            server_name: Self::default_server_name(),
            mcp_only: false,
        }
    }
}

impl WsServerConfig {
    fn default_message_buffer_size() -> usize {
        256
    }
    fn default_server_name() -> String {
        "RustWebsocketServer/1.0".to_string()
    }
}

fn default_upgrader() -> Option<Arc<dyn WsUpgrader>> {
    #[cfg(feature = "ws")]
    {
        Some(Arc::new(NagoWssUpgrader))
    }
    #[cfg(not(feature = "ws"))]
    {
        None
    }
}

#[cfg(test)]
mod protocol_mode_tests {
    use super::*;

    #[test]
    fn mcp_only_requires_the_mcp_router() {
        let mut server = WebsocketServer::new(WsServerConfig {
            mcp_only: true,
            ..Default::default()
        });
        assert!(server.validate_protocol_mode().is_err());

        server
            .enable_mcp(
                &TypeRegistry::new(),
                McpServerInfo {
                    name: "test".into(),
                    version: "0".into(),
                },
            )
            .expect("empty MCP surface should initialize");
        server
            .validate_protocol_mode()
            .expect("MCP-only server should validate after enable_mcp");
    }
}
