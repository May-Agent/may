//! Identity on the remote listener: every remote request must prove who it
//! comes from; the local loopback listener keeps trusting the machine's own
//! user. A request the accepted sources cannot attribute gets `401`, never a
//! guess — the resolved [`Identity`] rides the request extensions so handlers
//! and the request log record it.

use std::collections::HashSet;
use std::sync::Arc;

use skyzen::middleware::{Middleware, Next};
use skyzen::{Error, Request, Response, StatusCode};

/// Who a remote request belongs to and which proof brought it in.
#[derive(Debug, Clone)]
pub struct Identity {
    /// The login the source proved, as it appears in logs and audit trails.
    pub login: String,
    /// Which identity proof accepted the request.
    pub source: IdentitySourceKind,
}

/// The identity proof that accepted a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySourceKind {
    /// `tailscale serve`'s `Tailscale-User-Login` header on the tailnet
    /// listener — only that proxy may attach it.
    TailscaleServe,
    /// The peer address of a direct tailnet bind, resolved to a user login
    /// through tailscaled's `WhoIs`.
    TailnetWhois,
}

/// One way a remote request can prove an identity. Sources are tried in
/// order; the first to accept wins.
pub trait IdentitySource: Send + Sync {
    /// The request's identity, or `None` when this source does not trust it.
    fn authenticate(&self, request: &Request) -> Option<Identity>;
}

/// The `401` every unauthenticated remote request gets.
fn unauthorized() -> Error {
    Error::msg("the remote listener needs an identity").set_status(StatusCode::UNAUTHORIZED)
}

/// Rejects remote requests no registered source accepts, and publishes the
/// accepted [`Identity`] into the request extensions for handlers and logs.
pub struct RequireIdentity {
    sources: Vec<Box<dyn IdentitySource>>,
}

impl RequireIdentity {
    /// Sources tried in order for each request; an empty list accepts nobody.
    pub fn new(sources: Vec<Box<dyn IdentitySource>>) -> Self {
        Self { sources }
    }
}

impl Middleware for RequireIdentity {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        for source in &self.sources {
            if let Some(identity) = source.authenticate(request) {
                let method = request.method().clone();
                let path = request.uri().path().to_owned();
                tracing::info!(
                    login = %identity.login,
                    source = ?identity.source,
                    %method,
                    %path,
                    "remote request"
                );
                request.extensions_mut().insert(identity);
                return next.run(request).await;
            }
        }
        Err(unauthorized())
    }

    fn provisions(&self) -> Vec<std::any::TypeId> {
        vec![std::any::TypeId::of::<Identity>()]
    }
}

/// `Tailscale-User-Login`, accepted only when it is a listed login and the
/// request did not cross Tailscale Funnel, which is never trusted. The
/// allow-list swaps out whole so the remote-access actor can refresh it when
/// settings change or the node owner resolves.
#[derive(Clone)]
pub struct TailscaleServeIdentitySource {
    /// Swapped whole when the remote-access actor refreshes the allow-list.
    allowed: Arc<arc_swap::ArcSwap<HashSet<String>>>,
}

impl TailscaleServeIdentitySource {
    pub fn new(allowed: HashSet<String>) -> Self {
        Self {
            allowed: Arc::new(arc_swap::ArcSwap::from_pointee(allowed)),
        }
    }

    /// Replace the accepted logins (a changed allow-list or a resolved owner).
    pub fn set_allowed(&self, allowed: HashSet<String>) {
        self.allowed.store(Arc::new(allowed));
    }
}

impl TailscaleServeIdentitySource {
    /// The direct-bind source accepting the same live allow-list.
    pub fn whois_source(&self) -> TailnetWhoisIdentitySource {
        TailnetWhoisIdentitySource {
            allowed: self.allowed.clone(),
        }
    }
}

/// The login `tailscale whois` resolved for a connection's peer address,
/// attached to every request of that connection. Absent when the peer has no
/// user identity (a tagged node) or the resolution failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetLogin(pub String);

/// The connection's `WhoIs`-resolved login on a direct tailnet bind, accepted
/// only when it is in the allow-list shared with [`TailscaleServeIdentitySource`].
#[derive(Clone)]
pub struct TailnetWhoisIdentitySource {
    allowed: Arc<arc_swap::ArcSwap<HashSet<String>>>,
}

impl IdentitySource for TailnetWhoisIdentitySource {
    fn authenticate(&self, request: &Request) -> Option<Identity> {
        let TailnetLogin(login) = request.extensions().get::<TailnetLogin>()?;
        self.allowed
            .load()
            .contains(login.as_str())
            .then(|| Identity {
                login: login.clone(),
                source: IdentitySourceKind::TailnetWhois,
            })
    }
}

impl IdentitySource for TailscaleServeIdentitySource {
    fn authenticate(&self, request: &Request) -> Option<Identity> {
        // Funnel traffic is public and never trusted; the funnel header is what
        // marks it, not the tailnet identity.
        if request.headers().contains_key("Tailscale-Funnel-Request") {
            return None;
        }
        let login = request
            .headers()
            .get("Tailscale-User-Login")?
            .to_str()
            .ok()?
            .to_string();
        self.allowed
            .load()
            .contains(login.as_str())
            .then_some(Identity {
                login,
                source: IdentitySourceKind::TailscaleServe,
            })
    }
}
