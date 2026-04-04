use eyre::{ContextCompat, Result, bail, eyre};
use http_body_util::Empty;
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE};
use hyper::header::HeaderValue;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use itertools::Itertools;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fs::File;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tracing::*;

use crate::libs::error_code::ErrorCode;
use crate::libs::handler::{RequestHandler, RequestHandlerErased};
use crate::libs::toolbox::{ArcToolbox, RequestContext, TOOLBOX, Toolbox};
use crate::libs::utils::{get_conn_id, get_log_id};
use crate::libs::ws::client::WsRequest;
use crate::libs::ws::{ConnectionListener, TcpListener, TlsListener};
use crate::libs::ws::{WsClientSession, WsConnection};
use crate::model::EndpointSchema;

use super::{AuthController, ConnectionId, SimpleAuthController, WebsocketStates, WsEndpoint};

static HDR_UPGRADE: HeaderValue = HeaderValue::from_static("upgrade");
static HDR_WEBSOCKET: HeaderValue = HeaderValue::from_static("websocket");
static HDR_SERVER: HeaderValue = HeaderValue::from_static("RustWebsocketServer/1.0");
static HDR_CREDENTIALS_TRUE: HeaderValue = HeaderValue::from_static("true");

pub struct WebsocketServer {
    pub auth_controller: Arc<dyn AuthController>,
    pub handlers: HashMap<u32, WsEndpoint>,
    pub message_receiver: parking_lot::Mutex<Option<mpsc::Receiver<ConnectionId>>>,
    pub toolbox: ArcToolbox,
    pub config: WsServerConfig,
    pub cached_date: RwLock<HeaderValue>,
}


