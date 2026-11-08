use std::str::FromStr;
use std::sync::Arc;

use futures_util::StreamExt;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::db::ProfileEnvironmentKind;
use crate::pr::merge_watch::Upstream;
use crate::session::{
    MayHomePaths, SandboxResourceLayout, SessionId, SessionStore, list_summaries_from_db,
};
use crate::ui_event_bus::UiEventTopics;
use aither_core::llm::{Message as LlmMessage, Role as LlmRole};

use super::error::{bad_request_error, internal_error, internal_error_message};
use super::model_resolution::{
    persist_last_effort, persist_last_model, preferred_advanced_model, resolve_default_model,
};
use super::state::DaemonState;
use super::types::{
    ApiHealth, ApiPr, ApiPrEvent, ApiSession, ContextDebugMessage, CreateSessionRequest,
    NullableStringPatch, RewindRequest, SessionContextDebugResponse,
    SessionContextWindowMetricsResponse, SessionContextWindowSnapshotResponse,
    SessionPlanStateRequest, SessionPlanStateResponse, SessionTasksPayload, UpdateSessionRequest,
};

fn session_not_found_error(session_id: &str) -> SkyError {
    SkyError::msg(format!("Session {session_id} not found")).set_status(StatusCode::NOT_FOUND)
}

fn context_debug_not_found_error(session_id: &str) -> SkyError {
    SkyError::msg(format!(
        "No context debug snapshot for session {session_id}"
    ))
    .set_status(StatusCode::NOT_FOUND)
}

fn resolve_session_model(
    state: &DaemonState,
    session_id: &str,
    model: Option<String>,
) -> Result<String, skyzen::Error> {
    let config = state.config_snapshot();
    resolve_default_model(&config, model).ok_or_else(|| {
        internal_error_message(format!("no model resolved for session {session_id}"))
    })
}

