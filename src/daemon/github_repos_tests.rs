//! `GET /api/github/repos` against a mocked GitHub REST API.

use std::convert::Infallible;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use skyzen::StatusCode;

use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use super::types::{ApiGithubRepo, ApiGithubRepos};
use crate::config::MayConfig;
use crate::issue::github::GitHubRest;
use crate::pr::PrTokenSource;

type Seen = Arc<Mutex<Vec<String>>>;

/// A loopback GitHub API serving `/user/repos` in `pages` pages of one
/// repository each, with `Link` headers; `with_last` decides whether the
/// first page reveals the page count.
async fn mock_api(pages: u32, with_last: bool, status: u16) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let seen_in = seen.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let base = format!("http://{addr}");
    let base_in = base.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept");
            let seen = seen_in.clone();
            let base = base_in.clone();
            let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                let seen = seen.clone();
                let base = base.clone();
                async move {
                    let uri = request.uri().to_string();
                    seen.lock().expect("seen").push(uri.clone());
                    let page: u32 = uri
                        .split(['?', '&'])
                        .find_map(|part| part.strip_prefix("page=")?.parse().ok())
                        .unwrap_or(1);
                    let mut response = Response::builder()
                        .status(status)
                        .header("content-type", "application/json");
                    let mut links = Vec::new();
                    let url = |n: u32| format!("{base}/user/repos?per_page=100&page={n}");
                    if page < pages {
                        links.push(format!("<{}>; rel=\"next\"", url(page + 1)));
                        if with_last {
                            links.push(format!("<{}>; rel=\"last\"", url(pages)));
                        }
                    }
                    if !links.is_empty() {
                        response = response.header("link", links.join(", "));
                    }
                    let body = if status == 200 {
                        serde_json::json!([{"full_name": format!("org/repo{page}")}])
                    } else {
                        serde_json::json!({"message": "nope"})
                    };
                    Ok::<_, Infallible>(
                        response
                            .body(http_body_util::Full::new(Bytes::from(body.to_string())))
                            .expect("response"),
                    )
                }
            });
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (base, seen)
}

async fn state(home: &Path, api_base: &str) -> Arc<DaemonState> {
    let mut state = DaemonState::for_test(home, MayConfig::default())
        .await
        .expect("daemon state");
    state.github_api = GitHubRest::with_api_base(PrTokenSource::Fixed("t".to_string()), api_base);
    Arc::new(state)
}

async fn fetch(state: &Arc<DaemonState>) -> skyzen::Response {
    let router = build_router(state.clone(), DaemonMode::Rest, None);
    let request = hyper::Request::builder()
        .method("GET")
        .uri("/api/github/repos")
        .body(skyzen::Body::empty())
        .expect("request");
    router.go(request).await.expect("route answers")
}

fn names(list: &ApiGithubRepos) -> Vec<(&str, bool)> {
    list.repos
        .iter()
        .map(|repo| (repo.full_name.as_str(), repo.recent))
        .collect()
}

async fn parse(response: skyzen::Response) -> ApiGithubRepos {
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("repo list")
}

#[tokio::test]
async fn recent_checkouts_come_first_and_the_account_pages_are_all_fetched() {
    let (api, seen) = mock_api(3, true, 200).await;
    let home = tempfile::tempdir().expect("temp dir");
    let state = state(home.path(), &api).await;

    let checkout = home.path().join("checkout");
    std::fs::create_dir_all(&checkout).expect("dir");
    for args in [
        vec!["init", "-q"],
        vec!["remote", "add", "origin", "git@github.com:Org/Repo2.git"],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&checkout)
                .args(&args)
                .status()
                .expect("git")
                .success()
        );
    }
    state
        .db()
        .touch_repo_usage(&checkout)
        .await
        .expect("usage recorded");

    let list = parse(fetch(&state).await).await;
    assert_eq!(
        names(&list),
        [
            ("Org/Repo2", true),
            ("org/repo1", false),
            ("org/repo3", false)
        ],
        "a recent repository leads and is not repeated"
    );
    assert_eq!(seen.lock().expect("seen").len(), 3, "every page fetched");
}

#[tokio::test]
async fn pages_are_followed_when_the_count_is_unknown() {
    let (api, seen) = mock_api(3, false, 200).await;
    let home = tempfile::tempdir().expect("temp dir");
    let state = state(home.path(), &api).await;

    let list = parse(fetch(&state).await).await;
    assert_eq!(
        list.repos,
        ["org/repo1", "org/repo2", "org/repo3"].map(|full_name| ApiGithubRepo {
            full_name: full_name.to_string(),
            recent: false,
        })
    );
    assert_eq!(seen.lock().expect("seen").len(), 3);
}

#[tokio::test]
async fn a_failing_token_source_is_a_typed_error() {
    let home = tempfile::tempdir().expect("temp dir");
    let mut state = DaemonState::for_test(home.path(), MayConfig::default())
        .await
        .expect("daemon state");
    state.github_api =
        GitHubRest::with_api_base(PrTokenSource::Unauthenticated, "http://127.0.0.1:9");
    let response = fetch(&Arc::new(state)).await;
    assert_eq!(response.status(), StatusCode::FAILED_DEPENDENCY);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("body")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["code"], "github_unauthenticated");
}
