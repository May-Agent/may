//! Daemon PR list and host-side user merge/close endpoints.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use super::error::{bad_request_error, internal_error};
use super::state::DaemonState;
use super::types::{ApiPr, ApiPrChecks, ApiPrEvent};
use crate::db::{ChatDbError, PrFilter, PrState, PrSummaryRow};
use crate::pr::PrError;

#[derive(Debug, Deserialize, Default, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct PrListQuery {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    orphaned: Option<bool>,
    #[serde(default)]
    state: Option<String>,
    /// When set, returns PRs relevant to a session's PR panel: its repository's
    /// PRs, the session's own PRs, plus all orphaned open PRs.
    #[serde(default)]
    for_session: Option<String>,
}

/// SSE snapshot payload for `UiEventTopic::Prs` (`"prs"`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PrsEventPayload {
    pub(super) prs: Vec<ApiPr>,
}

/// Maps database PR summary rows into wire [`ApiPr`] items, attaching live
/// check rollups from `state.pr_checks` and full provenance event logs.
pub(super) async fn build_api_prs(
    state: &DaemonState,
    rows: Vec<PrSummaryRow>,
) -> Result<Vec<ApiPr>, ChatDbError> {
    let workspace_path =
        crate::session::MayHomePaths::new(state.may_dir.clone()).workspace_config();
    let workspace_config =
        crate::workspace::WorkspaceConfig::load(&workspace_path).unwrap_or_default();
    let mut prs = Vec::with_capacity(rows.len());
    for row in rows {
        let (checks, checks_error) = match state.pr_checks.get(row.id) {
            Some(Ok(report)) => (Some(ApiPrChecks::of(&report)), None),
            Some(Err(error)) => (None, Some(error)),
            None => (None, None),
        };
        let require_review =
            crate::pr::pr_requires_review(&workspace_config, &row.backend, &row.repo_key);
        let events = state
            .db()
            .pr_events(row.id)
            .await?
            .into_iter()
            .map(|e| ApiPrEvent {
                session_id: e.session_id,
                actor: e.agent_id,
                message_id: e.message_id,
                kind: e.kind,
                detail: e.detail,
                created_at: e.created_at,
            })
            .collect();
        prs.push(ApiPr {
            id: row.id,
            repo_key: row.repo_key,
            session_id: row.session_id,
            orphaned: row.orphaned,
            require_review,
            number: row.number,
            title: row.title,
            state: row.state,
            backend: row.backend,
            external_number: row.external_number,
            issue_number: row.issue_number,
            auto_merge: row.auto_merge,
            base_sha: row.base_sha,
            head_sha: row.head_sha,
            merge_commit_sha: row.merge_commit_sha,
            created_at: row.created_at,
            updated_at: row.updated_at,
            merged_at: row.merged_at,
            checks,
            checks_error,
            events,
        });
    }
    Ok(prs)
}

pub(super) async fn load_all_api_prs(state: &DaemonState) -> Result<Vec<ApiPr>, ChatDbError> {
    let rows = state
        .db()
        .list_prs_with_repo(None, None, PrFilter::All, false)
        .await?;
    build_api_prs(state, rows).await
}

/// `GET /api/prs`.
pub(super) async fn list_prs(
    Query(query): Query<PrListQuery>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<ApiPr>>> {
    let (db_filter, exact_state) =
        parse_state_filter(query.state.as_deref()).map_err(bad_request_error)?;

    let mut rows = if let Some(for_session) = query
        .for_session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        rows_for_session_surface(&state, for_session, db_filter)
            .await
            .map_err(internal_error)?
    } else {
        let repo_filter = resolve_repo_filter(
            &state,
            query
                .repo
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        )
        .await
        .map_err(internal_error)?;
        let session_filter = query
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let orphaned_only = query.orphaned.unwrap_or(false);
        state
            .db()
            .list_prs_with_repo(
                repo_filter.as_deref(),
                session_filter,
                db_filter,
                orphaned_only,
            )
            .await
            .map_err(internal_error)?
    };

    if let Some(exact) = exact_state {
        rows.retain(|row| row.state == exact);
    }
    let prs = build_api_prs(&state, rows).await.map_err(internal_error)?;
    Ok(Json(prs))
}