pub(super) async fn list_sessions(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<ApiSession>>> {
    Ok(Json(list_api_sessions(state.as_ref()).await?))
}

pub(super) async fn list_api_sessions(
    state: &DaemonState,
) -> Result<Vec<ApiSession>, skyzen::Error> {
    let db = state.db();
    let summaries = list_summaries_from_db(&db).await.map_err(internal_error)?;

    let config = state.config_snapshot();
    let mut sessions = Vec::with_capacity(summaries.len());
    for s in summaries {
        let model = resolve_default_model(&config, s.model.clone()).ok_or_else(|| {
            internal_error_message(format!("no model resolved for session {}", s.id.as_str()))
        })?;
        sessions.push(ApiSession::from_summary(s, model));
    }

    Ok(sessions)
}

/// `POST /api/sessions/{id}/history` — append stored messages in the
/// clients' `ChatMessage` shape, for flows that inject history without a
/// turn (transcript import).
pub(super) async fn append_session_history(
    params: Params,
    Json(body): Json<serde_json::Value>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<i64>> {
    let messages: Vec<crate::session::ChatMessage> =
        serde_json::from_value(body).map_err(bad_request_error)?;
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let db = state.db();
    let mut last = 0i64;
    for message in &messages {
        let row = db
            .append_chat_message(&id, message)
            .await
            .map_err(internal_error)?;
        last = i64::from(row.id);
    }
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
    Ok(Json(last))
}

/// `POST /api/sessions/{id}/rewind` — delete the session's stored history
/// from the `index`-th stored row on, the truncation half of a client-side
/// rewind. Clients index it by position among the stored rows, which hidden
/// and tool rows share with the messages they see.
pub(super) async fn rewind_session(
    params: Params,
    Json(req): Json<RewindRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<i64>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let db = state.db();
    let rows = db.get_messages(&id).await.map_err(internal_error)?;
    let index = {
        let history = crate::session::load_history_from_db(&db, &id)
            .await
            .map_err(internal_error)?;
        let entries = super::transcript::build_entries(history.messages());
        super::transcript::entry_cut_index(&rows, &entries, &req.entry_id)?
    };
    let Some(row) = rows.get(index) else {
        return Err(bad_request_error("rewind index out of range"));
    };
    let cut_id = i64::from(row.id);
    db.delete_messages_from(&id, cut_id)
        .await
        .map_err(internal_error)?;
    // The audit log is append-only: every PR this session opened keeps a
    // `rewound` event so its history records that the conversation was
    // cut back while the proposal itself still stands.
    if let Ok(prs) = db.list_session_prs(&id).await {
        let detail = format!("history rewound to message {cut_id}");
        for pr in prs {
            let _ = db
                .add_pr_event(pr.id, "rewound", &id, "user", None, Some(&detail))
                .await;
        }
    }
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
    Ok(Json(cut_id))
}

/// `POST /api/sessions/{id}/issues/lookup` — resolve the session's issue
/// store and return a brief per requested number; misses yield `null`
/// titles, matching the client's degraded rendering.
pub(super) async fn lookup_issues(
    params: Params,
    Json(body): Json<serde_json::Value>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<skyzen::Response> {
    #[derive(serde::Deserialize)]
    struct LookupRequest {
        numbers: Vec<i64>,
    }
    let req: LookupRequest = serde_json::from_value(body).map_err(bad_request_error)?;
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let briefs = issue_briefs(&state, &id, req.numbers).await?;
    super::error::json_ok_response(&briefs)
}

/// The issues `session_issues` reports for `session_id`: its tasks' linked
/// issues that the store could look up.
pub(super) async fn linked_issue_briefs(
    state: &DaemonState,
    session_id: &str,
    paths: &crate::session::SessionPaths,
) -> SkyResult<Vec<crate::tui::sidebar::IssueBrief>> {
    let content = paths.tasks_content().await.map_err(internal_error)?;
    let numbers = content
        .as_deref()
        .map(|content| crate::tui::sidebar::linked_issues(&crate::task_rows::task_rows(content)))
        .unwrap_or_default();
    let mut briefs = issue_briefs(state, session_id, numbers).await?;
    // A failed lookup says nothing new; the client keeps what it shows.
    briefs.retain(|brief| brief.title.is_some());
    Ok(briefs)
}

/// One brief per requested number from the session's issue store; a miss
/// yields a `null` title.
async fn issue_briefs(
    state: &DaemonState,
    id: &str,
    numbers: Vec<i64>,
) -> SkyResult<Vec<crate::tui::sidebar::IssueBrief>> {
    use crate::issue::IssueStore as _;
    let id = id.to_string();
    let db = state.db();
    let session = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| session_not_found_error(&id))?;
    let cwd = crate::session::SessionDirectory::of(&session, &state.may_dir).into_path();
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    let sandbox_dir = store.sandbox_dir(&id, session.profile_id.as_deref());
    let workspace_config = crate::session::SessionPaths::new(sandbox_dir).workspace_config();
    let issue_store =
        blocking::unblock(move || crate::issue::resolve_issue_store(&cwd, &workspace_config, db))
            .await
            .map_err(internal_error)?;
    let mut briefs = Vec::with_capacity(numbers.len());
    for number in numbers {
        let brief = match issue_store.get(number).await {
            Ok(detail) => crate::tui::sidebar::IssueBrief {
                number,
                title: Some(detail.title),
                closed: matches!(detail.state, crate::issue::IssueState::Closed),
            },
            Err(error) => {
                tracing::debug!(%error, number, "issue lookup failed");
                crate::tui::sidebar::IssueBrief {
                    number,
                    title: None,
                    closed: false,
                }
            }
        };
        briefs.push(brief);
    }
    Ok(briefs)
}

pub(super) async fn get_session(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiSession>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    // The cursor must precede every read so nothing between it and the
    // client's `?after=` subscription is lost.
    let cursor = state.session_cursor(&id);

    let db = state.db();

    let db_session = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| session_not_found_error(&id))?;

    let mut summary = crate::session::SessionSummary::from_db_model(db_session);

    // Fetch runtime and messages in parallel — they are independent queries
    let (runtime_result, messages_result) =
        tokio::join!(db.get_session_runtime(&id), db.get_messages(&id),);
    summary.runtime = runtime_result.map_err(internal_error)?.unwrap_or_default();
    let model = resolve_session_model(&state, &id, summary.model.clone())?;
    let _ = messages_result.map_err(internal_error)?;

    let mut session = ApiSession::from_summary(summary, model);
    session.cursor = Some(cursor);
    Ok(Json(session))
}

pub(super) async fn get_session_context_debug(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionContextDebugResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();
    let row = db
        .get_context_debug_snapshot(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| context_debug_not_found_error(&id))?;
    let window_snapshot = db
        .get_session_context_snapshot(&id)
        .await
        .map_err(internal_error)?
        .map(build_window_snapshot);

    Ok(Json(SessionContextDebugResponse {
        session_id: row.session_id,
        timestamp: row.timestamp,
        model: row.model,
        provider_id: row.provider_id,
        prompt: row.prompt,
        prompt_attachments: row.prompt_attachments,
        assembled_messages: row
            .assembled_messages
            .into_iter()
            .map(|message| ContextDebugMessage {
                role: message.role,
                content: message.content,
                attachments: message.attachments,
                tool_call_id: message.tool_call_id,
                tool_call_names: message.tool_call_names,
            })
            .collect(),
        window_snapshot,
    }))
}

pub(super) async fn get_session_tasks(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionTasksPayload>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let paths = super::helpers::load_session_paths(&state, &id).await?;
    let runtime = state
        .runtime_service
        .get(&id)
        .await
        .map_err(internal_error)?;
    let content = paths
        .tasks_content()
        .await
        .map_err(internal_error)?
        .unwrap_or_default();
    Ok(Json(SessionTasksPayload {
        rows: crate::task_rows::task_rows(&content),
        content,
        plan_state: runtime.plan_state,
    }))
}

/// `GET /api/sessions/{id}/repo-clones`: the session's repository clones from
/// the host's registry.
pub(super) async fn get_session_repo_clones(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<crate::repo_clone::CloneView>>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let db = state.db();
    let session = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| session_not_found_error(&id))?;
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    let paths = store.session_paths(&id, session.profile_id.as_deref());
    let repos = crate::repo_clone::SessionRepos::new(paths.root(), id, db);
    Ok(Json(repos.views().await.map_err(internal_error)?))
}

