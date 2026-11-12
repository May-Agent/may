//! A session runs in the directory its row names or the checkout of its
//! bound repository, never in the daemon's own directory.

use std::path::Path;
use std::sync::Arc;

use skyzen::{Request, StatusCode};

use super::auth::SocketOwner;
use super::server::{DaemonMode, build_router};
use super::sessions::resolve_repo_binding;
use super::state::DaemonState;
use crate::config::MayConfig;

struct Host {
    _home: tempfile::TempDir,
    state: Arc<DaemonState>,
    router: skyzen::routing::Router,
}

impl Host {
    async fn new() -> Self {
        let home = tempfile::tempdir().expect("may home");
        let state = Arc::new(
            DaemonState::for_test(home.path(), MayConfig::default())
                .await
                .expect("daemon state"),
        );
        let router = build_router(state.clone(), DaemonMode::Rest, SocketOwner);
        Self {
            _home: home,
            state,
            router,
        }
    }

    async fn post(&self, uri: &str, body: &str) -> StatusCode {
        let request: Request = hyper::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(skyzen::Body::from(body.to_string()))
            .expect("request builds");
        match self.router.go(request).await {
            Ok(response) => response.status(),
            Err(error) => skyzen::error_response(&*error).status(),
        }
    }

    /// Runs `touch ran-here` as the session's `!` command and reports
    /// whether it landed in `dir`.
    async fn bash_runs_in(&self, id: &str, dir: &Path) -> bool {
        let status = self
            .post(
                &format!("/api/sessions/{id}/actions/bash"),
                r#"{"command":"touch ran-here"}"#,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        dir.join("ran-here").exists()
    }
}

#[tokio::test]
async fn a_session_runs_in_its_clients_directory() {
    let host = Host::new().await;
    let client = tempfile::tempdir().expect("client dir");
    let db = host.state.db();
    db.create_session("s", None, None).await.expect("session");
    db.set_session_cwd("s", client.path().to_str().expect("utf-8"))
        .await
        .expect("cwd");

    assert!(host.bash_runs_in("s", client.path()).await);
    // The sidebar lookup resolves the same directory.
    assert_eq!(
        host.post("/api/sessions/s/issues/lookup", r#"{"numbers":[]}"#)
            .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_session_without_a_directory_runs_in_its_sandbox() {
    let host = Host::new().await;
    let db = host.state.db();
    db.create_session("s", None, None).await.expect("session");
    let session = db.get_session("s").await.expect("row").expect("session");
    let sandbox = crate::session::SessionDirectory::of(&session, &host.state.may_dir);
    std::fs::create_dir_all(sandbox.path()).expect("sandbox dir");

    assert!(host.bash_runs_in("s", sandbox.path()).await);
    assert_eq!(
        host.post("/api/sessions/s/issues/lookup", r#"{"numbers":[]}"#)
            .await,
        StatusCode::OK,
        "a repo-less session answers the sidebar lookup"
    );
}

#[test]
fn the_stored_local_form_is_accepted_as_a_repo_spec() {
    assert_eq!(
        resolve_repo_binding(Some("local:/work/app/.git")),
        Ok(Some("local:/work/app/.git".to_string()))
    );
    assert!(resolve_repo_binding(Some("local:")).is_err());
}

#[tokio::test]
async fn a_repo_bound_session_without_a_directory_runs_in_the_checkout() {
    let host = Host::new().await;
    let checkout = tempfile::tempdir().expect("checkout");
    git2::Repository::init(checkout.path()).expect("git init");
    let binding = resolve_repo_binding(checkout.path().to_str())
        .expect("binding resolves")
        .expect("a binding");
    let db = host.state.db();
    db.create_session("s", None, None).await.expect("session");
    db.set_session_repo("s", Some(&binding))
        .await
        .expect("repo");

    assert!(host.bash_runs_in("s", checkout.path()).await);
}
