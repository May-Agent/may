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
use skyzen::extract::PeerAddr;
use skyzen::routing::Router;
use skyzen::BodyError;

type ResponseBody =
    StreamBody<futures_util::stream::MapOk<skyzen::Body, fn(bytes::Bytes) -> Frame<bytes::Bytes>>>;

fn into_frame(bytes: bytes::Bytes) -> Frame<bytes::Bytes> {
    Frame::data(bytes)
}

async fn handle(
    router: Arc<Router>,
    peer: Option<SocketAddr>,
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
    let response = match router.go(request).await {
        Ok(response) => response,
        Err(error) => {
            skyzen_core::log_endpoint_error(&error, &method, path.as_str());
            skyzen_core::error_response(&error)
        }
    };
    Ok(response.map(|body| StreamBody::new(body.map_ok(into_frame as fn(_) -> _))))
}

fn serve_connection<IO>(io: IO, router: Arc<Router>, peer: Option<SocketAddr>)
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let service =
            hyper::service::service_fn(move |request| handle(router.clone(), peer, request));
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
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => serve_connection(stream, router.clone(), Some(peer)),
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
            Ok((stream, _)) => serve_connection(stream, router.clone(), None),
            Err(error) => {
                tracing::warn!(%error, "unix socket accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}
