//! `GET /api/sessions/{id}/repo-clones` answers from the host's registry.

use std::path::Path;
use std::process::Command;

use skyzen::{Request, StatusCode};

use super::auth::SocketOwner;
use super::server::{DaemonMode, build_router};
use super::state::DaemonState;
use crate::config::MayConfig;
use crate::repo_clone::{CloneView, SessionRepos};
use crate::session::{MayHomePaths, SessionStore};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn get(uri: &str) -> Request {
    hyper::Request::builder()
        .method("GET")
        .uri(uri)
        .body(skyzen::Body::empty())
        .expect("request builds")
}

#[tokio::test]
async fn the_endpoint_lists_the_registered_clones() {
    let home = tempfile::tempdir().expect("temp dir");
    let state = std::sync::Arc::new(
        DaemonState::for_test(home.path(), MayConfig::default())
            .await
            .expect("daemon state"),
    );
    let router = build_router(state.clone(), DaemonMode::Rest, SocketOwner);
    let db = state.db();
    db.create_session("s1", Some("S"), Some("m"))
        .await
        .expect("session row");

    let checkout = home.path().join("checkout");
    std::fs::create_dir_all(&checkout).expect("checkout dir");
    git(&checkout, &["init", "-q"]);
    git(&checkout, &["config", "user.email", "t@t"]);
    git(&checkout, &["config", "user.name", "t"]);
    std::fs::write(checkout.join("f"), "x").expect("file");
    git(&checkout, &["add", "."]);
    git(&checkout, &["commit", "-q", "-m", "init"]);

    let store = SessionStore::new(MayHomePaths::new(home.path().to_path_buf()));
    let root = store.session_paths("s1", None).root().to_path_buf();
    std::fs::create_dir_all(&root).expect("session dir");
    let repos = SessionRepos::new(&root, "s1", db);
    let key = dunce::canonicalize(checkout.join(".git")).expect("git dir");
    let clone = repos
        .ensure_local_clone(
            &checkout,
            &key,
            &MayHomePaths::new(home.path().to_path_buf()).repo_staging_dir(),
        )
        .await
        .expect("clone");

    let response = router
        .go(get("/api/sessions/s1/repo-clones"))
        .await
        .expect("route answers");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().into_bytes().await.expect("body");
    let views: Vec<CloneView> = serde_json::from_slice(&body).expect("json");
    assert_eq!(views.len(), 1);
    assert_eq!(Path::new(&views[0].path), clone);
    assert_eq!(Path::new(&views[0].source), checkout);
    assert!(views[0].local);

    let missing = match router.go(get("/api/sessions/nope/repo-clones")).await {
        Ok(response) => response.status(),
        Err(error) => skyzen::error_response(&*error).status(),
    };
    assert_eq!(missing, StatusCode::NOT_FOUND);
}