impl WebsocketServer {
    pub fn new(config: WsServerConfig) -> Self {
        if config.insecure {
            tracing::warn!("WS Server has been configured with insecure=true, is this desired?")
        }
        Self {
            auth_controller: Arc::new(SimpleAuthController),
            handlers: Default::default(),
            message_receiver: parking_lot::Mutex::new(None),
            toolbox: Toolbox::new(),
            cached_date: RwLock::new(
                httpdate::fmt_http_date(std::time::SystemTime::now())
                    .parse()
                    .unwrap(),
            ),
            config,
        }
    }
    pub fn set_auth_controller(&mut self, controller: impl AuthController + 'static) {
        self.auth_controller = Arc::new(controller);
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

        let old = self
            .handlers
            .insert(schema.code, WsEndpoint { schema, handler, allowed_roles: roles_set });
        if let Some(old) = old {
            panic!(
                "Overwriting handler for endpoint {} {}",
                old.schema.code, old.schema.name
            );
        }
    }
    async fn handle_ws_handshake_and_connection<
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    >(
        self: Arc<Self>,
        addr: SocketAddr,
        states: Arc<WebsocketStates>,
        stream: S,
    ) -> Result<()> {
        let io = TokioIo::new(stream);

        let service = service_fn(move |mut req: Request<Incoming>| {
            let this = Arc::clone(&self);
            let states = Arc::clone(&states);
            async move {
                let is_http2 = req.version() == Version::HTTP_2;

                // Validate the upgrade request based on HTTP version.
                // HTTP/1.1: GET + Upgrade: websocket + Sec-WebSocket-Key
                // HTTP/2:   CONNECT + :protocol = websocket (RFC 8441 / RFC 9113 §8.5)
                let derived = if is_http2 {
                    if req.method() != Method::CONNECT {
                        let mut resp = Response::new(Empty::<Bytes>::new());
                        *resp.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
                        return Ok::<_, Infallible>(resp);
                    }
                    let proto_ok = req
                        .extensions()
                        .get::<hyper::ext::Protocol>()
                        .map_or(false, |p| p.as_str() == "websocket");
                    if !proto_ok {
                        let mut resp = Response::new(Empty::<Bytes>::new());
                        *resp.status_mut() = StatusCode::BAD_REQUEST;
                        return Ok::<_, Infallible>(resp);
                    }
                    None
                } else {
                    let is_upgrade = req
                        .headers()
                        .get(UPGRADE)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.eq_ignore_ascii_case("websocket"))
                        .unwrap_or(false);
                    if !is_upgrade {
                        let mut resp = Response::new(Empty::<Bytes>::new());
                        *resp.status_mut() = StatusCode::BAD_REQUEST;
                        return Ok::<_, Infallible>(resp);
                    }
                    let Some(key) = req.headers().get(SEC_WEBSOCKET_KEY) else {
                        let mut resp = Response::new(Empty::<Bytes>::new());
                        *resp.status_mut() = StatusCode::BAD_REQUEST;
                        return Ok::<_, Infallible>(resp);
                    };
                    Some(derive_accept_key(key.as_bytes()))
                };

                let protocol = req
                    .headers()
                    .get("Sec-WebSocket-Protocol")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                let origin = req
                    .headers()
                    .get("Origin")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                let on_upgrade = hyper::upgrade::on(&mut req);

                let toolbox = this.toolbox.clone();
                let this_upgrade = Arc::clone(&this);
                let protocol_upgrade = protocol.clone();
                tokio::task::spawn_local(async move {
                    match on_upgrade.await {
                        Ok(upgraded) => {
                            let ws_stream = WebSocketStream::from_raw_socket(
                                TokioIo::new(upgraded),
                                Role::Server,
                                None,
                            )
                            .await;
                            let _ = TOOLBOX
                                .scope(
                                    toolbox,
                                    this_upgrade.post_upgrade_connection(
                                        addr,
                                        states,
                                        ws_stream,
                                        protocol_upgrade,
                                    ),
                                )
                                .await;
                        }
                        Err(e) => error!(?addr, "WS upgrade error: {e}"),
                    }
                });

                // HTTP/2: respond 200 OK (RFC 9113 §8.5); no 101 or Sec-WebSocket-Accept.
                // HTTP/1.1: respond 101 Switching Protocols with the derived accept key.
                let mut resp = Response::new(Empty::<Bytes>::new());
                if let Some(derived) = derived {
                    *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                    resp.headers_mut().append(CONNECTION, HDR_UPGRADE.clone());
                    resp.headers_mut().append(UPGRADE, HDR_WEBSOCKET.clone());
                    resp.headers_mut()
                        .append(SEC_WEBSOCKET_ACCEPT, derived.parse().unwrap());
                }

                if !protocol.is_empty() {
                    let first = protocol.split(',').next().unwrap_or("").trim();
                    if let Ok(val) = first.parse::<hyper::header::HeaderValue>() {
                        resp.headers_mut().append("Sec-WebSocket-Protocol", val);
                    }
                }

                if let Some(origin) = origin {
                    let allow = match this.config.allow_cors_urls.as_ref() {
                        Some(domains) => domains.iter().any(|d| d == &origin),
                        None => true,
                    };
                    if allow {
                        if let Ok(v) = origin.parse::<hyper::header::HeaderValue>() {
                            resp.headers_mut()
                                .append("Access-Control-Allow-Origin", v);
                            resp.headers_mut()
                                .append("Access-Control-Allow-Credentials", HDR_CREDENTIALS_TRUE.clone());
                        }
                    }
                }

                resp.headers_mut().append("Date", this.cached_date.read().clone());
                resp.headers_mut().append("Server", HDR_SERVER.clone());

                Ok::<_, Infallible>(resp)
            }
        });

