//! The access policy of each listener, the outermost layer of the router it
//! serves, so no route or fallback runs before it.
//!
//! - The unix socket ([`SocketOwner`]): its file mode is the access control.
//! - A loopback TCP bind ([`LoopbackOnly`]): the machine's own user, reached
//!   through a loopback `Host` and, from a browser, a loopback `Origin`.
//! - Every other listener ([`RequireIdentity`]): each request must prove who
//!   it comes from. A request the accepted sources cannot attribute gets
//!   `401`, never a guess — the resolved [`Identity`] rides the request
//!   extensions so handlers and the request log record it. The network
//!   attaches that identity to whatever the user's browser sends, so an
//!   identified request whose `Origin` is not the origin it was addressed to
//!   gets `403`.
//! - A direct tailnet bind ([`DirectListener`]): [`RequireIdentity`] behind a
//!   `Host` check. The peer's identity says who is connecting, not which site
//!   the user's browser believes it is talking to, so a request is answered
//!   only when its `Host` names the node itself ([`NodeNames`]) — a
//!   DNS-rebinding page's name never does.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hyper::http::uri::Authority;
use skyzen::header::{HOST, ORIGIN};
use skyzen::middleware::{Middleware, Next};
use skyzen::{Error, Request, Response, StatusCode};

/// The unix socket's policy: only the user owning the socket file can connect
/// (mode 0600), so every request on it is admitted.
pub struct SocketOwner;

impl Middleware for SocketOwner {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        next.run(request).await
    }
}

/// A loopback TCP bind's policy. Any local process may connect, but so may a
/// web page the user visits: its requests are refused when they name a
/// non-loopback `Host` (a DNS-rebinding name) or carry an `Origin` that is not
/// a loopback origin (`null` included — any site can produce it from a
/// sandboxed frame). A request without `Origin` is no browser cross-origin
/// request (curl, the CLI, a same-origin `GET`) and passes on its `Host`.
///
/// Any loopback port is admitted in either header: the Vite dev server
/// proxies the web UI's requests from its own port, and a local process can
/// reach the listener without a browser anyway.
pub struct LoopbackOnly;

impl Middleware for LoopbackOnly {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        if !has_loopback_host(request) {
            return Err(
                Error::msg("the local listener answers only a loopback Host")
                    .set_status(StatusCode::FORBIDDEN),
            );
        }
        if !has_loopback_origin_or_none(request) {
            return Err(
                Error::msg("the local listener refuses requests from a foreign Origin")
                    .set_status(StatusCode::FORBIDDEN),
            );
        }
        next.run(request).await
    }
}

/// The authority the request was addressed to: the URI's (HTTP/2's
/// `:authority`, or an absolute-form target), else `Host`.
fn request_authority(request: &Request) -> Option<Authority> {
    if let Some(authority) = request.uri().authority() {
        return Some(authority.clone());
    }
    request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Authority>().ok())
}

/// The request's authority names a loopback host. A request naming no
/// authority is refused.
fn has_loopback_host(request: &Request) -> bool {
    request_authority(request).is_some_and(|authority| is_loopback_name(authority.host()))
}

/// No `Origin`, or an http(s) origin whose host is a loopback name.
fn has_loopback_origin_or_none(request: &Request) -> bool {
    let Some(origin) = request.headers().get(ORIGIN) else {
        return true;
    };
    origin
        .to_str()
        .ok()
        .and_then(|origin| url::Url::parse(origin).ok())
        .filter(|origin| matches!(origin.scheme(), "http" | "https"))
        .is_some_and(|origin| origin.host_str().is_some_and(is_loopback_name))
}