/// `POST /api/prs/{id}/merge`.
pub(super) async fn merge_pr(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiPr>> {
    let pr_id = parse_pr_id(&params)?;
    let context = host_pr_context(&state);
    crate::pr::user_merge_pr(&context, pr_id)
        .await
        .map_err(|e| map_pr_error(&e))?;
    load_single_api_pr(&state, pr_id).await
}

/// `POST /api/prs/{id}/close`.
pub(super) async fn close_pr(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiPr>> {
    let pr_id = parse_pr_id(&params)?;
    let context = host_pr_context(&state);
    crate::pr::user_close_pr(&context, pr_id)
        .await
        .map_err(|e| map_pr_error(&e))?;
    load_single_api_pr(&state, pr_id).await
}

async fn load_single_api_pr(state: &DaemonState, pr_id: i64) -> SkyResult<Json<ApiPr>> {
    let rows = state
        .db()
        .list_prs_with_repo(None, None, PrFilter::All, false)
        .await
        .map_err(internal_error)?;
    let matched: Vec<_> = rows.into_iter().filter(|row| row.id == pr_id).collect();
    let mut built = build_api_prs(state, matched)
        .await
        .map_err(internal_error)?;
    built
        .pop()
        .ok_or_else(|| {
            SkyError::msg(format!("no PR with id {pr_id}")).set_status(StatusCode::NOT_FOUND)
        })
        .map(Json)
}

async fn rows_for_session_surface(
    state: &DaemonState,
    session_id: &str,
    filter: PrFilter,
) -> Result<Vec<PrSummaryRow>, ChatDbError> {
    let repo_key = session_repo_key(state, session_id).await?;
    let all = state
        .db()
        .list_prs_with_repo(None, None, filter, false)
        .await?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in all {
        let belongs = row.session_id == session_id
            || repo_key.as_deref() == Some(row.repo_key.as_str())
            || (row.orphaned && row.state == PrState::Open);
        if belongs && seen.insert(row.id) {
            out.push(row);
        }
    }
    Ok(out)
}

async fn session_repo_key(
    state: &DaemonState,
    session_id: &str,
) -> Result<Option<String>, ChatDbError> {
    let Some(session) = state.db().get_session(session_id).await? else {
        return Ok(None);
    };
    if let Some(repo) = &session.repo
        && let Ok(binding) = crate::session::RepoBinding::parse(repo)
    {
        return Ok(Some(binding.clone_key()));
    }
    let directory = crate::session::SessionDirectory::of(&session, &state.may_dir);
    if let Some(checkout) = directory.checkout() {
        let workspace_path =
            crate::session::MayHomePaths::new(state.may_dir.clone()).workspace_config();
        let workspace_config =
            crate::workspace::WorkspaceConfig::load(&workspace_path).unwrap_or_default();
        if let Ok(Some(source)) = crate::repo_clone::CloneSource::of(checkout, &workspace_config) {
            return Ok(Some(source.key()));
        }
    }
    let clones = state.db().list_repo_clones(session_id).await?;
    if let Some(first) = clones.into_iter().next() {
        return Ok(Some(first.repo_key));
    }
    Ok(None)
}

async fn resolve_repo_filter(
    state: &DaemonState,
    raw: Option<&str>,
) -> Result<Option<String>, ChatDbError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let rows = state
        .db()
        .list_prs_with_repo(None, None, PrFilter::All, false)
        .await?;
    if let Some(matched) = rows.into_iter().find(|row| {
        row.repo_key == raw || crate::pr::github_owner_repo(&row.repo_key).as_deref() == Some(raw)
    }) {
        return Ok(Some(matched.repo_key));
    }
    Ok(Some(raw.to_string()))
}

fn parse_state_filter(raw: Option<&str>) -> Result<(PrFilter, Option<PrState>), String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("all") => Ok((PrFilter::All, None)),
        Some("open") => Ok((PrFilter::Open, None)),
        Some("checks_pending") => Ok((PrFilter::All, Some(PrState::ChecksPending))),
        Some("failed") => Ok((PrFilter::All, Some(PrState::Failed))),
        Some("rejected") => Ok((PrFilter::All, Some(PrState::Rejected))),
        Some("merging") => Ok((PrFilter::All, Some(PrState::Merging))),
        Some("merged") => Ok((PrFilter::All, Some(PrState::Merged))),
        Some("closed") => Ok((PrFilter::All, Some(PrState::Closed))),
        Some(other) => Err(format!(
            "invalid state filter {other:?}; expected all, open, or a PR state"
        )),
    }
}

