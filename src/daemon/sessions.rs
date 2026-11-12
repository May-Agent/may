use std::str::FromStr;
use std::sync::Arc;

use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use crate::db::ProfileEnvironmentKind;
use crate::session::{MayHomePaths, SandboxResourceLayout, SessionStore, list_summaries_from_db};
use crate::ui_event_bus::UiEventTopics;
use aither_core::llm::{Message as LlmMessage, Role as LlmRole};

use super::error::{bad_request_error, internal_error, internal_error_message};
use super::model_resolution::{
    persist_last_effort, persist_last_model, preferred_advanced_model, resolve_default_model,
};
use super::state::DaemonState;
use super::types::{
    ApiHealth, ApiPr, ApiPrEvent, ApiSession, ContextDebugMessage, CreateSessionRequest,
    RewindRequest, SessionContextDebugResponse, SessionContextWindowMetricsResponse,
    SessionContextWindowSnapshotResponse, SessionTasksPayload,
};

pub(super) fn session_not_found_error(session_id: &str) -> SkyError {
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
pub(super) async fn rewind_inner(
    state: &Arc<DaemonState>,
    id: &str,
    req: &RewindRequest,
) -> SkyResult<Json<i64>> {
    let db = state.db();
    let rows = db.get_messages(id).await.map_err(internal_error)?;
    let index = {
        let history = crate::session::load_history_from_db(&db, id)
            .await
            .map_err(internal_error)?;
        let entries = super::transcript::build_entries(history.messages());
        super::transcript::entry_cut_index(&rows, &entries, &req.entry_id)?
    };
    let Some(row) = rows.get(index) else {
        return Err(bad_request_error("rewind index out of range"));
    };
    let cut_id = i64::from(row.id);
    db.delete_messages_from(id, cut_id)
        .await
        .map_err(internal_error)?;
    // The audit log is append-only: every PR this session opened keeps a
    // `rewound` event so its history records that the conversation was
    // cut back while the proposal itself still stands.
    if let Ok(prs) = db.list_session_prs(id).await {
        let detail = format!("history rewound to message {cut_id}");
        for pr in prs {
            let _ = db
                .add_pr_event(pr.id, "rewound", id, "user", None, Some(&detail))
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
    let binding = session
        .repo
        .as_deref()
        .map(crate::session::RepoBinding::parse)
        .transpose()
        .map_err(internal_error)?;
    let issue_store = blocking::unblock(move || {
        crate::issue::resolve_bound_issue_store(binding.as_ref(), &cwd, &workspace_config, db)
    })
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
            Err(error) => unresolved_issue_brief(number, &error),
        };
        briefs.push(brief);
    }
    Ok(briefs)
}

/// The brief shown for an issue whose lookup failed; the failure goes to the
/// daemon log.
pub(super) fn unresolved_issue_brief(
    number: i64,
    error: &dyn std::fmt::Display,
) -> crate::tui::sidebar::IssueBrief {
    tracing::warn!(%error, number, "issue lookup failed");
    crate::tui::sidebar::IssueBrief {
        number,
        title: None,
        closed: false,
    }
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
    Ok(Json(
        session_prs(&state, &id).await.map_err(internal_error)?,
    ))
}

/// The session's PRs with their events and the live checks the daemon's
/// poller last read (see `pr_checks`), newest first.
pub(super) async fn session_prs(
    state: &DaemonState,
    session_id: &str,
) -> Result<Vec<ApiPr>, crate::db::ChatDbError> {
    let rows = state.db().list_session_prs(session_id).await?;
    let mut prs = Vec::with_capacity(rows.len());
    for row in rows {
        let (checks, checks_error) = match state.pr_checks.get(row.id) {
            Some(Ok(report)) => (Some(super::types::ApiPrChecks::of(&report)), None),
            Some(Err(error)) => (None, Some(error)),
            None => (None, None),
        };
        let events = state
            .db()
            .pr_events(row.id)
            .await?
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
            checks,
            checks_error,
            events,
        });
    }
    Ok(prs)
}

/// Atomically set the plan state for a session without touching task content.
///
/// Set the session's yolo switch; it applies to the next approval request.
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
        LlmRole::Configuration => "configuration",
        LlmRole::Tool => "tool",
    }
}