/// `localhost` or a `.localhost` name (RFC 6761), a 127/8 address, or `[::1]`
/// — names no DNS answer can point elsewhere. IPv6 comes bracketed, as in a
/// URL authority.
fn is_loopback_name(host: &str) -> bool {
    if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return v6.parse::<Ipv6Addr>().is_ok_and(|ip| ip.is_loopback());
    }
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return v4.is_loopback();
    }
    host.rsplit('.')
        .next()
        .is_some_and(|tld| tld.eq_ignore_ascii_case("localhost"))
}

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
    /// The bearer token of the daemon that fronts this cloud home, the one
    /// it installed here when it bootstrapped the machine.
    FrontingDaemon,
}

/// The scheme a remote request was addressed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// Plain HTTP.
    Http,
    /// HTTP over TLS.
    Https,
}

impl Scheme {
    const fn name(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    /// The port an authority without one implies (RFC 9110 §4.2.1, §4.2.2).
    const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
        }
    }
}

/// One way a remote request can prove an identity. Sources are tried in
/// order; the first to accept wins.
pub trait IdentitySource: Send + Sync {
    /// The request's identity, or `None` when this source does not trust it.
    fn authenticate(&self, request: &Request) -> Option<Identity>;

    /// The scheme the request was addressed with, as the path this source
    /// trusts reports it. With the request's authority it makes the origin
    /// the request was addressed to.
    fn scheme(&self, request: &Request) -> Scheme;
}

/// The `401` every unauthenticated remote request gets.
fn unauthorized() -> Error {
    Error::msg("the remote listener needs an identity").set_status(StatusCode::UNAUTHORIZED)
}

/// The `403` an identified request from another origin gets.
fn foreign_origin() -> Error {
    Error::msg("the remote listener refuses requests from a foreign Origin")
        .set_status(StatusCode::FORBIDDEN)
}

/// No `Origin`, or exactly the origin the request was addressed to:
/// `scheme` with the request's authority, whose missing port is the
/// scheme's default. `null`, an opaque origin, an unparsable one and any
/// origin when the request names no authority are foreign.
fn has_addressed_origin_or_none(request: &Request, scheme: Scheme) -> bool {
    let Some(origin) = request.headers().get(ORIGIN) else {
        return true;
    };
    let Some(addressed) = request_authority(request).and_then(|authority| {
        let host = url::Host::parse(authority.host()).ok()?;
        let port = authority
            .port_u16()
            .unwrap_or_else(|| scheme.default_port());
        Some(url::Origin::Tuple(scheme.name().to_owned(), host, port))
    }) else {
        return false;
    };
    origin
        .to_str()
        .ok()
        .and_then(|origin| url::Url::parse(origin).ok())
        .is_some_and(|origin| origin.origin() == addressed)
}

/// Rejects remote requests no registered source accepts, and those an
/// accepted source identifies but whose `Origin` is not the origin they were
/// addressed to. Publishes the accepted [`Identity`] into the request
/// extensions for handlers and logs.
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
                if !has_addressed_origin_or_none(request, source.scheme(request)) {
                    tracing::warn!(
                        login = %identity.login,
                        source = ?identity.source,
                        origin = ?request.headers().get(ORIGIN),
                        %method,
                        %path,
                        "remote request from a foreign Origin refused"
                    );
                    return Err(foreign_origin());
                }
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

/// The local address a connection arrived on, attached to every request of
/// that connection on a direct bind: the address the listener is bound to, or
/// for a wildcard bind the node address the peer connected to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalAddr(pub SocketAddr);

/// The names a request may address this node by: its tailnet addresses and
/// its `MagicDNS` name, as tailscaled reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeNames {
    addresses: HashSet<IpAddr>,
    /// Lowercase, without the trailing dot.
    dns_name: Option<String>,
}

impl NodeNames {
    /// The node's tailnet addresses (IPv4 and IPv6) and `MagicDNS` name; an
    /// empty name means the node has none.
    pub fn new(addresses: impl IntoIterator<Item = IpAddr>, dns_name: &str) -> Self {
        let dns_name = dns_name.trim_end_matches('.').to_ascii_lowercase();
        Self {
            addresses: addresses.into_iter().map(|ip| ip.to_canonical()).collect(),
            dns_name: (!dns_name.is_empty()).then_some(dns_name),
        }
    }