/// The session's PRs with their transition events — the sidebar's list.
pub(super) async fn get_session_prs(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<Vec<ApiPr>>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let rows = state
        .db()
        .list_session_prs(&id)
        .await
        .map_err(internal_error)?;
    // Live checks ride the same payload: every open GitHub PR gets one
    // GraphQL snapshot of its head rollup, best-effort — a slow or dead
    // GitHub must not hold up the list itself. `buffered` keeps the row
    // order so each summary zips back onto its own PR.
    let mut check_reads = Vec::with_capacity(rows.len());
    for row in &rows {
        check_reads.push(pr_check_summary(&state, row));
    }
    let summaries = futures_util::stream::iter(check_reads)
        .buffered(4)
        .collect::<Vec<_>>()
        .await;
    let mut prs = Vec::with_capacity(rows.len());
    for (row, summary) in rows.into_iter().zip(summaries) {
        let events = state
            .db()
            .pr_events(row.id)
            .await
            .map_err(internal_error)?
            .into_iter()
            .map(|e| ApiPrEvent {
                kind: e.kind,
                detail: e.detail,
                created_at: e.created_at,
            })
            .collect();
        prs.push(ApiPr {
            number: row.number,
            title: row.title,
            state: row.state,
            backend: row.backend,
            external_number: row.external_number,
            issue_number: row.issue_number,
            auto_merge: row.auto_merge,
            created_at: row.created_at,
            updated_at: row.updated_at,
            merged_at: row.merged_at,
            checks: summary.map(|s| super::types::ApiPrChecks {
                pending: s.pending,
                passed: s.passed,
                failed: s.failed,
            }),
            events,
        });
    }
    Ok(Json(prs))
}

/// One GraphQL snapshot of a live GitHub PR's check rollup; `None` for
/// local/may-backend PRs, landed states, and failed reads.
async fn pr_check_summary(
    state: &DaemonState,
    row: &crate::db::PrRow,
) -> Option<crate::pr::merge_watch::CheckSummary> {
    let live = row.backend == "github"
        && matches!(row.state.as_str(), "open" | "checks_pending" | "merging")
        && row.external_number.is_some();
    if !live {
        return None;
    }
    let repo_key = state
        .db()
        .repo_key_of_project(row.project_id)
        .await
        .ok()
        .flatten()?;
    let github = crate::pr::merge_watch::GithubPr {
        api: &state.github_api,
        owner_repo: &repo_key,
        number: row.external_number?,
    };
    github
        .snapshot()
        .await
        .ok()
        .map(|snapshot| snapshot.check_summary())
}

/// Atomically set the plan state for a session without touching task content.
///
/// This is what the UI uses to toggle plan mode (enter/approve/reopen); it
/// avoids the fetch-then-save race of the generic `save_session_tasks` path.
pub(super) async fn set_session_plan_state(
    params: Params,
    Json(payload): Json<SessionPlanStateRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionPlanStateResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let runtime = state
        .runtime_service
        .set_plan_state(&id, payload.plan_state)
        .await
        .map_err(internal_error)?;
    Ok(Json(SessionPlanStateResponse {
        plan_state: runtime.plan_state,
    }))
}

