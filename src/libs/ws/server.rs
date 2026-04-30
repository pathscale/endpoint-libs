use super::WsMessage as Message;
use eyre::{ContextCompat, Result, bail, eyre};
use itertools::Itertools;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tracing::*;

use crate::libs::error_code::ErrorCode;
use crate::libs::handler::{RequestHandler, RequestHandlerErased};
use crate::libs::toolbox::{ArcToolbox, RequestContext, TOOLBOX, Toolbox};
use crate::libs::utils::{get_conn_id, get_log_id};
#[cfg(feature = "ws")]
use crate::libs::ws::HyperTungsteniteUpgrader;
#[cfg(any(feature = "ws", feature = "ws-wtx"))]
use crate::libs::ws::TlsListener;
#[cfg(all(feature = "ws-wtx", not(feature = "ws")))]
use crate::libs::ws::WtxUpgrader;
use crate::libs::ws::{
    BoxedStream, ConnectionListener, TcpListener, WsClientSession, WsConnection, WsRequest,
    WsStream, WsUpgrader,
};
use crate::model::EndpointSchema;

use super::{AuthController, ConnectionId, SimpleAuthController, WebsocketStates, WsEndpoint};

pub struct WebsocketServer {
    pub auth_controller: Arc<dyn AuthController>,
    pub handlers: HashMap<u32, WsEndpoint>,
    pub message_receiver: parking_lot::Mutex<Option<mpsc::Receiver<ConnectionId>>>,
    pub toolbox: ArcToolbox,
    pub config: WsServerConfig,
    pub cached_date: RwLock<String>,
    pub upgrader: Option<Arc<dyn WsUpgrader>>,
}