    /// `host` (a URL authority's host: IPv6 bracketed) is one of the node's
    /// names. A DNS name matches in any case.
    fn names(&self, host: &str) -> bool {
        host_ip(host).map_or_else(
            || {
                self.dns_name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(host.trim_end_matches('.')))
            },
            |ip| self.addresses.contains(&ip),
        )
    }
}

/// The IP address an authority host spells, if it spells one.
fn host_ip(host: &str) -> Option<IpAddr> {
    let ip = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(v6) => IpAddr::V6(v6.parse::<Ipv6Addr>().ok()?),
        None => IpAddr::V4(host.parse::<Ipv4Addr>().ok()?),
    };
    Some(ip.to_canonical())
}

/// Where a direct listener reads its node's names from.
pub trait NodeNamesSource: Send + Sync + 'static {
    /// The node's current names, or why they could not be read.
    fn read(&self) -> impl Future<Output = Result<NodeNames, String>> + Send;
}

/// The least time between two reads of the source. A request naming an
/// unknown host triggers a read, and anyone who can reach the listener can
/// send such requests, so reads are not unbounded. One a second caps the
/// `tailscale` processes such a flood starts, and a renamed node is still
/// answered within a second of its first request.
const NAMES_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// The node's names, shared by every connection of a direct listener and
/// re-read when a `Host` is not among them: the names change (a renamed node,
/// a new address) without a restart.
pub struct NodeNamesCache<S> {
    source: S,
    names: arc_swap::ArcSwap<NodeNames>,
    /// When the source was last read. Held across a read, so concurrent
    /// requests with an unknown host share one read.
    last_read: async_lock::Mutex<Option<Instant>>,
}

impl<S: NodeNamesSource> NodeNamesCache<S> {
    /// A cache with no names yet: the first request to a direct listener
    /// reads them, so a daemon without one never runs `tailscale`.
    pub fn new(source: S) -> Arc<Self> {
        Self::with_names(source, NodeNames::default())
    }

    /// A cache that starts with `names` and has not read its source.
    pub fn with_names(source: S, names: NodeNames) -> Arc<Self> {
        Arc::new(Self {
            source,
            names: arc_swap::ArcSwap::from_pointee(names),
            last_read: async_lock::Mutex::new(None),
        })
    }

    /// `host` names the node, re-reading its names once when it does not. A
    /// failed read keeps the previous names.
    async fn names(&self, host: &str) -> bool {
        if self.names.load().names(host) {
            return true;
        }
        let mut last_read = self.last_read.lock().await;
        // Another request may have refreshed while this one waited.
        if self.names.load().names(host) {
            return true;
        }
        if (*last_read).is_some_and(|at| at.elapsed() < NAMES_REFRESH_INTERVAL) {
            return false;
        }
        let read = self.source.read().await;
        *last_read = Some(Instant::now());
        drop(last_read);
        match read {
            Ok(names) => {
                self.names.store(Arc::new(names));
                self.names.load().names(host)
            }
            Err(error) => {
                tracing::warn!(%error, "the node's tailnet names could not be re-read; keeping the previous ones");
                false
            }
        }
    }
}

/// The `403` a request addressed to another host gets.
fn foreign_host() -> Error {
    Error::msg("the direct listener answers only requests addressed to this node")
        .set_status(StatusCode::FORBIDDEN)
}

/// A direct tailnet bind's policy: the request's `Host` must name the node
/// itself — one of its tailnet addresses, its `MagicDNS` name, or the address
/// the connection arrived on ([`LocalAddr`]) — before any identity is
/// resolved; then [`RequireIdentity`] decides.
pub struct DirectListener<S> {
    names: Arc<NodeNamesCache<S>>,
    identity: RequireIdentity,
}