/// Where a new session's name comes from.
#[derive(Debug, Clone, Copy)]
pub(super) enum SessionName<'a> {
    /// The daemon's generator draws one.
    Generate,
    /// A client brings one the daemon issued it.
    Issued(&'a str),
    /// The daemon fronting this cloud home brings one its own generator
    /// drew, so the session has the same name on both daemons.
    Fronting(&'a str),
}

/// Creates the durable DB row and session directory layout for a profile.
///
/// Shared by the regular create-session endpoint and notification callbacks,
/// which open a fresh conversation in the originating profile.
pub(super) async fn create_session_for_profile(
    state: &Arc<DaemonState>,
    name: SessionName<'_>,
    profile_id: Option<&str>,
    model: &str,
) -> SkyResult<String> {
    let db = state.db();
    // Every session is named by the one generator: a name a client brings
    // is one the daemon issued it and no session took yet. A cloud home's
    // sessions are named by the generator of the daemon fronting it.
    let id = match name {
        SessionName::Issued(id) => {
            if !db.session_name_unused(id).await.map_err(internal_error)? {
                return Err(bad_request_error(
                    "session name was not issued by this daemon or is already taken",
                ));
            }
            id.to_string()
        }
        SessionName::Fronting(id) => {
            if db.get_session(id).await.map_err(internal_error)?.is_some() {
                return Err(bad_request_error("session name is already taken"));
            }
            id.to_string()
        }
        SessionName::Generate => db
            .issue_session_name()
            .await
            .map_err(internal_error)?
            .to_string(),
    };
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

/// `POST /api/session-names` — issues a session name for a session the
/// client creates later with `POST /api/sessions`.
pub(super) async fn issue_session_name(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<super::types::IssuedSessionName>> {
    let name = state
        .db()
        .issue_session_name()
        .await
        .map_err(internal_error)?;
    Ok(Json(super::types::IssuedSessionName {
        name: name.to_string(),
    }))
}

pub(super) async fn create_session(
    fronted: super::auth::Fronted,
    Json(req): Json<CreateSessionRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiSession>> {
    let requested_profile_id = req.profile_id.clone();
    let name = match (req.id.as_deref(), fronted) {
        (Some(id), super::auth::Fronted(true)) => SessionName::Fronting(id),
        (Some(id), super::auth::Fronted(false)) => SessionName::Issued(id),
        (None, _) => SessionName::Generate,
    };
    let cloud_account = match &req.cloud {
        Some(_) => Some(cloud_account(&state, requested_profile_id.as_deref()).await?),
        None => None,
    };
    let cloud_project = match (&req.cloud, req.repo.as_deref()) {
        (Some(_), Some(spec)) => Some(cloud_repository(spec)?),
        _ => None,
    };
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
    let id =
        create_session_for_profile(&state, name, resolved_profile_id.as_deref(), &model).await?;
    // A cloud session works in its codespace's workspace, never here.
    if let Some(cwd) = req
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty() && req.cloud.is_none())
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

    let location = match (&req.cloud, &cloud_account) {
        (Some(cloud), Some(account)) => {
            let created = state
                .cloud_sessions
                .create(&id, account, cloud_project.as_ref(), &cloud.machine_type)
                .await;
            match created {
                Ok(location) => Some(location),
                Err(error) => {
                    // No codespace serves it: the session is not kept.
                    let _ = state.db().delete_session(&id).await;
                    return Err(bad_request_error(error.to_string()));
                }
            }
        }
        _ => None,
    };

    // The model a user last chose is this machine's user's choice; a cloud
    // home's sessions carry the choice made on the fronting daemon.
    if !fronted.0 {
        persist_last_model(&state, &model)
            .await
            .map_err(internal_error)?;
    }
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS | UiEventTopics::SESSION_RUNTIMES);

    let mut session = ApiSession::new_empty(id, model, resolved_profile_id);
    session.repo = resolve_repo_binding(req.repo.as_deref()).ok().flatten();
    session.location = location;
    Ok(Json(session))
}

/// The Codespaces account `profile_id` runs cloud sessions as.
async fn cloud_account(
    state: &Arc<DaemonState>,
    profile_id: Option<&str>,
) -> SkyResult<crate::cloud::CodespacesAccount> {
    let profile_id = profile_id.unwrap_or(&state.default_profile_id);
    state
        .db()
        .profile_cloud(profile_id)
        .await
        .map_err(internal_error)?
        .codespaces
        .ok_or_else(|| {
            bad_request_error(
                "this profile has no Codespaces account; set one up with /cloud first",
            )
        })
}

/// The GitHub repository a cloud session binds: a codespace is created
/// from a GitHub repository, never a local checkout.
fn cloud_repository(spec: &str) -> SkyResult<crate::workspace::GitHubRepository> {
    let spec = spec.trim();
    spec.strip_prefix("github:")
        .unwrap_or(spec)
        .parse()
        .map_err(|_| {
            bad_request_error(format!(
                "a cloud session's repository is a GitHub repository (owner/repo), not {spec}"
            ))
        })
}

/// `POST /api/profiles/{id}/cloud/codespaces/catalog` — the machine types
/// a cloud session of the profile with the repository may start on.
pub(super) async fn cloud_catalog(
    params: Params,
    Json(req): Json<super::types::CloudCatalogRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<super::types::CloudCatalogResponse>> {
    let profile_id = params.get("id").map_err(bad_request_error)?.to_string();
    let account = cloud_account(&state, Some(&profile_id)).await?;
    let project = req.repo.as_deref().map(cloud_repository).transpose()?;
    let machines = state
        .cloud_sessions
        .catalog(&account, project.as_ref())
        .await
        .map_err(|error| bad_request_error(error.to_string()))?;
    Ok(Json(super::types::CloudCatalogResponse { machines }))
}

/// Normalize a repo spec into a stored binding: `owner/repo` or a
/// `github:owner/repo` spec binds the GitHub repository itself; an
/// existing path binds that repository's local identity; anything else
/// fails.
pub(super) fn resolve_repo_binding(spec: Option<&str>) -> Result<Option<String>, String> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    // The stored `local:<identity>` form is the daemon's own identity format
    // for a local repository, which clients echo back from a session's group.
    if spec.starts_with("local:") {
        return crate::session::RepoBinding::parse(spec)
            .map(|binding| Some(binding.to_stored()))
            .map_err(|error| error.to_string());
    }
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
    // So is its codespace: a failed deletion keeps the session, so the
    // delete can be retried instead of leaving the machine running.
    state
        .cloud_sessions
        .delete(&id)
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

/// `GET /api/sessions/{id}/queue`: the session's pending user messages —
/// the immediate one first, then queued oldest-first.
pub(super) async fn get_session_queue(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<super::types::SessionQueueResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let runtime = state
        .runtime_service
        .get(&id)
        .await
        .map_err(internal_error)?;
    let mut entries = Vec::new();
    entries.extend(runtime.pending_immediate);
    entries.extend(runtime.queued_messages);
    Ok(Json(super::types::SessionQueueResponse { entries }))
}

/// `DELETE /api/sessions/{id}/queue/{entryId}`: drop one pending entry by
/// its id, whichever kind it is.
pub(super) async fn remove_queue_entry(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<super::types::SessionRuntimeResponse>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let entry_id = params
        .get("entryId")
        .map_err(bad_request_error)?
        .parse::<i64>()
        .map_err(|_| bad_request_error("queue entry id must be a number"))?;
    let runtime = state
        .runtime_service
        .remove_pending_entry(&id, entry_id)
        .await
        .map_err(|error| match error {
            crate::db::ChatDbError::RowNotFound(message) => {
                skyzen::Error::msg(message).set_status(skyzen::StatusCode::NOT_FOUND)
            }
            error => internal_error(error),
        })?;
    Ok(Json(super::types::SessionRuntimeResponse::from(runtime)))
}

/// `PATCH /api/sessions/{id}`: apply any subset of session fields. Every
/// field is validated before anything is written; the response is the same
/// `ApiSession` `GET` returns, with a fresh `?after=` cursor.
pub(super) async fn patch_session(
    params: Params,
    Json(req): Json<super::types::SessionPatch>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<ApiSession>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();
    let db = state.db();
    let session = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| session_not_found_error(&id))?;

    // ── Validate everything up front: nothing is written until the whole
    // patch is known to apply. ──
    if let Some(model) = req.model.as_deref()
        && resolve_default_model(&state.config_snapshot(), Some(model.to_string())).is_none()
    {
        return Err(bad_request_error(format!("unknown model {model}")));
    }
    if let Some(super::types::NullableStringPatch::Set(spec)) = req.repo.as_ref() {
        resolve_repo_binding(Some(spec.as_str())).map_err(bad_request_error)?;
    }
    let next_pinned = req.pinned.unwrap_or(session.pinned);
    let next_archived = req.archived.unwrap_or(session.archived);
    let next_folder = match req.folder_id.as_ref() {
        Some(super::types::NullableStringPatch::Null) => None,
        Some(super::types::NullableStringPatch::Set(folder)) => Some(folder.clone()),
        None => session.folder_id.clone(),
    };
    if let Some(folder_id) = next_folder.as_deref() {
        if next_archived {
            return Err(bad_request_error(
                "cannot move an archived session into a pinned folder",
            ));
        }
        if !next_pinned {
            return Err(bad_request_error(
                "cannot move an unpinned session into a pinned folder",
            ));
        }
        let profile_id = session.profile_id.as_deref().ok_or_else(|| {
            super::error::internal_error_message("session has no profile for folders")
        })?;
        let folders = db.list_folders(profile_id).await.map_err(internal_error)?;
        if !folders.iter().any(|folder| folder.id == folder_id) {
            return Err(bad_request_error(format!("unknown folder {folder_id}")));
        }
    }

    // ── Apply. ──
    let list_visible_changed = req.title.is_some()
        || req.model.is_some()
        || req.reasoning_effort.is_some()
        || req.repo.is_some()
        || req.pinned.is_some()
        || req.archived.is_some()
        || req.folder_id.is_some();

    let effort_patch = req.reasoning_effort.as_ref().map(|patch| match patch {
        super::types::NullableStringPatch::Null => None,
        super::types::NullableStringPatch::Set(value) => Some(value.as_str()),
    });
    if req.title.is_some() || req.model.is_some() || effort_patch.is_some() {
        db.update_session(
            &id,
            req.title.as_deref(),
            req.model.as_deref(),
            effort_patch,
        )
        .await
        .map_err(internal_error)?;
    }
    if let Some(model) = req.model.as_deref()
        && session.model.as_deref() != Some(model)
    {
        persist_last_model(&state, model)
            .await
            .map_err(internal_error)?;
    }
    if let Some(super::types::NullableStringPatch::Set(effort)) = req.reasoning_effort.as_ref() {
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
    if let Some(patch) = req.repo.as_ref() {
        let next_repo = match patch {
            super::types::NullableStringPatch::Null => None,
            super::types::NullableStringPatch::Set(spec) => {
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
    if let Some(pinned) = req.pinned
        && pinned != session.pinned
    {
        {
            if pinned {
                db.pin_session(&id)
                    .await
                    .map_err(super::profiles::map_profile_error)?;
            } else {
                db.unpin_session(&id).await.map_err(internal_error)?;
            }
        }
    }
    if let Some(folder_patch) = req.folder_id.as_ref()
        && next_folder.as_deref() != session.folder_id.as_deref()
    {
        let target = match folder_patch {
            super::types::NullableStringPatch::Null => None,
            super::types::NullableStringPatch::Set(folder) => Some(folder.as_str()),
        };
        db.move_session_to_folder(&id, target)
            .await
            .map_err(super::profiles::map_profile_error)?;
    }
    if let Some(archived) = req.archived
        && archived != session.archived
    {
        {
            if archived {
                db.archive_session(&id).await.map_err(internal_error)?;
            } else {
                db.unarchive_session(&id).await.map_err(internal_error)?;
            }
        }
    }
    if let Some(yolo) = req.yolo {
        state
            .runtime_service
            .set_yolo(&id, yolo)
            .await
            .map_err(internal_error)?;
    }
    if let Some(plan_state) = req.plan_state {
        state
            .runtime_service
            .set_plan_state(&id, plan_state)
            .await
            .map_err(internal_error)?;
    }

    if list_visible_changed {
        state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
    }

    // Respond like `get_session`: cursor first, then the fresh row.
    let cursor = state.session_cursor(&id);
    let db_session = db
        .get_session(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| session_not_found_error(&id))?;
    let mut summary = crate::session::SessionSummary::from_db_model(db_session);
    summary.runtime = db
        .get_session_runtime(&id)
        .await
        .map_err(internal_error)?
        .unwrap_or_default();
    let model = resolve_session_model(&state, &id, summary.model.clone())?;
    let mut patched = ApiSession::from_summary(summary, model);
    patched.cursor = Some(cursor);
    Ok(Json(patched))
}