fn parse_pr_id(params: &Params) -> SkyResult<i64> {
    let raw = params.get("id").map_err(bad_request_error)?;
    raw.parse::<i64>()
        .map_err(|_| bad_request_error(format!("invalid PR id {raw:?}")))
}

fn host_pr_context(state: &DaemonState) -> crate::pr::PrContext {
    let workspace_path =
        crate::session::MayHomePaths::new(state.may_dir.clone()).workspace_config();
    let workspace_config =
        crate::workspace::WorkspaceConfig::load(&workspace_path).unwrap_or_default();
    let host_git = crate::host_git::HostGit::inherit();
    crate::pr::PrContext {
        db: Some(state.db.clone()),
        session_id: String::new(),
        session_root: state.may_dir.clone(),
        scratch_root: crate::session::MayHomePaths::new(state.may_dir.clone()).repo_staging_dir(),
        cwd: state.may_dir.clone(),
        workspace_config,
        review: None,
        signing: crate::commit_signing::CommitSigning::new(host_git, &state.may_dir, "user"),
        github_api: state.github_api.clone(),
        land_locks: state.services.land_locks.clone(),
        checkouts: Some(state.services.checkouts.clone()),
        task_events: None,
        publish: None,
        ci: Some(crate::pr::CiWatch {
            may_dir: state.may_dir.clone(),
            ui_events: state.ui_event_bus.clone(),
        }),
        ui_events: Some(state.ui_event_bus.clone()),
    }
}