impl<S: NodeNamesSource> DirectListener<S> {
    pub const fn new(names: Arc<NodeNamesCache<S>>, identity: RequireIdentity) -> Self {
        Self { names, identity }
    }
}

impl<S: NodeNamesSource> Middleware for DirectListener<S> {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let host = request_authority(request).map(|authority| authority.host().to_owned());
        let local = request
            .extensions()
            .get::<LocalAddr>()
            .map(|local| local.0.ip().to_canonical());
        let addressed = match host.as_deref() {
            None => false,
            Some(host) => {
                host_ip(host)
                    .zip(local)
                    .is_some_and(|(ip, local)| ip == local)
                    || self.names.names(host).await
            }
        };
        if !addressed {
            tracing::warn!(host = ?host, "direct request addressed to another host refused");
            return Err(foreign_host());
        }
        self.identity.handle(request, next).await
    }

    fn provisions(&self) -> Vec<std::any::TypeId> {
        self.identity.provisions()
    }
}

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

    /// A direct bind serves plain HTTP to the peer itself: no proxy stands in
    /// between, so a forwarding header is the peer's own and never read.
    fn scheme(&self, _request: &Request) -> Scheme {
        Scheme::Http
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

    /// `tailscale serve` terminates TLS and then sets `X-Forwarded-Proto:
    /// https`; its proxy (Go's `httputil.ReverseProxy` with `Rewrite`) strips
    /// the client's own forwarding headers first, so the header is the
    /// proxy's. Without it the request reached serve over plain HTTP. The
    /// proxy forwards the browser's `Host` unchanged.
    fn scheme(&self, request: &Request) -> Scheme {
        if request
            .headers()
            .get("X-Forwarded-Proto")
            .is_some_and(|proto| proto == "https")
        {
            Scheme::Https
        } else {
            Scheme::Http
        }
    }
}

/// The bearer token of the daemon that fronts this cloud home: requests it
/// relays through its forwarded connection carry `Authorization: Bearer`,
/// and only the token's SHA-256 is kept here. The forward lands on this
/// machine's loopback, so the token, not the address, is the proof.
#[derive(Clone)]
pub struct FrontingTokenIdentitySource {
    /// SHA-256 of the accepted token.
    digest: [u8; 32],
    /// The account the fronting daemon acts for, for logs.
    owner: String,
}

impl FrontingTokenIdentitySource {
    /// Accepts the token whose SHA-256 is `digest`, acting for `owner`.
    #[must_use]
    pub const fn new(digest: [u8; 32], owner: String) -> Self {
        Self { digest, owner }
    }
}

impl IdentitySource for FrontingTokenIdentitySource {
    fn authenticate(&self, request: &Request) -> Option<Identity> {
        use sha2::Digest as _;
        use subtle::ConstantTimeEq as _;

        let token = request
            .headers()
            .get(skyzen::header::AUTHORIZATION)?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?;
        let digest = sha2::Sha256::digest(token.as_bytes());
        bool::from(digest.as_slice().ct_eq(&self.digest)).then(|| Identity {
            login: self.owner.clone(),
            source: IdentitySourceKind::FrontingDaemon,
        })
    }

    /// The forward delivers plain HTTP to this machine's loopback.
    fn scheme(&self, _request: &Request) -> Scheme {
        Scheme::Http
    }
}

/// Whether the request was relayed by the daemon fronting this cloud home:
/// its identity came from [`FrontingTokenIdentitySource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fronted(pub bool);

impl skyzen::extract::Extractor for Fronted {
    type Error = std::convert::Infallible;

    fn extract(
        request: &mut Request,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send {
        std::future::ready(Ok(Self(
            request
                .extensions()
                .get::<Identity>()
                .is_some_and(|identity| identity.source == IdentitySourceKind::FrontingDaemon),
        )))
    }
}