impl WebsocketServer {
    pub fn new(config: WsServerConfig) -> Self {
        if config.insecure {
            tracing::warn!(
                ws_server = true,
                "WS Server has been configured with insecure=true, is this desired?"
            )
        }
        Self {
            auth_controller: Arc::new(SimpleAuthController),
            handlers: Default::default(),
            message_receiver: parking_lot::Mutex::new(None),
            toolbox: Toolbox::new(),
            cached_date: RwLock::new(httpdate::fmt_http_date(std::time::SystemTime::now())),
            config,
            upgrader: default_upgrader(),
        }
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
    async fn handle_ws_handshake_and_connection(
        self: Arc<Self>,
        addr: SocketAddr,
        states: Arc<WebsocketStates>,
        stream: BoxedStream,
    ) -> Result<()> {
        let cached_date = self.cached_date.read().clone();
        let upgrader = self.upgrader.as_ref().ok_or_else(|| {
            eyre!("No WS backend configured; call set_upgrader() before listen()")
        })?;
        match upgrader
            .upgrade(stream, addr, &self.config, &cached_date)
            .await
        {
            Ok((ws_stream, protocol)) => {
                debug!(ws_server = true, ?addr, protocol = %protocol, "WsServer: upgrade succeeded, protocol received");
                self.post_upgrade_connection(addr, states, ws_stream, protocol)
                    .await;
                Ok(())
            }
            Err(e) if e.to_string().contains("not a websocket upgrade") => {
                debug!(
                    ws_server = true,
                    ?addr,
                    "Non-WebSocket HTTP request ignored"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn post_upgrade_connection(
        self: Arc<Self>,
        addr: SocketAddr,
        states: Arc<WebsocketStates>,
        stream: Box<dyn WsStream>,
        protocol: String,
    ) {
        let conn = Arc::new(WsConnection {
            connection_id: get_conn_id(),
            user_id: Default::default(),
            roles: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            address: addr,
            log_id: get_log_id(),
        });
        debug!(
            ws_server = true,
            ?addr,
            "New connection handshaken {:?}",
            conn
        );

        let (tx, rx) = mpsc::channel(self.config.message_buffer_size);
        states.insert(conn.connection_id, tx, conn.clone());

        let auth_result = Arc::clone(&self.auth_controller)
            .auth(&self.toolbox, protocol, Arc::clone(&conn))
            .await;
        let raw_ctx = RequestContext::from_conn(&conn);
        if let Err(err) = auth_result {
            self.toolbox
                .send_request_error(&raw_ctx, ErrorCode::BAD_REQUEST, err.to_string());
            error!(
                ws_server=true,
                error_code=?ErrorCode::BAD_REQUEST,
                ip_addr=%raw_ctx.ip_addr,
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

    pub async fn handle_session_connection(
        self: Arc<Self>,
        conn: Arc<WsConnection>,
        states: Arc<WebsocketStates>,
        stream: Box<dyn WsStream>,
        rx: mpsc::Receiver<Message>,
    ) {
        let addr = conn.address;
        let context = RequestContext::from_conn(&conn);
        let conn_id = context.connection_id;

        debug!(
            ws_server = true,
            ?addr,
            ?conn_id,
            "Starting websocket session"
        );
        let session = WsClientSession::new(conn, stream, rx, self);
        session.run().await;

        states.remove(context.connection_id);
        info!(
            ws_server = true,
            ?addr,
            ?conn_id,
            "Connection closed and removed from states (check logs above for any errors)"
        );
    }

    pub async fn listen(self) -> Result<()> {
        debug!(ws_server = true, "Listening on {}", self.config.address);

        // Resolve the address and get the socket address
        let addr = tokio::net::lookup_host(&self.config.address)
            .await?
            .next()
            .with_context(|| format!("Failed to lookup host to bind: {}", self.config.address))?;

        let listener = TcpListener::bind(addr).await?;
        if self.config.insecure {
            self.listen_impl(Arc::new(listener)).await
        } else {
            self.listen_tls(listener).await
        }
    }

    #[cfg(any(feature = "ws", feature = "ws-wtx"))]
    async fn listen_tls(self, listener: TcpListener) -> Result<()> {
        if self.config.pub_certs.is_some() && self.config.priv_key.is_some() {
            let listener = TlsListener::bind(
                listener,
                self.config.pub_certs.clone().unwrap(),
                self.config.priv_key.clone().unwrap(),
            )
            .await?;
            self.listen_impl(Arc::new(listener)).await
        } else {
            bail!("pub_certs and priv_key should be set")
        }
    }

    #[cfg(not(any(feature = "ws", feature = "ws-wtx")))]
    async fn listen_tls(self, _listener: TcpListener) -> Result<()> {
        bail!(
            "TLS requires a ws backend that provides TLS support (e.g. the `ws` or `ws-wtx` feature)"
        )
    }

    async fn listen_impl<T: ConnectionListener + 'static>(self, listener: Arc<T>) -> Result<()> {
        let states = Arc::new(WebsocketStates::new());
        let this = Arc::new(self);
        this.toolbox.set_ws_states(
            states.clone_states(),
            this.config.header_only,
            this.config.drop_conn_on_buffer_full,
        );

        let num_shards = shard_count();
        debug!(ws_server = true, "Starting {} WebSocket shards", num_shards);

        let mut shard_senders = Vec::with_capacity(num_shards);
        for _ in 0..num_shards {
            let (tx, rx) = mpsc::channel::<(T::Channel1, SocketAddr)>(256);
            let this = Arc::clone(&this);
            let states = Arc::clone(&states);
            let listener = Arc::clone(&listener);
            std::thread::spawn(move || {
                WebsocketServer::run_shard(this, states, listener, rx);
            });
            shard_senders.push(tx);
        }

        // Date cache updater — runs on the multi-thread scheduler, no LocalSet needed.
        tokio::spawn({
            let this = Arc::clone(&this);
            async move {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    *this.cached_date.write() =
                        httpdate::fmt_http_date(std::time::SystemTime::now());
                }
            }
        });

        let (mut sigterm, mut sigint) = crate::libs::signal::init_signals()?;
        let mut shard_idx: usize = 0;
        loop {
            tokio::select! {
                _ = crate::libs::signal::wait_for_signals(&mut sigterm, &mut sigint) => break,
                accepted = listener.accept() => {
                    let (stream, addr) = match accepted {
                        Ok(x) => x,
                        Err(err) => {
                            error!(ws_server=true, "Error while accepting stream: {:?}", err);
                            continue;
                        }
                    };
                    let shard = &shard_senders[shard_idx % num_shards];
                    shard_idx = shard_idx.wrapping_add(1);
                    if shard.send((stream, addr)).await.is_err() {
                        error!(ws_server=true, "Shard channel closed unexpectedly for addr {}", addr);
                    }
                }
            }
        }

        Ok(())
    }

    fn run_shard<T: ConnectionListener + 'static>(
        this: Arc<Self>,
        states: Arc<WebsocketStates>,
        listener: Arc<T>,
        mut rx: mpsc::Receiver<(T::Channel1, SocketAddr)>,
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build shard runtime");
        let local_set = LocalSet::new();
        rt.block_on(local_set.run_until(async move {
            loop {
                let Some((stream, addr)) = rx.recv().await else {
                    break;
                };
                let this = Arc::clone(&this);
                let states = Arc::clone(&states);
                let listener = Arc::clone(&listener);
                tokio::task::spawn_local(async move {
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
                });
            }
        }));
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

/// Determine the number of WebSocket shards to spawn.
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
        if let Ok(n) = val.trim().parse::<usize>() {
            if n > 0 {
                return n;
            }
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
    #[serde(default)]
    pub pub_certs: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub priv_key: Option<PathBuf>,
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
}

impl Default for WsServerConfig {
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
        return Some(Arc::new(HyperTungsteniteUpgrader));
    }
    #[cfg(all(feature = "ws-wtx", not(feature = "ws")))]
    {
        return Some(Arc::new(WtxUpgrader));
    }
    #[cfg(not(any(feature = "ws", feature = "ws-wtx")))]
    {
        None
    }
}
