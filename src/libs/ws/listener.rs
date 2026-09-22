use crate::libs::peer::PeerIdentity;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use eyre::{Result, bail};
use futures::FutureExt;
use futures::future::BoxFuture;
use nagoya::io::Stream;
use nagoya::reactor::{Addr, Handle, TcpStream};

/// Accepts raw byte streams for the TCP/TLS path.
///
/// **Breaking change in the tokio removal.** `Channel1` and `Channel2` were bounded
/// on `tokio::io::{AsyncRead, AsyncWrite}` and are now bounded on
/// [`nagoya::io::Stream`], which is a different shape rather than a renamed one: it
/// is an async-fn trait with `read(&mut self, &mut [u8]) -> Result<usize, _>` and
/// `write_all(&mut self, &[u8]) -> Result<(), _>`, so there is no `poll_read` to
/// forward and no `ReadBuf`. An implementor that wrapped a tokio stream re-bases on
/// nagoya's; an implementor that forwarded to an inner channel keeps forwarding.
///
/// Everything else is deliberately unchanged — the two associated types, `accept`,
/// `handshake`, their signatures and the `SocketAddr` — so the migration is
/// mechanical and the server side needs no rework.
///
/// Note that [`nagoya::io::Stream`] is an async-fn trait, so `dyn ConnectionListener`
/// was never possible and `Box<dyn Channel2>` is not possible either: the bound is
/// usable generically, not behind a vtable.
pub trait ConnectionListener: Send + Sync + Unpin {
    type Channel1: Stream + Send + Sync + Unpin + 'static;
    type Channel2: Stream + Send + Sync + Unpin + 'static;

    fn accept(&self) -> BoxFuture<'_, Result<(Self::Channel1, SocketAddr)>>;
    fn handshake(&self, channel: Self::Channel1) -> BoxFuture<'_, Result<Self::Channel2>>;
}

/// A TCP listener on one nagoya reactor.
///
/// **Breaking change.** `bind` was `async fn bind(SocketAddr)`; it is now a plain
/// `fn` that takes an [`Addr`] and the [`Handle`] of the reactor the listener and
/// every socket it accepts will live on. Both halves of that come from nagoya:
/// `TcpListener::bind` is synchronous down to the `listen(2)`, and a reactor
/// `Handle` has to exist before a descriptor can be registered, so there is no
/// longer anything to await and no longer an ambient runtime to guess the reactor
/// from.
///
/// The reactor is not incidental. A socket accepted here is registered with
/// `handle`'s reactor, which is the only reactor that will ever report its
/// readiness, so the connection has to be driven on the thread that polls that
/// reactor. That is what makes the accept loop, the connections and the shutdown
/// signal one thread's work in [`WebsocketServer::listen`](super::WebsocketServer).
pub struct TcpListener {
    listener: nagoya::reactor::TcpListener,
}
impl TcpListener {
    pub fn bind(addr: Addr, handle: &Handle) -> Result<Self> {
        let listener = nagoya::reactor::TcpListener::bind(addr, handle)?;
        Ok(Self { listener })
    }

    /// Bind the first address in `addrs` that the kernel accepts.
    ///
    /// [`nagoya::reactor::resolve`] returns *every* address a name has, and a host
    /// with both an A and an AAAA record is ordinary, so a single answer is the
    /// exception rather than the rule. Taking only the first is the dual-stack bug
    /// nagoya's resolver documentation names: on a machine where one family has no
    /// route, or where the v6 address is held by something else, the bind fails
    /// outright while a perfectly good address sits second in the list.
    ///
    /// One address is bound, not all of them. Binding every answer would mean one
    /// listening socket and one accept loop per family, and the configuration names
    /// a single address to serve; a server that wants both families asks for `::`
    /// and gets them from one socket. The chosen address is logged, because "which
    /// one did it pick" is otherwise unanswerable from outside.
    ///
    /// The error reported is the last failure, with the whole candidate list
    /// attached: unlike [`nagoya::reactor::connect_any`], every error here is about
    /// this machine, so there is no remote-versus-local distinction to preserve.
    pub fn bind_any(addrs: &[Addr], handle: &Handle) -> Result<Self> {
        let mut last = None;
        for addr in addrs {
            match Self::bind(*addr, handle) {
                Ok(listener) => {
                    tracing::debug!(
                        ws_server = true,
                        "Bound {:?} of {} resolved address(es)",
                        addr,
                        addrs.len()
                    );
                    return Ok(listener);
                }
                Err(err) => {
                    tracing::debug!(ws_server = true, "Could not bind {:?}: {}", addr, err);
                    last = Some(err);
                }
            }
        }
        match last {
            Some(err) => Err(err).map_err(|err| {
                err.wrap_err(format!("None of {addrs:?} could be bound; last failure"))
            }),
            None => bail!("no address to bind"),
        }
    }

