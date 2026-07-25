//! Peer identity for a connection, independent of transport.
//!
//! Before 2.0 a connection's peer was a bare `SocketAddr`, which only makes sense for
//! TCP/TLS. Local transports (Unix sockets, Windows named pipes, macOS XPC) identify
//! peers by process and by *code identity* — a codesign requirement, an executable
//! digest, a SID — and that has to survive all the way into the request context so
//! authorization and logging can see it.
//!
//! [`PeerIdentity`] is `#[non_exhaustive]`: new transports may add variants without a
//! breaking release.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Who is on the other end of a connection.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PeerIdentity {
    /// A TCP/TLS network peer — the pre-2.0 behaviour.
    Network(SocketAddr),
    /// A same-machine peer reached over a local transport.
    Local(LocalPeer),
    /// Transport could not determine a peer (in-process tests, exotic transports).
    Unknown,
}

/// A same-machine peer: OS-level process identity plus whatever code identity the
/// transport was able to verify.
#[derive(Debug, Clone)]
pub struct LocalPeer {
    pub pid: Option<u32>,
    /// Effective uid. Unix only — always `None` on Windows.
    pub uid: Option<u32>,
    pub attestation: Attestation,
}

/// Whether — and how — the transport verified the peer's *code* identity.
///
/// This is deliberately separate from `pid`/`uid`: knowing which process connected is
/// not the same as knowing it runs the binary you expect.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Attestation {
    /// The transport did not verify code identity. Treat the peer as untrusted.
    None,
    /// The OS or the transport verified code identity before the connection was
    /// handed over.
    Verified {
        /// How it was verified, e.g. `"xpc-codesign-requirement"`,
        /// `"pidfd-exe-sha256"`, `"pipe-sid-dacl"`.
        mechanism: &'static str,
        /// What matched: the requirement string, digest, or SID.
        subject: String,
    },
}

impl Attestation {
    /// True only for [`Attestation::Verified`].
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }
}

impl PeerIdentity {
    /// Best-effort IP for logging and for the pre-2.0 `ip_addr` field.
    ///
    /// Local and unknown peers report loopback — they have no IP, and loopback is
    /// both truthful about locality and safe for code that only formats this value.
    pub fn ip_addr(&self) -> IpAddr {
        match self {
            Self::Network(addr) => addr.ip(),
            Self::Local(_) | Self::Unknown => IpAddr::V4(Ipv4Addr::LOCALHOST),
        }
    }

    /// The socket address for network peers; a loopback placeholder otherwise.
    ///
    /// Provided for the deprecated `WsConnection::address` accessor. Prefer matching
    /// on the enum.
    pub fn socket_addr(&self) -> SocketAddr {
        match self {
            Self::Network(addr) => *addr,
            Self::Local(_) | Self::Unknown => SocketAddr::from(([127, 0, 0, 1], 0)),
        }
    }

    /// Attestation for local peers; `None` for network peers (TLS client certs are
    /// not modelled here).
    pub fn attestation(&self) -> Option<&Attestation> {
        match self {
            Self::Local(peer) => Some(&peer.attestation),
            _ => None,
        }
    }

    /// Compact one-line form for logs: `1.2.3.4:5678`, `local(pid=42,xpc-codesign-requirement)`.
    pub fn display(&self) -> String {
        match self {
            Self::Network(addr) => addr.to_string(),
            Self::Local(peer) => {
                let pid = peer
                    .pid
                    .map_or_else(|| "?".to_owned(), |pid| pid.to_string());
                match &peer.attestation {
                    Attestation::Verified { mechanism, .. } => {
                        format!("local(pid={pid},{mechanism})")
                    }
                    Attestation::None => format!("local(pid={pid},unattested)"),
                }
            }
            Self::Unknown => "unknown".to_owned(),
        }
    }
}

impl std::fmt::Display for PeerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

impl From<SocketAddr> for PeerIdentity {
    fn from(addr: SocketAddr) -> Self {
        Self::Network(addr)
    }
}

// ---------------------------------------------------------------------------
// Extensions
// ---------------------------------------------------------------------------