/// Set the session's yolo switch; it applies to the next approval request.
pub(super) async fn set_session_yolo(
    params: Params,
    Json(payload): Json<super::types::SessionYoloRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<super::types::SessionYoloResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let runtime = state
        .runtime_service
        .set_yolo(&id, payload.yolo)
        .await
        .map_err(internal_error)?;
    Ok(Json(super::types::SessionYoloResponse {
        yolo: runtime.yolo,
    }))
}

pub(super) async fn save_session_tasks(
    params: Params,
    Json(payload): Json<SessionTasksPayload>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SessionTasksPayload>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let paths = super::helpers::load_session_paths(&state, &id).await?;
    async_fs::write(paths.tasks_path(), &payload.content)
        .await
        .map_err(internal_error)?;
    let runtime = state
        .runtime_service
        .set_plan_state(&id, payload.plan_state)
        .await
        .map_err(internal_error)?;
    Ok(Json(SessionTasksPayload {
        rows: crate::task_rows::task_rows(&payload.content),
        content: payload.content,
        plan_state: runtime.plan_state,
    }))
}

fn build_window_snapshot(
    snapshot: crate::db::StoredSessionContextSnapshot,
) -> SessionContextWindowSnapshotResponse {
    let window = snapshot.checkpoint.context_window;

    SessionContextWindowSnapshotResponse {
        phase: window.phase,
        metrics: SessionContextWindowMetricsResponse::from(window.metrics),
        assembled_messages: window
            .messages
            .iter()
            .map(map_context_window_message)
            .collect(),
    }
}

fn map_context_window_message(message: &LlmMessage) -> ContextDebugMessage {
    ContextDebugMessage {
        role: map_llm_role(message.role()).to_string(),
        content: message.content().to_string(),
        attachments: message
            .attachments()
            .iter()
            .map(|attachment| attachment.url().to_string())
            .collect(),
        tool_call_id: message.tool_call_id().map(ToString::to_string),
        tool_call_names: message
            .tool_calls()
            .iter()
            .map(|call| call.name.clone())
            .collect(),
    }
}

const fn map_llm_role(role: LlmRole) -> &'static str {
    match role {
        LlmRole::User => "user",
        LlmRole::Assistant => "assistant",
        LlmRole::System => "system",
        LlmRole::Tool => "tool",
    }
}

/// Creates the durable DB row and session directory layout for a profile.
///
/// Shared by the regular create-session endpoint and notification callbacks,
/// which open a fresh conversation in the originating profile.
pub(super) async fn create_session_for_profile(
    state: &Arc<DaemonState>,
    id: Option<&str>,
    profile_id: Option<&str>,
    model: &str,
) -> SkyResult<String> {
    let id = id.map_or_else(|| SessionId::new().to_string(), str::to_string);
    let db = state.db();
    let layout = match profile_id {
        Some(profile_id) => {
            let profile = db
                .get_profile(profile_id)
                .await
                .map_err(internal_error)?
                .ok_or_else(|| bad_request_error("profile not found"))?;
            let environment = ProfileEnvironmentKind::from_str(profile.environment.as_str())
                .map_err(internal_error)?;
            if environment == ProfileEnvironmentKind::Container {
                SandboxResourceLayout::ContainerMountPoints
            } else {
                SandboxResourceLayout::Symlinked
            }
        }
        None => SandboxResourceLayout::Symlinked,
    };
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .create_session_dirs(&id, profile_id, layout)
        .await
        .map_err(internal_error)?;
    db.create_session_in_profile(&id, None, Some(model), profile_id)
        .await
        .map_err(internal_error)?;
    Ok(id)
}

