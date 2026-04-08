use eyre::{ContextCompat, Result, bail, eyre};
use http_body_util::Empty;
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
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
use crate::libs::ws::{ConnectionListener, TcpListener};
#[cfg(feature = "tls")]
use crate::libs::ws::TlsListener;
use crate::libs::ws::{WsClientSession, WsConnection};
use crate::model::EndpointSchema;

use super::{AuthController, ConnectionId, SimpleAuthController, WebsocketStates, WsEndpoint};

pub struct WebsocketServer {
    pub auth_controller: Arc<dyn AuthController>,
    pub handlers: HashMap<u32, WsEndpoint>,
    pub allowed_roles: HashMap<u32, HashSet<u32>>,
    pub message_receiver: Option<mpsc::Receiver<ConnectionId>>,
    pub toolbox: ArcToolbox,
    pub config: WsServerConfig,
}


impl WebsocketServer {
    pub fn new(config: WsServerConfig) -> Self {
        if config.insecure {
            tracing::warn!("WS Server has been configured with insecure=true, is this desired?")
        }
        Self {
            auth_controller: Arc::new(SimpleAuthController),
            allowed_roles: HashMap::new(),
            handlers: Default::default(),
            message_receiver: None,
            toolbox: Toolbox::new(),
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

        let _old_roles = self.allowed_roles.insert(schema.code, roles_set);

        let old = self
            .handlers
            .insert(schema.code, WsEndpoint { schema, handler });
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
                let is_upgrade = req
                    .headers()
                    .get(UPGRADE)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.eq_ignore_ascii_case("websocket"))
                    .unwrap_or(false);
                let key = req
                    .headers()
                    .get(SEC_WEBSOCKET_KEY)
                    .map(|k| k.as_bytes().to_vec());

                if !is_upgrade || key.is_none() {
                    let mut resp = Response::new(Empty::<Bytes>::new());
                    *resp.status_mut() = StatusCode::BAD_REQUEST;
                    return Ok::<_, Infallible>(resp);
                }
                let derived = derive_accept_key(&key.unwrap());

                let protocol = req
                    .headers()
                    .get("Sec-WebSocket-Protocol")
                    .or_else(|| req.headers().get("sec-websocket-protocol"))
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

                let mut resp = Response::new(Empty::<Bytes>::new());
                *resp.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
                resp.headers_mut()
                    .append(CONNECTION, "upgrade".parse().unwrap());
                resp.headers_mut()
                    .append(UPGRADE, "websocket".parse().unwrap());
                resp.headers_mut()
                    .append(SEC_WEBSOCKET_ACCEPT, derived.parse().unwrap());

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
                            resp.headers_mut().append(
                                "Access-Control-Allow-Credentials",
                                "true".parse().unwrap(),
                            );
                        }
                    }
                }

                resp.headers_mut()
                    .append("Date", chrono::Utc::now().to_rfc2822().parse().unwrap());
                resp.headers_mut()
                    .append("Server", "RustWebsocketServer/1.0".parse().unwrap());

                Ok::<_, Infallible>(resp)
            }
        });

        http1::Builder::new()
            .serve_connection(io, service)
            .with_upgrades()
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
            #[cfg(feature = "tls")]
            {
                // Proceed with binding the listener for secure mode
                let listener = TlsListener::bind(
                    listener,
                    self.config.pub_certs.clone().unwrap(),
                    self.config.priv_key.clone().unwrap(),
                )
                .await?;
                return self.listen_impl(Arc::new(listener)).await;
            }
            #[cfg(not(feature = "tls"))]
            bail!("TLS certs configured but binary was compiled without the 'tls' feature; recompile with --features tls")
        } else {
            bail!("pub_certs and priv_key should be set")
        }
    }

    async fn listen_impl<T: ConnectionListener + 'static>(self, listener: Arc<T>) -> Result<()> {
        let states = Arc::new(WebsocketStates::new());
        self.toolbox
            .set_ws_states(states.clone_states(), self.config.header_only);
        let this = Arc::new(self);
        let local_set = LocalSet::new();
        let (mut sigterm, mut sigint) = crate::libs::signal::init_signals()?;
        local_set
            .run_until(async {
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
                            let listener = Arc::clone(&listener);
                            let this = Arc::clone(&this);
                            let states = Arc::clone(&states);
                            local_set.spawn_local(async move {
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

                                let _ = TOOLBOX.scope(this.toolbox.clone(), this.handle_ws_handshake_and_connection(addr, states, stream)).await;
                            });
                        }
                    }
                }
                Ok(())
            })
            .await
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
