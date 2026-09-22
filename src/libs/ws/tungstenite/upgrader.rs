//! The server side of the RFC 6455 opening handshake, over nago-wss.
//!
//! # What replaced what
//!
//! This was hyper: a `service_fn` handed to `hyper_util`'s auto builder, an
//! `OnUpgrade` future posted through an mpsc, and `tokio_tungstenite` wrapping
//! the upgraded socket. All three are gone. The handshake is now HTTP/1.1 only
//! and is performed inline by [`nago_wss::proto::handshake`], and the upgraded
//! socket becomes a [`nago_wss::conn::Connection`] before this function returns.
//!
//! The HTTP/2 arm went with it. Extended CONNECT (RFC 8441) is only reachable
//! after an ALPN negotiation, ALPN only happens inside TLS, and this crate no
//! longer serves TLS — every deployment terminates it at the edge and speaks
//! plain `ws://` to the internal port. With no ALPN there is no h2 to negotiate,
//! so `enable_connect_protocol`, the `:protocol` check and the `spawn_local`
//! that drove the h2 connection have nothing left to do. That `spawn_local` was
//! a live runtime panic in any case: the `LocalSet` underneath it was deleted
//! when the server moved to a nagoya reactor.
//!
//! # Why the handshake is spelled out here instead of calling `upgrade::accept`
//!
//! [`nago_wss::upgrade::accept`] takes a fixed set of extra response headers, and
//! the headers this server must emit are not fixed: `Access-Control-Allow-Origin`
//! is either `*` or the request's own `Origin`, and which one depends on a
//! request that has not been parsed yet when `accept` is called. It also answers
//! only upgrades, and this endpoint has to answer `OPTIONS` preflights, `HEAD`
//! and plain `GET` — requests nago-wss deliberately hands back whole
//! ([`Parsed::Plain`]) precisely because it does not know anyone's CORS policy
//! and will not invent one.
//!
//! So the pieces are used rather than the wrapper: [`head_end`] and
//! [`parse_request`] to read and parse, [`build_response`] and
//! [`build_rejection`] to answer, and [`Connection::with_buffered`] to carry the
//! bytes a fast client sent in the same segment as its request. The only thing
//! duplicated from nago-wss is the loop that reads until `head_end` reports a
//! complete head; see the note on [`read_head`].
//!
//! # Which headers survive, and why `Date` does not
//!
//! The settled set is `Server` (from [`WsServerConfig::server_name`]),
//! `X-Content-Type-Options: nosniff`, `Cache-Control: no-store` on anything 4xx
//! or 5xx, and the CORS set driven by [`WsServerConfig::allow_cors_urls`]. `Date`
//! is deliberately absent: it was a per-response `format!` of the current time
//! behind a cache that needed its own task to tick, RFC 9110 requires it only of
//! a server with a clock it trusts, and nothing in the fleet reads it.

use std::net::SocketAddr;

use async_trait::async_trait;
use bytes::BytesMut;
use eyre::{Result, eyre};
use nago_wss::conn::{Connection, Error as ConnError, Role};
use nago_wss::proto::handshake::{
    DEFAULT_MAX_HEAD, Method, Parsed, Request, UpgradeError, build_rejection, build_response,
    head_end, parse_request,
};
use nago_wss::proto::message::Limits;
use nago_wss::stream::StreamExt;
use tracing::*;

use super::super::{
    WsMessage as Message, WsServerConfig,
    traits::{BoxedStream, MessageStream, StreamError, UpgradeEvent, WsUpgrader},
};

/// The methods this endpoint answers, for `Allow` and `Access-Control-Allow-Methods`.
///
/// `CONNECT` used to be in this list because the h2 arm accepted it. There is no
/// h2 arm, so advertising it would be advertising a method that now returns 405.
const ALLOWED_METHODS: &str = "GET, HEAD, OPTIONS";