        let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
        builder.http2().enable_connect_protocol();
        builder
            .serve_connection_with_upgrades(io, service)
            .await
            .map_err(|e| eyre!(e))
    }

    async fn post_upgrade_connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        self: Arc<Self>,
        addr: SocketAddr,
        states: Arc<WebsocketStates>,
        stream: WebSocketStream<S>,
        protocol: String,
    ) {
        let conn = Arc::new(WsConnection {
            connection_id: get_conn_id(),
            user_id: Default::default(),
            roles: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            address: addr,
            log_id: get_log_id(),
        });
        debug!(?addr, "New connection handshaken {:?}", conn);

        let (tx, rx) = mpsc::channel(100);
        states.insert(conn.connection_id, tx, conn.clone());

        let auth_result = Arc::clone(&self.auth_controller)
            .auth(&self.toolbox, protocol, Arc::clone(&conn))
            .await;
        let raw_ctx = RequestContext::from_conn(&conn);
        if let Err(err) = auth_result {
            self.toolbox
                .send_request_error(&raw_ctx, ErrorCode::BAD_REQUEST, err.to_string());
            error!(
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

        info!(
            ip_addr=%raw_ctx.ip_addr,
            user_id=raw_ctx.user_id,
            conn_id=raw_ctx.connection_id,
            roles=?raw_ctx.roles,
            "WS connection request valid, proceeding to initiate the session"
        );
        self.handle_session_connection(conn, states, stream, rx)
            .await;
    }

    pub async fn handle_session_connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        self: Arc<Self>,
        conn: Arc<WsConnection>,
        states: Arc<WebsocketStates>,
        stream: WebSocketStream<S>,
        rx: mpsc::Receiver<Message>,
    ) {
        let addr = conn.address;
        let context = RequestContext::from_conn(&conn);

        let session = WsClientSession::new(conn, stream, rx, self);
        session.run().await;

        states.remove(context.connection_id);
        debug!(?addr, "Connection closed");
    }

    pub async fn listen(self) -> Result<()> {
        info!("Listening on {}", self.config.address);

        // Resolve the address and get the socket address
        let addr = tokio::net::lookup_host(&self.config.address)
            .await?
            .next()
            .with_context(|| format!("Failed to lookup host to bind: {}", self.config.address))?;

        let listener = TcpListener::bind(addr).await?;
        if self.config.insecure {
            self.listen_impl(Arc::new(listener)).await
        } else if self.config.pub_certs.is_some() && self.config.priv_key.is_some() {
            // Proceed with binding the listener for secure mode
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

    async fn listen_impl<T: ConnectionListener + 'static>(self, listener: Arc<T>) -> Result<()> {
        let states = Arc::new(WebsocketStates::new());
        let this = Arc::new(self);
        this.toolbox
            .set_ws_states(states.clone_states(), this.config.header_only);

        let num_shards = shard_count();
        info!("Starting {} WebSocket shards", num_shards);

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
                        httpdate::fmt_http_date(std::time::SystemTime::now())
                            .parse()
                            .unwrap();
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
                            error!("Error while accepting stream: {:?}", err);
                            continue;
                        }
                    };
                    let shard = &shard_senders[shard_idx % num_shards];
                    shard_idx = shard_idx.wrapping_add(1);
                    if shard.send((stream, addr)).await.is_err() {
                        error!("Shard channel closed unexpectedly for addr {}", addr);
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
            while let Some((stream, addr)) = rx.recv().await {
                let this = Arc::clone(&this);
                let states = Arc::clone(&states);
                let listener = Arc::clone(&listener);
                tokio::task::spawn_local(async move {
                    let stream = match listener.handshake(stream).await {
                        Ok(channel) => {
                            debug!("Accepted stream from {}", addr);
                            channel
                        }
                        Err(err) => {
                            error!("Error while handshaking stream: {:?}", err);
                            return;
                        }
                    };
                    let _ = TOOLBOX
                        .scope(
                            this.toolbox.clone(),
                            this.handle_ws_handshake_and_connection(addr, states, stream),
                        )
                        .await;
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
        info!(
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
        warn!("WS_SHARDS env var set but invalid, ignoring: {:?}", val);
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

pub fn wrap_ws_error<T>(err: Result<T, WsError>) -> Result<T> {
    err.map_err(|x| eyre!(x))
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    #[serde(skip)]
    pub header_only: bool,
    #[serde(skip)]
    pub allow_cors_urls: Arc<Option<Vec<String>>>,
}