pub(super) async fn create_session(
    Json(req): Json<CreateSessionRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiSession>> {
    let requested_profile_id = req.profile_id.clone();
    let model = {
        let config = state.config_snapshot();
        req.model
            .or_else(|| config.last_model.as_ref().map(ToString::to_string))
            .or_else(|| preferred_advanced_model(&config))
            .ok_or_else(|| bad_request_error("No model configured"))?
    };

    let resolved_profile_id = requested_profile_id
        .clone()
        .or_else(|| Some(state.default_profile_id.clone()));
    let id = create_session_for_profile(
        &state,
        req.id.as_deref(),
        resolved_profile_id.as_deref(),
        &model,
    )
    .await?;
    if let Some(cwd) = req
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
    {
        state
            .db()
            .set_session_cwd(&id, cwd)
            .await
            .map_err(internal_error)?;
        if let Some(worktree) = crate::repo_search::worktree_root(std::path::Path::new(cwd)) {
            let _ = state.db().touch_repo_usage(&worktree).await;
        }
    }

    if let Some(repo) = resolve_repo_binding(req.repo.as_deref()).map_err(bad_request_error)? {
        state
            .db()
            .set_session_repo(&id, Some(repo.as_str()))
            .await
            .map_err(internal_error)?;
    }

    persist_last_model(&state, &model)
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS | UiEventTopics::SESSION_RUNTIMES);

    let mut session = ApiSession::new_empty(id, model, resolved_profile_id);
    session.repo = resolve_repo_binding(req.repo.as_deref()).ok().flatten();
    Ok(Json(session))
}

/// Normalize a repo spec into a stored binding: `owner/repo` or a
/// `github:owner/repo` spec binds the GitHub repository itself; an
/// existing path binds that repository's local identity; anything else
/// fails.
pub(super) fn resolve_repo_binding(spec: Option<&str>) -> Result<Option<String>, String> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let github = spec.strip_prefix("github:").unwrap_or(spec);
    let looks_like_github = {
        let mut parts = github.split('/');
        matches!(
            (parts.next(), parts.next(), parts.next()),
            (Some(o), Some(r), None) if !o.is_empty() && !r.is_empty() && !o.contains(' ') && !r.contains(' ')
        )
    };
    // An existing filesystem path binds the local repository identity.
    let path = std::path::Path::new(spec);
    if let Some(worktree) = crate::repo_search::worktree_root(path)
        && !spec.starts_with("github:")
    {
        let key = crate::issue::repository_identity(&worktree)
            .map_err(|e| format!("repository identity: {e}"))?;
        return Ok(Some(format!("local:{}", key.display())));
    }
    if looks_like_github {
        return Ok(Some(format!("github:{github}")));
    }
    Err(format!("invalid repo spec '{spec}'"))
}

pub(super) async fn update_session(
    params: Params,
    Json(req): Json<UpdateSessionRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiSession>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let db = state.db();
    let session = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| session_not_found_error(&id))?;

    let mut model_to_persist: Option<String> = None;
    let requested_title = req.title.clone();
    let requested_model = req.model.clone();
    let requested_effort_patch = req.reasoning_effort.clone();
    let next_title = requested_title.clone().or_else(|| session.title.clone());
    let next_model = requested_model.clone().or_else(|| session.model.clone());
    let next_effort = match requested_effort_patch.as_ref() {
        Some(NullableStringPatch::Null) => None,
        Some(NullableStringPatch::Set(value)) => Some(value.clone()),
        None => session.reasoning_effort.clone(),
    };

    if let Some(model) = next_model.clone()
        && session.model.as_deref() != Some(model.as_str())
    {
        model_to_persist = Some(model);
    }

    let effort_patch_ref = requested_effort_patch.as_ref().map(|inner| match inner {
        NullableStringPatch::Null => None,
        NullableStringPatch::Set(value) => Some(value.as_str()),
    });

    db.update_session(
        &id,
        requested_title.as_deref(),
        requested_model.as_deref(),
        effort_patch_ref,
    )
    .await
    .map_err(internal_error)?;

    if let Some(model) = model_to_persist {
        persist_last_model(&state, &model)
            .await
            .map_err(internal_error)?;
    }
    if let Some(NullableStringPatch::Set(effort)) = requested_effort_patch.as_ref() {
        persist_last_effort(&state, effort)
            .await
            .map_err(internal_error)?;
    }
    if let Some(cwd) = req
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
    {
        db.set_session_cwd(&id, cwd).await.map_err(internal_error)?;
        if let Some(worktree) = crate::repo_search::worktree_root(std::path::Path::new(cwd)) {
            let _ = db.touch_repo_usage(&worktree).await;
        }
    }
    let mut next_repo = session.repo.clone();
    if let Some(patch) = req.repo.clone() {
        next_repo = match patch {
            crate::daemon::types::NullableStringPatch::Null => None,
            crate::daemon::types::NullableStringPatch::Set(spec) => {
                resolve_repo_binding(Some(spec.as_str())).map_err(bad_request_error)?
            }
        };
        db.set_session_repo(&id, next_repo.as_deref())
            .await
            .map_err(internal_error)?;
    }
    if req.non_interactive == Some(true) {
        db.set_session_non_interactive(&id)
            .await
            .map_err(internal_error)?;
    }

    let model = resolve_session_model(&state, session.id.as_str(), next_model.clone())?;
    let runtime = state
        .runtime_service
        .get(&id)
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);

    Ok(Json(ApiSession {
        id,
        title: next_title.unwrap_or_else(|| "New Chat".to_string()),
        last_modified: jiff::Timestamp::now().to_string(),
        preview: session
            .preview
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "New conversation started".to_string()),
        profile_id: session.profile_id,
        model,
        reasoning_effort: next_effort,
        runtime: runtime.into(),
        repo: next_repo,
        cursor: None,
    }))
}