/// The request headers a browser is told it may send on a WebSocket open.
///
/// Sent when a preflight did not name its own set. `Sec-WebSocket-*` are here
/// because a cross-origin open carries them and a browser that has not been told
/// they are allowed will not send them.
const DEFAULT_ALLOW_HEADERS: &str = "Content-Type, Authorization, Sec-WebSocket-Key, \
     Sec-WebSocket-Version, Sec-WebSocket-Extensions, Sec-WebSocket-Protocol";

/// A [`BoxedStream`] takes nago-wss's slow paths.
///
/// [`nago_wss::stream::StreamExt`] has no blanket impl on purpose — Rust has no
/// specialisation, and a blanket one would collide with the socket's own, which
/// is the entire reason the trait has overridable defaults. A boxed stream has
/// already erased whatever socket was underneath, so the vectored write and the
/// uninitialised read are unreachable through it and the defaults are exactly
/// right: one extra copy on the upgrade path, which is paid once per connection.
///
/// This belongs next to the `nagoya::io::Stream` impl for the same type in
/// `traits.rs`; it is here only because that file has another owner this pass.
impl StreamExt for BoxedStream {}

pub struct NagoWssUpgrader;

/// The pre-2.0 name, kept so a consumer that named the backend still compiles.
///
/// It is a lie about the implementation now — there is no hyper and no
/// tungstenite under it — which is why the type it aliases was renamed rather
/// than left alone.
#[deprecated(note = "renamed to NagoWssUpgrader; hyper and tungstenite are gone from this path")]
pub type HyperTungsteniteUpgrader = NagoWssUpgrader;

#[async_trait(?Send)]
impl WsUpgrader for NagoWssUpgrader {
    async fn upgrade_stream(
        &self,
        mut stream: BoxedStream,
        addr: SocketAddr,
        config: &WsServerConfig,
    ) -> Result<Option<UpgradeEvent>> {
        // The two failures below happen before anything is known about the
        // request, so their CORS set is whatever applies with no `Origin` in
        // hand. Built inside the arms rather than up front: it allocates, and on
        // the path that matters — a successful upgrade — it is never wanted.
        let (head, rest) = match read_head(&mut stream).await? {
            HeadOutcome::Head(head, rest) => (head, rest),
            HeadOutcome::Eof => {
                debug!(
                    ws_server = true,
                    ?addr,
                    "peer closed before finishing its request head"
                );
                return Ok(None);
            }
            HeadOutcome::TooLarge => {
                debug!(ws_server = true, ?addr, "request head over the size limit");
                let extra = Headers::new(config, None, None).error_extra();
                let rejection = build_rejection(UpgradeError::HeadTooLarge, &borrow(&extra));
                write_all(&mut stream, &rejection).await?;
                return Ok(None);
            }
        };

        let parsed = match parse_request(&head, DEFAULT_MAX_HEAD) {
            Ok(parsed) => parsed,
            Err(error) => {
                // Best effort, exactly as nago-wss's own `accept` treats it: the
                // connection is being refused either way, so failing to deliver
                // the reason does not change the outcome.
                debug!(ws_server = true, ?addr, ?error, "unparseable request head");
                let extra = Headers::new(config, None, None).error_extra();
                let rejection = build_rejection(error, &borrow(&extra));
                let _ = write_all(&mut stream, &rejection).await;
                return Ok(None);
            }
        };

        let request = match parsed {
            // Not a WebSocket open. nago-wss hands these back whole rather than
            // collapsing them into one 400, which is what lets the three cases
            // below be told apart at all.
            Parsed::Plain(request) => {
                let headers = Headers::from_request(config, &request);
                let response = match &request.method {
                    // A preflight, a plain GET and a HEAD are all answered 200
                    // with the CORS set. The difference between a preflight and
                    // the others is only which `Allow-Headers` goes out, and
                    // `Headers` has already read the preflight's ask; the
                    // difference between HEAD and GET is a body, and every
                    // response on this path carries none either way.
                    Method::Options | Method::Get | Method::Head => {
                        plain_response("200 OK", &headers.ok_extra())
                    }
                    Method::Other(method) => {
                        debug!(
                            ws_server = true,
                            ?addr,
                            %method,
                            "method not allowed on the WebSocket endpoint"
                        );
                        let mut extra = headers.error_extra();
                        extra.push(("Allow".to_string(), ALLOWED_METHODS.to_string()));
                        plain_response("405 Method Not Allowed", &extra)
                    }
                };
                write_all(&mut stream, &response).await?;
                return Ok(None);
            }
            Parsed::Upgrade(request) => request,
        };

        // The client's first offer, which is the policy the hyper upgrader had:
        // it echoed the first comma separated token of `Sec-WebSocket-Protocol`.
        // `protocols` is that list already split and trimmed.
        let selected = request.protocols.first().cloned();
        let headers = Headers::from_request(config, &request);
        let extra = headers.ok_extra();
        let borrowed = borrow(&extra);
        write_all(
            &mut stream,
            &build_response(&request.key, selected.as_deref(), &borrowed),
        )
        .await?;

        debug!(
            ws_server = true,
            ?addr,
            path = %request.path,
            protocol = ?selected,
            "upgrade accepted, 101 sent"
        );

        // `rest` is whatever arrived past the head. A client that puts its first
        // frame in the same segment as its request is ordinary, and those bytes
        // are already out of the socket, so dropping them would lose the first
        // message of every fast client.
        let connection = Connection::with_buffered(stream, Role::Server, Limits::default(), rest);

        Ok(Some(UpgradeEvent {
            stream: Box::new(NagoWsStream { inner: connection }),
            // The whole offered list, not the selection. This is what reaches
            // `AuthController::auth`, which is where the fleet carries its bearer
            // token, and the hyper upgrader passed the raw header value.
            protocol: request.protocols.join(", "),
        }))
    }
}

