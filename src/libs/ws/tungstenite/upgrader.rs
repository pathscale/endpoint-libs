use std::convert::Infallible;
use std::net::SocketAddr;

use async_trait::async_trait;
use eyre::{Result, eyre};
use futures::{SinkExt, StreamExt};
use http_body_util::Empty;
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONNECTION, HeaderValue, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tracing::*;

use super::super::{
    WsMessage as Message, WsServerConfig,
    traits::{BoxedStream, StreamError, WsStream, WsUpgrader},
};

static HDR_UPGRADE: HeaderValue = HeaderValue::from_static("upgrade");
static HDR_WEBSOCKET: HeaderValue = HeaderValue::from_static("websocket");
static HDR_SERVER: HeaderValue = HeaderValue::from_static("RustWebsocketServer/1.0");
static HDR_CREDENTIALS_TRUE: HeaderValue = HeaderValue::from_static("true");

struct UpgradeEvent {
    on_upgrade: hyper::upgrade::OnUpgrade,
    protocol: String,
}

pub struct HyperTungsteniteUpgrader;

#[async_trait]
impl WsUpgrader for HyperTungsteniteUpgrader {
    async fn upgrade(
        &self,
        stream: BoxedStream,
        addr: SocketAddr,
        config: &WsServerConfig,
        cached_date: &str,
    ) -> Result<(Box<dyn WsStream>, String)> {
        let io = TokioIo::new(stream);
        let (tx, mut rx) = mpsc::channel::<UpgradeEvent>(8);

        let svc_config = config.clone();
        let svc_date = cached_date.to_owned();
        let svc_addr = addr;

        let service = service_fn(move |mut req: Request<Incoming>| {
            let tx = tx.clone();
            let config = svc_config.clone();
            let cached_date = svc_date.clone();
            let addr = svc_addr;

            async move {
                let is_http2 = req.version() == Version::HTTP_2;
                let is_options = req.method() == Method::OPTIONS;
                debug!(
                    ws_server = true, ?addr,
                    method = %req.method(), uri = %req.uri(),
                    version = ?req.version(), is_http2, is_options,
                    "WS handshake request received"
                );

                let origin = req
                    .headers()
                    .get("Origin")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                let access_control_request_method = req
                    .headers()
                    .get("Access-Control-Request-Method")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                let access_control_request_headers = req
                    .headers()
                    .get("Access-Control-Request-Headers")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                if is_options && access_control_request_method.is_some() {
                    debug!(ws_server = true, ?addr, "CORS preflight request detected");
                    let mut resp = Response::new(Empty::<Bytes>::new());
                    *resp.status_mut() = StatusCode::OK;
                    add_cors_headers(&mut resp, &origin, &config, &access_control_request_headers);
                    resp.headers_mut().append(
                        "Date",
                        cached_date
                            .parse::<HeaderValue>()
                            .unwrap_or_else(|_| HeaderValue::from_static("")),
                    );
                    resp.headers_mut().append("Server", HDR_SERVER.clone());
                    return Ok::<_, Infallible>(resp);
                }

                let derived = if is_http2 {
                    if req.method() == Method::GET {
                        let resp = Response::new(Empty::<Bytes>::new());
                        return Ok::<_, Infallible>(resp);
                    }
                    if req.method() != Method::CONNECT {
                        debug!(ws_server = true, ?addr, method=%req.method(), "H2: rejected — expected CONNECT method");
                        let mut resp = Response::new(Empty::<Bytes>::new());
                        *resp.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
                        return Ok::<_, Infallible>(resp);
                    }
                    let protocol_ext = req
                        .extensions()
                        .get::<hyper::ext::Protocol>()
                        .map(|p| p.as_str().to_owned());
                    let proto_ok = protocol_ext.as_deref() == Some("websocket");
                    debug!(
                        ws_server = true,
                        ?addr,
                        ?protocol_ext,
                        proto_ok,
                        "H2: checking :protocol extension"
                    );
                    if !proto_ok {
                        debug!(
                            ws_server = true,
                            ?addr,
                            "H2: rejected — :protocol != websocket"
                        );
                        let mut resp = Response::new(Empty::<Bytes>::new());
                        *resp.status_mut() = StatusCode::BAD_REQUEST;
                        return Ok::<_, Infallible>(resp);
                    }
                    debug!(
                        ws_server = true,
                        ?addr,
                        "H2: CONNECT upgrade valid, responding 200 OK"
                    );
                    None
                } else {
                    let is_upgrade = req
                        .headers()
                        .get(UPGRADE)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.eq_ignore_ascii_case("websocket"))
                        .unwrap_or(false);
                    if !is_upgrade {
                        debug!(
                            ws_server = true,
                            ?addr,
                            "H1: rejected — missing or invalid Upgrade: websocket header"
                        );
                        let mut resp = Response::new(Empty::<Bytes>::new());
                        *resp.status_mut() = StatusCode::BAD_REQUEST;
                        return Ok::<_, Infallible>(resp);
                    }
                    let Some(key) = req.headers().get(SEC_WEBSOCKET_KEY) else {
                        debug!(
                            ws_server = true,
                            ?addr,
                            "H1: rejected — missing Sec-WebSocket-Key"
                        );
                        let mut resp = Response::new(Empty::<Bytes>::new());
                        *resp.status_mut() = StatusCode::BAD_REQUEST;
                        return Ok::<_, Infallible>(resp);
                    };
                    debug!(
                        ws_server = true,
                        ?addr,
                        "H1: upgrade valid, responding 101 Switching Protocols"
                    );
                    Some(derive_accept_key(key.as_bytes()))
                };

                let protocol = req
                    .headers()
                    .get("Sec-WebSocket-Protocol")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                let on_upgrade = hyper::upgrade::on(&mut req);
                tx.send(UpgradeEvent {
                    on_upgrade,
                    protocol: protocol.clone(),
                })
                .await
                .ok();

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
                    if let Ok(val) = first.parse::<HeaderValue>() {
                        resp.headers_mut().append("Sec-WebSocket-Protocol", val);
                    }
                }

                add_cors_headers(&mut resp, &origin, &config, &access_control_request_headers);
                resp.headers_mut().append(
                    "Date",
                    cached_date
                        .parse::<HeaderValue>()
                        .unwrap_or_else(|_| HeaderValue::from_static("")),
                );
                resp.headers_mut().append("Server", HDR_SERVER.clone());

                Ok::<_, Infallible>(resp)
            }
        });

        // The builder borrows into the conn future via serve_connection_with_upgrades,
        // making conn non-'static. To spawn conn as a background task (required to keep
        // the H2 framing layer alive after the WS upgrade), builder must be created
        // inside the spawned task so its lifetime is tied to the task, not to this fn.
        let (result_tx, result_rx) = oneshot::channel::<Result<(Box<dyn WsStream + 'static>, String)>>();

        tokio::task::spawn_local(async move {
            let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            builder.http2().enable_connect_protocol();
            let conn = builder.serve_connection_with_upgrades(io, service);
            tokio::pin!(conn);

            // The upgrade event is sent from inside the service_fn, which runs within
            // conn's poll. This means rx.recv() cannot be Ready before conn is polled,
            // so biased select cannot help — the event is buffered only after conn runs.
            //
            // Two cases:
            //   rx arm: conn is still running (H2 always; H1 when network write is async).
            //           conn_done = false → drive conn in inner loop, keep alive after.
            //   conn arm: H1 with synchronous TLS write — conn completes in a single poll.
            //             The service already buffered the event; retrieve with try_recv.
            //             conn_done = true → await on_upgrade directly, skip conn.await.
            let (event, mut conn_done) = tokio::select! {
                event = rx.recv() => {
                    // tx lives inside service_fn owned by conn; None only if tx dropped
                    // before conn completes — structurally impossible here.
                    (event.unwrap(), false)
                }
                result = &mut conn => {
                    match rx.try_recv() {
                        Ok(event) => (event, true),
                        Err(_) => {
                            // No event buffered: truly a non-WS request (CORS preflight,
                            // health check, rejected upgrade).
                            let e = result.map_err(|e| eyre!(e))
                                .and(Err(eyre!("not a websocket upgrade: connection closed without upgrade event")));
                            result_tx.send(e).ok();
                            return;
                        }
                    }
                }
            };

            let UpgradeEvent { on_upgrade, protocol } = event;
            debug!(ws_server = true, ?addr, "Upgrade event received, completing handshake");
            tokio::pin!(on_upgrade);

            // Resolve on_upgrade. For H1 sync path (conn_done=true), hyper has already
            // resolved on_upgrade internally before conn returned Ready — await directly.
            // For H2 / slow H1 (conn_done=false), drive conn concurrently so hyper can
            // complete the upgrade handshake; track if conn finishes mid-loop.
            let upgraded_result = if conn_done {
                on_upgrade.await.map_err(|e| eyre!(e))
            } else {
                loop {
                    tokio::select! {
                        biased;
                        result = &mut on_upgrade => break result.map_err(|e| eyre!(e)),
                        result = &mut conn, if !conn_done => match result {
                            Ok(()) => conn_done = true,
                            Err(e) => break Err(eyre!(e)),
                        }
                    }
                }
            };

            let upgraded = match upgraded_result {
                Ok(u) => u,
                Err(e) => { result_tx.send(Err(e)).ok(); return; }
            };
            let ws = WebSocketStream::from_raw_socket(
                TokioIo::new(upgraded),
                Role::Server,
                None,
            ).await;
            let stream: Box<dyn WsStream + 'static> = Box::new(HyperWsStream { inner: ws });
            result_tx.send(Ok((stream, protocol))).ok();
            // H2: conn drives H2 framing for the logical WS stream — must keep running.
            // H1: conn is already done (conn_done=true); do not re-poll a completed future.
            if !conn_done {
                let _ = conn.await;
            }
        });

        result_rx.await.map_err(|_| eyre!("upgrade task exited before producing result"))?
    }
}