pub(super) async fn delete_session(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let db = state.db();
    let profile_id = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .and_then(|session| session.profile_id);
    // Live PRs of the dying session stay on record — the audit log marks
    // them `orphaned` so a later session knows to `pr adopt` before merge.
    if let Ok(prs) = db.list_session_prs(&id).await {
        for pr in prs {
            if matches!(
                pr.state.as_str(),
                "open" | "checks_pending" | "failed" | "rejected" | "merging"
            ) {
                let _ = db
                    .add_pr_event(
                        pr.id,
                        "orphaned",
                        &id,
                        "user",
                        None,
                        Some("session deleted"),
                    )
                    .await;
            }
        }
    }
    // The session's own secrets go first: a keychain failure keeps the
    // session, so the delete can be retried instead of orphaning them.
    let secrets = crate::secrets::SecretStore::new(&state.may_dir);
    let session_id = id.clone();
    blocking::unblock(move || secrets.remove_session(&session_id))
        .await
        .map_err(internal_error)?;
    db.delete_session(&id).await.map_err(internal_error)?;
    state.parked_agents.evict(&id);
    // After the eviction, whose dropped agent sessions start closing: the
    // host waits for those closes before it ends the rest and removes the
    // session's ACP storage.
    state
        .acp_host
        .remove_session(&id)
        .await
        .map_err(internal_error)?;
    state.runtime_service.forget_terminal_grants(&id);
    state.runtime_service.forget_task_events(&id);
    state.tunnels.remove_session(&id);
    #[cfg(target_os = "macos")]
    if state.computer.is_running() {
        // No helper means no display to release; never launch one to say so.
        drop(
            state
                .computer
                .request(crate::computer::proto::AgentRequest::ReleaseDisplay {
                    session_id: id.clone(),
                })
                .await,
        );
    }
    state
        .tool_environments
        .evict(&crate::session::SessionId::from_string(id.as_str()));
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .delete_session(&id, profile_id.as_deref(), &db)
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS | UiEventTopics::SESSION_RUNTIMES);
    Ok(Json(ApiHealth { ok: true }))
}

pub(super) async fn delete_all_sessions(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiHealth>> {
    // Delete all from DB
    let db = state.db();
    state.tunnels.remove_all();
    // Every live PR outlives its session — mark them `orphaned` first.
    if let Ok(prs) = db.list_prs(None, Some("open")).await {
        for pr in prs {
            let _ = db
                .add_pr_event(
                    pr.id,
                    "orphaned",
                    &pr.session_id,
                    "user",
                    None,
                    Some("session deleted"),
                )
                .await;
        }
    }
    let secrets = crate::secrets::SecretStore::new(&state.may_dir);
    blocking::unblock(move || secrets.remove_all_sessions())
        .await
        .map_err(internal_error)?;
    db.delete_all_sessions().await.map_err(internal_error)?;
    state
        .acp_host
        .remove_all_sessions()
        .await
        .map_err(internal_error)?;
    state.runtime_service.forget_all_terminal_grants();
    state.runtime_service.forget_all_task_events();
    state.tool_environments.evict_all();
    #[cfg(target_os = "macos")]
    {
        // One helper restart tears down every session desktop at once; the
        // wait for it to exit runs off the async thread.
        let computer = state.computer.clone();
        match tokio::task::spawn_blocking(move || computer.restart()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "computer-agent did not stop after session deletion");
            }
            Err(error) => {
                tracing::warn!(%error, "computer-agent restart task failed");
            }
        }
    }

    // Clean up filesystem session directories
    let store = SessionStore::new(MayHomePaths::new(state.may_dir.clone()));
    store
        .delete_all_session_dirs()
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS | UiEventTopics::SESSION_RUNTIMES);
    Ok(Json(ApiHealth { ok: true }))
}