/// What came back from trying to read a request head.
enum HeadOutcome {
    /// The head, and whatever was read past it.
    Head(Vec<u8>, BytesMut),
    /// The peer hung up before the head was complete. There is nothing to
    /// answer: the other end is already gone.
    Eof,
    /// The head passed the size limit without terminating, which is a denial of
    /// service guard rather than a protocol rule — a peer that never sends the
    /// blank line would otherwise grow this buffer forever.
    TooLarge,
}

/// Read until the head is complete, keeping anything read past it.
///
/// nago-wss has this loop too, as a private `read_head` inside `upgrade::accept`.
/// It is duplicated rather than shared because the alternative is either calling
/// `accept` — which cannot express this server's CORS policy, see the module note
/// — or a `pub use` that nago-wss has not made. Making that function public is the
/// one change that would let this go away.
async fn read_head(stream: &mut BoxedStream) -> Result<HeadOutcome> {
    let mut buffer = BytesMut::with_capacity(2 * 1024);
    let mut chunk = [0u8; 2 * 1024];
    loop {
        if let Some(end) = head_end(&buffer) {
            let rest = buffer.split_off(end);
            return Ok(HeadOutcome::Head(buffer.to_vec(), rest));
        }
        if buffer.len() > DEFAULT_MAX_HEAD {
            return Ok(HeadOutcome::TooLarge);
        }

        let read = nagoya::io::Stream::read(stream, &mut chunk)
            .await
            .map_err(|err| eyre!("reading the request head: {err}"))?;
        if read == 0 {
            return Ok(HeadOutcome::Eof);
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Write a whole response, naming what failed.
async fn write_all(stream: &mut BoxedStream, bytes: &[u8]) -> Result<()> {
    nagoya::io::Stream::write_all(stream, bytes)
        .await
        .map_err(|err| eyre!("writing the handshake response: {err}"))
}

/// Build a plain HTTP response with no body.
///
/// nago-wss builds the 101 and the 4xx refusals; it has no opinion about a 200,
/// because a 200 is only reachable through [`Parsed::Plain`] and that is the case
/// it explicitly hands back for the caller to answer. `Connection: close` is on
/// every one of these: this endpoint speaks WebSocket or it speaks once, and
/// keeping the socket open for a second request would mean implementing HTTP/1.1
/// persistence for no caller that wants it.
fn plain_response(status: &str, extra: &[(String, String)]) -> Vec<u8> {
    let mut out = String::with_capacity(160);
    out.push_str("HTTP/1.1 ");
    out.push_str(status);
    out.push_str("\r\nConnection: close\r\nContent-Length: 0\r\n");
    for (name, value) in extra {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
}

/// Borrow an owned header list as the slice nago-wss's builders take.
fn borrow(extra: &[(String, String)]) -> Vec<(&str, &str)> {
    extra
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// The response headers this deployment adds, decided from the request.
///
/// Held as owned strings because the CORS set echoes values out of the request
/// and the request is dropped before the response goes out on one path.
struct Headers {
    /// `Server`, always sent.
    server: String,
    /// The CORS headers, already resolved against the policy. Empty when the
    /// policy refused the origin, which is how a disallowed origin is reported:
    /// the browser sees a response with no `Allow-Origin` and blocks the read.
    cors: Vec<(String, String)>,
}

impl Headers {
    fn from_request(config: &WsServerConfig, request: &Request) -> Self {
        Self::new(
            config,
            header(request, "origin"),
            header(request, "access-control-request-headers"),
        )
    }

    fn new(config: &WsServerConfig, origin: Option<&str>, requested_headers: Option<&str>) -> Self {
        // `WS_SERVER_NAME` overrides the configured name at compile time, which
        // is how a deployment stamps its own identity without threading it
        // through config. Built per response rather than cached in a `OnceLock`:
        // the cache keyed on nothing, so the first config to reach it decided the
        // `Server` header for every config in the process.
        let app = option_env!("WS_SERVER_NAME").unwrap_or(&config.server_name);
        let server = format!("{} endpointlibs/{}", app, env!("CARGO_PKG_VERSION"));

        let allow_headers = requested_headers
            .filter(|value| is_safe_header_value(value))
            .unwrap_or(DEFAULT_ALLOW_HEADERS)
            .to_string();

        let mut cors = Vec::new();
        match config.allow_cors_urls.as_ref() {
            // No allowlist means open. Credentials cannot be allowed alongside a
            // wildcard origin — a browser refuses that combination outright — so
            // `Allow-Credentials` appears only in the branch below.
            None => {
                cors.push(("Access-Control-Allow-Origin".into(), "*".into()));
                cors.push(("Timing-Allow-Origin".into(), "*".into()));
                cors.push((
                    "Access-Control-Allow-Methods".into(),
                    ALLOWED_METHODS.into(),
                ));
                cors.push(("Access-Control-Allow-Headers".into(), allow_headers));
                cors.push(("Access-Control-Max-Age".into(), "86400".into()));
            }
            // An allowlist means the answer depends on who asked, so the origin
            // is echoed rather than wildcarded and `Vary: Origin` says so to any
            // cache in between. An origin that is not on the list gets no CORS
            // headers at all.
            Some(domains) => {
                let Some(origin) = origin else {
                    return Self { server, cors };
                };
                if !domains.iter().any(|domain| domain == origin) {
                    return Self { server, cors };
                }
                cors.push(("Access-Control-Allow-Origin".into(), origin.to_string()));
                cors.push(("Timing-Allow-Origin".into(), origin.to_string()));
                cors.push(("Vary".into(), "Origin".into()));
                cors.push(("Access-Control-Allow-Credentials".into(), "true".into()));
                cors.push((
                    "Access-Control-Allow-Methods".into(),
                    ALLOWED_METHODS.into(),
                ));
                cors.push(("Access-Control-Allow-Headers".into(), allow_headers));
                cors.push(("Access-Control-Max-Age".into(), "86400".into()));
            }
        }
        Self { server, cors }
    }

    /// The headers for a 101 or a 200.
    fn ok_extra(&self) -> Vec<(String, String)> {
        let mut extra = Vec::with_capacity(self.cors.len() + 2);
        extra.push(("Server".to_string(), self.server.clone()));
        extra.push(("X-Content-Type-Options".to_string(), "nosniff".to_string()));
        extra.extend(self.cors.iter().cloned());
        extra
    }

    /// The headers for a 4xx or 5xx.
    ///
    /// `no-store` is the difference: a refusal is about this request, and an
    /// intermediary that cached it would serve one client's 405 to another. The
    /// CORS set is still here so a browser is allowed to read the status rather
    /// than reporting an opaque network error it cannot explain.
    fn error_extra(&self) -> Vec<(String, String)> {
        let mut extra = self.ok_extra();
        extra.push(("Cache-Control".to_string(), "no-store".to_string()));
        extra
    }
}

/// Look a header up the way the parse stores them: case insensitively, first
/// match wins.
///
/// Duplicates are kept by the parse on purpose — which of two `Origin` headers to
/// believe is a policy question, and answering it inside the parse would hide the
/// attack that asks it. Taking the first is this server's answer to it.
fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(had, _)| had.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Whether a value read out of the request is safe to write back out.
///
/// The parse splits on `\n` and trims `\r`, so a header value cannot already
/// carry a line break and response splitting is not reachable. This is the belt
/// to that brace: the old upgrader got the same guarantee from `HeaderValue`'s
/// parse and dropped anything that failed it, and losing that check silently
/// while moving to a builder that validates nothing is how it comes back.
fn is_safe_header_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..0x7F).contains(&byte))
}

/// The session loop's view of an upgraded connection.
struct NagoWsStream {
    inner: Connection<BoxedStream>,
}

#[async_trait(?Send)]
impl MessageStream for NagoWsStream {
    async fn send(&mut self, msg: Message) -> Result<(), StreamError> {
        // Backend edge: the canonical `WireMessage` becomes nago-wss's `Message`
        // here and nowhere else.
        self.inner.write(msg.into()).await.map_err(map_err)
    }

    async fn recv(&mut self) -> Option<Result<Message, StreamError>> {
        match self.inner.read().await {
            Ok(Some(message)) => Some(Ok(message.into())),
            // A close handshake completed. Not an error, and not a message.
            Ok(None) => None,
            Err(error) => Some(Err(map_err(error))),
        }
    }
}

fn map_err(error: ConnError) -> StreamError {
    match error {
        // The peer vanished without a closing handshake. The session loop treats
        // this the same as a clean close; it has nowhere to report it to.
        ConnError::UnexpectedEof => StreamError::Closed,
        ConnError::Frame(_) | ConnError::Protocol(_) | ConnError::MaskingViolation => {
            StreamError::Protocol(error.to_string())
        }
        other => StreamError::Other(eyre!(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A request with the headers a test wants, and nothing else interpreted.
    fn request(headers: &[(&str, &str)]) -> Request {
        Request {
            method: Method::Get,
            path: "/".to_string(),
            key: Vec::new(),
            protocols: Vec::new(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        }
    }

    fn config(allow: Option<Vec<String>>) -> WsServerConfig {
        WsServerConfig {
            allow_cors_urls: Arc::new(allow),
            ..Default::default()
        }
    }

    fn value<'a>(extra: &'a [(String, String)], name: &str) -> Option<&'a str> {
        extra
            .iter()
            .find(|(had, _)| had.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn no_allowlist_means_wildcard_and_no_credentials() {
        let extra = Headers::from_request(&config(None), &request(&[])).ok_extra();
        assert_eq!(value(&extra, "access-control-allow-origin"), Some("*"));
        assert_eq!(value(&extra, "timing-allow-origin"), Some("*"));
        // A browser refuses credentials alongside a wildcard origin outright, so
        // sending both would break exactly the cross-origin client it looks like
        // it is helping.
        assert_eq!(value(&extra, "access-control-allow-credentials"), None);
        assert_eq!(
            value(&extra, "access-control-allow-methods"),
            Some(ALLOWED_METHODS)
        );
    }

    #[test]
    fn an_allowlisted_origin_is_echoed_and_varied_on() {
        let config = config(Some(vec!["https://app.example.com".to_string()]));
        let extra =
            Headers::from_request(&config, &request(&[("Origin", "https://app.example.com")]))
                .ok_extra();
        assert_eq!(
            value(&extra, "access-control-allow-origin"),
            Some("https://app.example.com")
        );
        // Without this a shared cache can serve one origin's response to another.
        assert_eq!(value(&extra, "vary"), Some("Origin"));
        assert_eq!(
            value(&extra, "access-control-allow-credentials"),
            Some("true")
        );
    }

    #[test]
    fn an_origin_off_the_allowlist_gets_no_cors_headers() {
        let config = config(Some(vec!["https://app.example.com".to_string()]));
        for origin in [None, Some("https://evil.example.com")] {
            let headers: Vec<(&str, &str)> = origin
                .map(|value| vec![("Origin", value)])
                .unwrap_or_default();
            let extra = Headers::from_request(&config, &request(&headers)).ok_extra();
            assert_eq!(
                value(&extra, "access-control-allow-origin"),
                None,
                "a refused origin was echoed: {origin:?}"
            );
            // The `Server` header is not part of the CORS decision and is still
            // there, which is what distinguishes "refused" from "built nothing".
            assert!(value(&extra, "server").is_some());
        }
    }

    #[test]
    fn a_preflight_gets_the_headers_it_asked_for() {
        let asked = "authorization, x-trace";
        let extra = Headers::from_request(
            &config(None),
            &request(&[("Access-Control-Request-Headers", asked)]),
        )
        .ok_extra();
        assert_eq!(value(&extra, "access-control-allow-headers"), Some(asked));

        // With no preflight ask, the default list, which has to name the
        // `Sec-WebSocket-*` headers a cross-origin open carries.
        let extra = Headers::from_request(&config(None), &request(&[])).ok_extra();
        assert_eq!(
            value(&extra, "access-control-allow-headers"),
            Some(DEFAULT_ALLOW_HEADERS)
        );
    }

    #[test]
    fn a_header_value_with_control_bytes_is_not_echoed() {
        // The parse cannot hand over a value containing CR or LF, so response
        // splitting is already unreachable. This is the belt to that brace: the
        // old upgrader got the same guarantee from `HeaderValue`'s parse, and
        // losing it silently while moving to a builder that validates nothing is
        // how it comes back.
        let extra = Headers::from_request(
            &config(None),
            &request(&[("Access-Control-Request-Headers", "authorization\u{7f}")]),
        )
        .ok_extra();
        assert_eq!(
            value(&extra, "access-control-allow-headers"),
            Some(DEFAULT_ALLOW_HEADERS)
        );
    }

    #[test]
    fn only_a_failure_is_marked_uncacheable() {
        let headers = Headers::from_request(&config(None), &request(&[]));
        assert_eq!(value(&headers.ok_extra(), "cache-control"), None);
        assert_eq!(
            value(&headers.error_extra(), "cache-control"),
            Some("no-store")
        );
    }

    #[test]
    fn the_first_of_two_origins_decides() {
        // The parse keeps duplicates on purpose: which one to believe is a policy
        // question, and answering it inside the parse would hide the attack that
        // asks it. Taking the first is this server's answer, and it has to be the
        // same one every time or the policy is a coin flip.
        let config = config(Some(vec!["https://app.example.com".to_string()]));
        let extra = Headers::from_request(
            &config,
            &request(&[
                ("Origin", "https://evil.example.com"),
                ("Origin", "https://app.example.com"),
            ]),
        )
        .ok_extra();
        assert_eq!(value(&extra, "access-control-allow-origin"), None);
    }

    #[test]
    fn a_plain_response_ends_the_head_exactly_once() {
        // An extra or missing blank line here is not a subtle bug: the client
        // either hangs waiting for a head that never ends or reads the next
        // response's status line as a body.
        let response = plain_response("200 OK", &[("Server".into(), "test".into())]);
        let text = String::from_utf8(response).expect("ASCII response");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("\r\nServer: test\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
        assert_eq!(text.matches("\r\n\r\n").count(), 1);
        // No body is coming, and a client that is not told so waits for one.
        assert!(text.contains("\r\nContent-Length: 0\r\n"));
    }
}