/// Object-safe `Any` that can also clone itself.
///
/// `RequestContext` derives `Clone` and consumers rely on that, so a plain
/// `Box<dyn Any>` map cannot live inside it. Requiring `Clone` on stored values (the
/// same trade `http::Extensions` makes) keeps the containing types cloneable.
trait CloneAny: Any + Send + Sync {
    fn clone_box(&self) -> Box<dyn CloneAny>;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

impl<T: Any + Send + Sync + Clone> CloneAny for T {
    fn clone_box(&self) -> Box<dyn CloneAny> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl Clone for Box<dyn CloneAny> {
    fn clone(&self) -> Self {
        // `(**self)` is load-bearing. `Box<dyn CloneAny>` satisfies the blanket impl's
        // bounds, so `self.clone_box()` would resolve to the box's own `clone_box`,
        // which calls `clone` — infinite recursion and a stack overflow. Deref to the
        // trait object so the inner value's `clone_box` runs instead.
        (**self).clone_box()
    }
}

/// A type-keyed map for attaching arbitrary data to a connection or request.
///
/// Modelled on `http::Extensions`, implemented here to avoid the dependency. Used to
/// carry attestation on a connection and verified mission-token claims on a request,
/// without either concept leaking into this crate's core types.
///
/// Stored values must be `Clone` so that the containing `RequestContext` stays
/// `Clone`.
#[derive(Default, Clone)]
pub struct Extensions {
    map: HashMap<TypeId, Box<dyn CloneAny>>,
}

impl Extensions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value, returning the previous one of the same type, if any.
    pub fn insert<T: Clone + Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        self.map
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|prev| prev.into_any().downcast().ok().map(|boxed| *boxed))
    }

    pub fn get<T: Clone + Send + Sync + 'static>(&self) -> Option<&T> {
        // `(**boxed)` is load-bearing: `Box<dyn CloneAny>` itself satisfies
        // `Any + Send + Sync + Clone`, so it matches the blanket impl below and
        // `boxed.as_any()` would resolve to the *box's* impl — yielding a `dyn Any`
        // whose concrete type is the box, which never downcasts to `T`.
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|boxed| (**boxed).as_any().downcast_ref())
    }

    pub fn get_mut<T: Clone + Send + Sync + 'static>(&mut self) -> Option<&mut T> {
        // See the note in `get` — the explicit deref is required here too.
        self.map
            .get_mut(&TypeId::of::<T>())
            .and_then(|boxed| (**boxed).as_any_mut().downcast_mut())
    }

    pub fn remove<T: Clone + Send + Sync + 'static>(&mut self) -> Option<T> {
        self.map
            .remove(&TypeId::of::<T>())
            .and_then(|boxed| boxed.into_any().downcast().ok().map(|b| *b))
    }

    pub fn contains<T: Clone + Send + Sync + 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Values are `dyn Any` and cannot be formatted; report the count so logs
        // still show whether anything was attached.
        f.debug_struct("Extensions")
            .field("len", &self.map.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_peer_reports_its_own_ip() {
        let addr: SocketAddr = "203.0.113.7:9000".parse().unwrap();
        let peer = PeerIdentity::Network(addr);
        assert_eq!(peer.ip_addr(), addr.ip());
        assert_eq!(peer.socket_addr(), addr);
        assert_eq!(peer.display(), "203.0.113.7:9000");
        assert!(peer.attestation().is_none());
    }

    #[test]
    fn local_peer_reports_loopback_and_keeps_attestation() {
        let peer = PeerIdentity::Local(LocalPeer {
            pid: Some(42),
            uid: Some(501),
            attestation: Attestation::Verified {
                mechanism: "xpc-codesign-requirement",
                subject: "identifier agentone".to_owned(),
            },
        });
        assert_eq!(peer.ip_addr(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(peer.attestation().unwrap().is_verified());
        assert_eq!(peer.display(), "local(pid=42,xpc-codesign-requirement)");
    }

    #[test]
    fn unattested_local_peer_is_visible_as_such() {
        let peer = PeerIdentity::Local(LocalPeer {
            pid: None,
            uid: None,
            attestation: Attestation::None,
        });
        assert!(!peer.attestation().unwrap().is_verified());
        assert_eq!(peer.display(), "local(pid=?,unattested)");
    }

    #[test]
    fn extensions_survive_a_clone() {
        #[derive(Debug, Clone, PartialEq)]
        struct Claims(&'static str);

        let mut ext = Extensions::new();
        ext.insert(Claims("mission-1"));
        // RequestContext derives Clone; extensions must come along intact.
        let copy = ext.clone();
        assert_eq!(copy.get::<Claims>(), Some(&Claims("mission-1")));
    }

    #[test]
    fn extensions_round_trip_by_type() {
        #[derive(Debug, Clone, PartialEq)]
        struct Claims(&'static str);

        let mut ext = Extensions::new();
        assert!(ext.is_empty());
        assert!(ext.get::<Claims>().is_none());

        assert!(ext.insert(Claims("mission-1")).is_none());
        assert_eq!(ext.get::<Claims>(), Some(&Claims("mission-1")));
        assert!(ext.contains::<Claims>());

        // Re-inserting returns the previous value rather than silently dropping it.
        let prev = ext.insert(Claims("mission-2"));
        assert_eq!(prev, Some(Claims("mission-1")));
        assert_eq!(ext.get::<Claims>(), Some(&Claims("mission-2")));

        assert_eq!(ext.remove::<Claims>(), Some(Claims("mission-2")));
        assert!(ext.is_empty());
    }
}
