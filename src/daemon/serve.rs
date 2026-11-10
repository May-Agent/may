//! Serves the API router on the daemon's listeners.
//!
//! skyzen's own launcher binds exactly one TCP address on a runtime it
//! owns; the daemon needs the same router on a unix socket and on any
//! number of TCP addresses inside its runtime, so it drives hyper directly
//! and hands every request to [`Router::go`].

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::TryStreamExt as _;
use http_body_util::{BodyDataStream, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper_util::rt::TokioIo;
use skyzen::BodyError;
use skyzen::extract::PeerAddr;
use skyzen::routing::Router;

use super::auth::TailnetLogin;
use super::session_ids::ConnectionIds;
use super::state::DaemonState;

type ResponseBody =
    StreamBody<futures_util::stream::MapOk<skyzen::Body, fn(bytes::Bytes) -> Frame<bytes::Bytes>>>;

fn into_frame(bytes: bytes::Bytes) -> Frame<bytes::Bytes> {
    Frame::data(bytes)
}

async fn handle(
    router: Arc<Router>,
    peer: Option<SocketAddr>,
    tailnet: Option<TailnetLogin>,
    ids: ConnectionIds,
    request: hyper::Request<Incoming>,
) -> Result<hyper::Response<ResponseBody>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let mut request: skyzen::Request = request.map(|body| {
        skyzen::Body::from_stream(
            BodyDataStream::new(body).map_err(|error| BodyError::Other(Box::new(error))),
        )
    });
    if let Some(peer) = peer {
        request.extensions_mut().insert(PeerAddr(peer));
    }
    if let Some(login) = tailnet {
        request.extensions_mut().insert(login);
    }
    request.extensions_mut().insert(ids);
    // WebSocket upgrade extraction needs the runtime executor skyzen's own
    // launcher inserts per request.
    request
        .extensions_mut()
        .insert(std::sync::Arc::new(executor_core::AnyExecutor::new(
            executor_core::tokio::TokioGlobal,
        )));
    let response = answer(&router, request, &method, path.as_str()).await;
    Ok(response.map(|body| StreamBody::new(body.map_ok(into_frame as fn(_) -> _))))
}

/// The `500` a panicking handler is answered with.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct HandlerPanicked(crate::panic_boundary::Panicked);

impl skyzen::HttpError for HandlerPanicked {
    fn status(&self) -> skyzen::StatusCode {
        skyzen::StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// The router's answer to `request`; a handler that panics is logged with
/// its message and answered with a `500`, so its connection and the others
/// keep being served.
async fn answer(
    router: &Router,
    request: skyzen::Request,
    method: &hyper::Method,
    path: &str,
) -> skyzen::Response {
    let error = match crate::panic_boundary::catch(router.go(request)).await {
        Ok(Ok(response)) => return response,
        Ok(Err(error)) => error,
        Err(panicked) => {
            tracing::error!(%method, path, panic = %panicked, "a request handler panicked");
            Box::new(HandlerPanicked(panicked))
        }
    };
    skyzen_core::log_endpoint_error(&*error, method, path);
    skyzen_core::error_response(&*error)
}

/// How a TCP listener attributes its peers.
enum PeerIdentity {
    /// No peer attribution: the listener's router decides alone.
    None,
    /// Resolve each connection's peer through `tailscale whois`, using the
    /// binary configured in `state` when the connection arrives.
    TailnetWhois(Arc<DaemonState>),
}

fn serve_connection<IO>(
    io: IO,
    router: Arc<Router>,
    peer: Option<SocketAddr>,
    identity: PeerIdentity,
) where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        // A connection's peer cannot change: resolve once, in this task so
        // the accept loop never waits on the CLI.
        let tailnet = match (identity, peer) {
            (PeerIdentity::TailnetWhois(state), Some(peer)) => {
                super::remote_access::resolve_tailnet_login(&state, peer.ip()).await
            }
            _ => None,
        };
        // The id leases this connection's requests take; they go when the
        // connection closes and the service holding them drops.
        let ids = ConnectionIds::default();
        let service = hyper::service::service_fn(move |request| {
            handle(router.clone(), peer, tailnet.clone(), ids.clone(), request)
        });
        if let Err(error) = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(io), service)
            .with_upgrades()
            .await
        {
            tracing::debug!(%error, "connection closed with an error");
        }
    });
}

/// Accepts TCP connections on `listener` until it fails.
pub async fn serve_tcp(listener: tokio::net::TcpListener, router: Arc<Router>) {
    accept_tcp(listener, router, || PeerIdentity::None).await;
}

/// Accepts TCP connections on a direct tailnet bind, resolving every peer's
/// login through `tailscale whois` before its first request is served.
pub(super) async fn serve_tcp_tailnet(
    listener: tokio::net::TcpListener,
    router: Arc<Router>,
    state: Arc<DaemonState>,
) {
    accept_tcp(listener, router, move || {
        PeerIdentity::TailnetWhois(state.clone())
    })
    .await;
}

async fn accept_tcp(
    listener: tokio::net::TcpListener,
    router: Arc<Router>,
    identity: impl Fn() -> PeerIdentity,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => serve_connection(stream, router.clone(), Some(peer), identity()),
            Err(error) => {
                tracing::warn!(%error, "TCP accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Accepts unix-socket connections on `listener` until it fails. Requests
/// on the socket carry no peer address; the socket's file mode is the
/// access control.
pub async fn serve_unix(listener: tokio::net::UnixListener, router: Arc<Router>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => serve_connection(stream, router.clone(), None, PeerIdentity::None),
            Err(error) => {
                tracing::warn!(%error, "unix socket accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use skyzen::StatusCode;
    use skyzen::routing::{CreateRouteNode, Route};

    use super::answer;

    async fn boom() -> &'static str {
        panic!("test panic in a handler")
    }

    async fn ok() -> &'static str {
        "fine"
    }

    async fn get(router: &skyzen::routing::Router, path: &str) -> skyzen::Response {
        let request: skyzen::Request = hyper::Request::builder()
            .method("GET")
            .uri(path)
            .body(skyzen::Body::empty())
            .expect("request builds");
        answer(router, request, &hyper::Method::GET, path).await
    }

    #[tokio::test]
    async fn a_panicking_handler_is_answered_with_a_500_and_the_next_request_is_served() {
        let router = Route::new(("/boom".at(boom), "/ok".at(ok))).build();

        let failed = get(&router, "/boom").await;
        assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let served = get(&router, "/ok").await;
        assert_eq!(served.status(), StatusCode::OK);
    }
}