    /// The address actually bound, which is how to learn the port when zero was asked for.
    pub fn local_addr(&self) -> Result<Addr> {
        Ok(self.listener.local_addr()?)
    }
}
impl ConnectionListener for TcpListener {
    type Channel1 = TcpStream;
    type Channel2 = TcpStream;

    fn accept(&self) -> BoxFuture<'_, Result<(Self::Channel1, SocketAddr)>> {
        async {
            let (stream, addr) = self.listener.accept().await?;
            Ok((stream, socket_addr(addr)?))
        }
        .boxed()
    }
    fn handshake(&self, channel: Self::Channel1) -> BoxFuture<'_, Result<Self::Channel2>> {
        async move { Ok(channel) }.boxed()
    }
}

/// Convert a nagoya [`Addr`] to a [`SocketAddr`] for [`ConnectionListener::accept`].
///
/// [`ConnectionListener`] keeps yielding `SocketAddr` rather than `Addr` because
/// that is what the rest of the crate speaks: [`PeerIdentity::Network`] holds one,
/// and widening it would push nagoya's address type into every consumer that only
/// ever logs the peer. The cost is this conversion, and one case that cannot be
/// represented.
///
/// That case is [`Addr::Path`]: a Unix-domain peer has no address at all, so there
/// is nothing to convert to. It is an error here rather than a stand-in, because
/// the local transports have their own seam — [`SessionListener`], which carries a
/// [`PeerIdentity`] and can say `Local` — and a Unix socket arriving on this one is
/// a miswiring rather than a peer to describe.
pub fn socket_addr(addr: Addr) -> Result<SocketAddr> {
    match addr {
        Addr::V4(octets, port) => Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port)),
        Addr::V6(octets, port) => Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port)),
        Addr::Path(_) => {
            bail!("a Unix-domain peer has no SocketAddr; serve it through SessionListener")
        }
    }
}

/// Convert a [`SocketAddr`] to a nagoya [`Addr`], for a caller that already has one.
///
/// The inverse of [`socket_addr`] and total, since every `SocketAddr` is one of the
/// two address families. `listen` does not use it — it resolves straight to `Addr`
/// — but a consumer holding a `SocketAddr` needs a way to reach [`TcpListener::bind`]
/// without taking nagoya's address type apart by hand.
#[must_use]
pub fn nagoya_addr(addr: SocketAddr) -> Addr {
    match addr {
        SocketAddr::V4(v4) => Addr::V4(v4.ip().octets(), v4.port()),
        SocketAddr::V6(v6) => Addr::V6(v6.ip().octets(), v6.port()),
    }
}

/// Accepts already-framed connections for [`WebsocketServer::serve_with`].
///
/// This is the seam a platform-transport crate implements: a Unix socket listener, a
/// Windows named-pipe server, or an XPC mach-service listener each yield a
/// [`MessageStream`] plus the [`PeerIdentity`] they were able to establish — including
/// any code-signature attestation, which is the whole point of the local transports.
///
/// Distinct from [`ConnectionListener`], which yields *raw byte streams* for the
/// TCP/TLS path and knows nothing about messages or peers.
#[async_trait::async_trait]
pub trait SessionListener: Send + Sync + 'static {
    /// Wait for the next peer.
    ///
    /// Returning `Err` stops `serve_with`, so implementations should handle
    /// per-connection failures internally and only surface errors that make the
    /// listener itself unusable.
    async fn accept(&self)
    -> eyre::Result<(Box<dyn crate::libs::ws::MessageStream>, PeerIdentity)>;
}