fn map_pr_error(error: &PrError) -> SkyError {
    let message = error.to_string();
    let status = match &error {
        PrError::State(_) if message.starts_with("no PR with id ") => StatusCode::NOT_FOUND,
        PrError::State(_)
        | PrError::Ownership(_)
        | PrError::Resolve(_)
        | PrError::Rejected(_)
        | PrError::CheckFailed(_)
        | PrError::DirtyClone(_)
        | PrError::Screened(_)
        | PrError::MainAgentOnly(_) => StatusCode::CONFLICT,
        PrError::Gh(_) | PrError::Signing(_) => StatusCode::BAD_GATEWAY,
        PrError::Db(_) | PrError::RepoClone(_) | PrError::Publish(_) | PrError::Io(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    SkyError::msg(message).set_status(status)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http_body_util::BodyExt as _;
    use skyzen::{Request, StatusCode};

    use super::super::auth::SocketOwner;
    use super::super::server::{DaemonMode, build_router};
    use super::super::state::DaemonState;
    use super::super::types::ApiPr;
    use crate::config::MayConfig;
    use crate::db::{PrState, RepoCloneRow};
    use crate::ui_event_bus::UiEventTopics;

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

        async fn call(&self, method: &str, uri: &str) -> (StatusCode, Vec<u8>) {
            let request: Request = hyper::Request::builder()
                .method(method)
                .uri(uri)
                .body(skyzen::Body::empty())
                .expect("request builds");
            let response = match self.router.go(request).await {
                Ok(resp) => resp,
                Err(err) => skyzen::error_response(&*err),
            };
            let status = response.status();
            let bytes = response
                .into_body()
                .collect()
                .await
                .expect("collect body")
                .to_bytes()
                .to_vec();
            (status, bytes)
        }
    }

    #[tokio::test]
    async fn pr_routes_filter_list_and_enforce_user_close_after_orphan() {
        let host = Host::new().await;
        let db = host.state.db();
        db.create_session("s1", None, None).await.unwrap();
        db.create_session("s2", None, None).await.unwrap();
        let repo_a = "/tmp/repo-a/.git";
        let repo_b = "/tmp/repo-b/.git";
        db.insert_repo_clone(
            "s2",
            &RepoCloneRow {
                repo_key: repo_a.to_string(),
                dir: "repo-a".to_string(),
                source: repo_a.to_string(),
                local: true,
                base_sha: None,
            },
        )
        .await
        .unwrap();

        let pr_s1_open = db
            .create_pr(
                repo_a,
                "s1",
                "Repo A s1 open",
                "",
                "b1",
                "sha1",
                "may",
                false,
                None,
                None,
            )
            .await
            .unwrap();
        let pr_s2_rej = db
            .create_pr(
                repo_a,
                "s2",
                "Repo A s2 rejected",
                "",
                "b2",
                "sha2",
                "may",
                false,
                None,
                None,
            )
            .await
            .unwrap();
        db.update_pr(pr_s2_rej.id, PrState::Rejected, Some("sha2h"), None, None)
            .await
            .unwrap();
        db.add_pr_event(
            pr_s2_rej.id,
            "rejected",
            "s2",
            "main",
            None,
            Some("conflict"),
        )
        .await
        .unwrap();

        let pr_orphan = db
            .create_pr(
                repo_b,
                "gone-session",
                "Orphaned B",
                "",
                "b3",
                "sha3",
                "may",
                false,
                None,
                None,
            )
            .await
            .unwrap();

        // Filter by repo, sessionId, orphaned, state, and forSession.
        let (st, body) = host.call("GET", "/api/prs?orphaned=true&state=open").await;
        assert_eq!(st, StatusCode::OK);
        let orphaned_list: Vec<ApiPr> = serde_json::from_slice(&body).unwrap();
        assert_eq!(orphaned_list.len(), 1);
        assert_eq!(orphaned_list[0].id, pr_orphan.id);
        assert!(orphaned_list[0].orphaned);

        let (st, body) = host.call("GET", "/api/prs?state=rejected").await;
        assert_eq!(st, StatusCode::OK);
        let rejected_list: Vec<ApiPr> = serde_json::from_slice(&body).unwrap();
        assert_eq!(rejected_list.len(), 1);
        assert_eq!(rejected_list[0].id, pr_s2_rej.id);
        assert_eq!(rejected_list[0].events.len(), 1);
        assert_eq!(rejected_list[0].events[0].actor, "main");

        let (st, body) = host.call("GET", "/api/prs?forSession=s2").await;
        assert_eq!(st, StatusCode::OK);
        let for_s2: Vec<ApiPr> = serde_json::from_slice(&body).unwrap();
        let ids: Vec<i64> = for_s2.iter().map(|p| p.id).collect();
        assert!(ids.contains(&pr_s1_open.id));
        assert!(ids.contains(&pr_s2_rej.id));
        assert!(ids.contains(&pr_orphan.id));

        // Closing s1's active PR directly as user is rejected while s1 exists without require_review.
        let (st, _) = host
            .call("POST", &format!("/api/prs/{}/close", pr_s1_open.id))
            .await;
        assert_eq!(st, StatusCode::CONFLICT);

        // Delete session s1 -> emits UiEventTopics::PRS and makes pr_s1_open orphaned.
        let mut rx = host.state.ui_event_bus.subscribe();
        let (st, _) = host.call("DELETE", "/api/sessions/s1").await;
        assert_eq!(st, StatusCode::OK);
        let mut saw_prs = false;
        while let Ok(topics) = rx.try_recv() {
            if topics.contains(UiEventTopics::PRS) {
                saw_prs = true;
            }
        }
        assert!(saw_prs);

        // User can now close the orphaned PR with actor "user" recorded in provenance.
        let (st, body) = host
            .call("POST", &format!("/api/prs/{}/close", pr_s1_open.id))
            .await;
        assert_eq!(st, StatusCode::OK);
        let closed: ApiPr = serde_json::from_slice(&body).unwrap();
        assert_eq!(closed.state, PrState::Closed);
        assert!(
            closed
                .events
                .iter()
                .any(|e| e.actor == "user" && e.kind == "closed")
        );
    }
}