struct HyperWsStream {
    inner: WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>,
}

#[async_trait(?Send)]
impl WsStream for HyperWsStream {
    async fn send(&mut self, msg: Message) -> Result<(), StreamError> {
        self.inner.send(msg).await.map_err(map_err)
    }

    async fn recv(&mut self) -> Option<Result<Message, StreamError>> {
        self.inner.next().await.map(|r| r.map_err(map_err))
    }
}

fn map_err(e: tokio_tungstenite::tungstenite::Error) -> StreamError {
    match e {
        tokio_tungstenite::tungstenite::Error::ConnectionClosed
        | tokio_tungstenite::tungstenite::Error::AlreadyClosed => StreamError::Closed,
        tokio_tungstenite::tungstenite::Error::WriteBufferFull(_) => StreamError::WriteBufferFull,
        tokio_tungstenite::tungstenite::Error::Protocol(s) => StreamError::Protocol(s.to_string()),
        other => StreamError::Other(eyre!(other)),
    }
}

fn add_cors_headers(
    resp: &mut Response<Empty<Bytes>>,
    origin: &Option<String>,
    config: &WsServerConfig,
    access_control_request_headers: &Option<String>,
) {
    let Some(origin) = origin else {
        return;
    };

    let allow = match config.allow_cors_urls.as_ref() {
        Some(domains) => domains.iter().any(|d| d == origin),
        None => true,
    };
    if !allow {
        return;
    }

    if let Ok(v) = origin.parse::<HeaderValue>() {
        resp.headers_mut().append("Access-Control-Allow-Origin", v);
        resp.headers_mut().append(
            "Access-Control-Allow-Credentials",
            HDR_CREDENTIALS_TRUE.clone(),
        );
        resp.headers_mut().append(
            "Access-Control-Allow-Methods",
            "GET, OPTIONS, CONNECT".parse().unwrap(),
        );
        if let Some(req_headers) = access_control_request_headers {
            if let Ok(v) = req_headers.parse::<HeaderValue>() {
                resp.headers_mut().append("Access-Control-Allow-Headers", v);
            }
        } else {
            resp.headers_mut().append(
                "Access-Control-Allow-Headers",
                "Content-Type, Authorization, Sec-WebSocket-Key, Sec-WebSocket-Version, Sec-WebSocket-Extensions, Sec-WebSocket-Protocol"
                    .parse()
                    .unwrap(),
            );
        }
        resp.headers_mut()
            .append("Access-Control-Max-Age", "86400".parse().unwrap());
    }
}
