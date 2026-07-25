//! Per-request and per-connection interception.
//!
//! STOLEN SHAPE (tarpc `request_hook`): a `Before`/`After` pair around dispatch.
//!
//! This is the seam where policy that is *not* endpoint-specific plugs in — mission
//! token verification, quota enforcement, audit logging — without the transport layer
//! or the handlers knowing it exists. Hooks see every request on both the legacy
//! `{method, seq, params}` path and the MCP `tools/call` path.
//!
//! # Ordering
//!
//! ```text
//! connect ──► OnConnect ──► auth ──► [per request] roles ──► BeforeRequest ──► handler ──► AfterRequest
//! ```
//!
//! `BeforeRequest` runs *after* the endpoint's role check, so a hook can assume the
//! caller was allowed to reach this endpoint at all and concern itself only with the
//! finer-grained question. Hooks run in registration order and the first error
//! short-circuits: the handler never runs and the error goes back in whichever
//! envelope the caller used.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::libs::peer::{Extensions, PeerIdentity};
use crate::libs::toolbox::{CustomError, RequestContext};
use crate::model::EndpointSchema;

/// How a request finished. Passed to [`AfterRequest`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RequestOutcome {
    Ok,
    /// The handler (or a `BeforeRequest` hook) returned a public error.
    PublicErr {
        code: u32,
    },
    /// The handler failed internally; the client saw a generic error.
    InternalErr,
}

/// Runs before a request is dispatched; may reject it.
#[async_trait(?Send)]
pub trait BeforeRequest: Send + Sync {
    /// Returning `Err` skips the handler entirely and sends the error to the client.
    ///
    /// `ctx` is mutable so a hook can attach verified claims to
    /// [`RequestContext::extensions`] for the handler to read.
    async fn before(
        &self,
        ctx: &mut RequestContext,
        endpoint: &EndpointSchema,
        params: &Value,
    ) -> Result<(), CustomError>;
}

/// Runs after a request completes, for observation only.
#[async_trait(?Send)]
pub trait AfterRequest: Send + Sync {
    async fn after(
        &self,
        ctx: &RequestContext,
        endpoint: &EndpointSchema,
        outcome: &RequestOutcome,
    );
}

/// Runs once per connection, after auth, before any message is processed.
///
/// This is where a peer that failed attestation gets refused — once, rather than in
/// every [`BeforeRequest`]. Note that on macOS XPC a peer failing the code-signing
/// requirement never reaches Rust at all (libxpc drops the check-in), so this hook
/// sees only peers the transport was willing to hand over.
#[async_trait(?Send)]
pub trait OnConnect: Send + Sync {
    /// Returning `Err` refuses the connection; no messages are exchanged.
    ///
    /// `ext` is the connection-scoped [`Extensions`], so a hook can record what it
    /// verified for later requests to consult.
    async fn on_connect(
        &self,
        peer: &PeerIdentity,
        ext: &mut Extensions,
    ) -> Result<(), CustomError>;
}

/// The registered hooks, snapshotted into each spawned dispatch task.
#[derive(Clone, Default)]
pub struct Hooks {
    pub(crate) before: Vec<Arc<dyn BeforeRequest>>,
    pub(crate) after: Vec<Arc<dyn AfterRequest>>,
    pub(crate) on_connect: Vec<Arc<dyn OnConnect>>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.before.is_empty() && self.after.is_empty() && self.on_connect.is_empty()
    }

    /// Run every `BeforeRequest` in registration order, stopping at the first error.
    pub(crate) async fn run_before(
        &self,
        ctx: &mut RequestContext,
        endpoint: &EndpointSchema,
        params: &Value,
    ) -> Result<(), CustomError> {
        for hook in &self.before {
            hook.before(ctx, endpoint, params).await?;
        }
        Ok(())
    }

    /// Run every `AfterRequest`. Observers cannot fail the request, so errors are
    /// not representable here.
    pub(crate) async fn run_after(
        &self,
        ctx: &RequestContext,
        endpoint: &EndpointSchema,
        outcome: &RequestOutcome,
    ) {
        for hook in &self.after {
            hook.after(ctx, endpoint, outcome).await;
        }
    }

    pub(crate) async fn run_on_connect(
        &self,
        peer: &PeerIdentity,
        ext: &mut Extensions,
    ) -> Result<(), CustomError> {
        for hook in &self.on_connect {
            hook.on_connect(peer, ext).await?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("before", &self.before.len())
            .field("after", &self.after.len())
            .field("on_connect", &self.on_connect.len())
            .finish()
    }
}
