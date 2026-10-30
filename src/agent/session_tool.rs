use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;

use aither_core::llm::{Tool, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::db::{ChatDb, IndexedMessageRow};
use crate::repo_search::indexer::{IndexerError, SESSIONS_COLLECTION, SearchIndexer, parse_message_doc_id};
use crate::repo_search::{CollectionBackend, RepoSearchCollections};

/// Session index as the `session` command queries it.
pub type SessionSearchIndex = SearchIndexer<RepoSearchCollections>;

/// Output shape for `session search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionOutputMode {
    /// Return grouped session summaries.
    Sessions,
    /// Return matching messages flat.
    Messages,
}

/// Search May sessions by what was discussed, or inspect, read and list them.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SessionArgs {
    /// Optional command-form input. Supported:
    /// - `session <id> search <prompt>`
    /// - `session search <prompt>`
    /// - `session inspect <id>`
    /// - `session messages <id>`
    /// - `session list`
    #[serde(default)]
    pub command: Option<String>,

    /// What to find, described in natural language.
    pub query: String,

    /// Output mode: `sessions` (aggregated) or `messages`.
    #[serde(default = "default_output_mode")]
    pub output_mode: SessionOutputMode,

    /// Max returned rows.
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// Skip N rows after ranking.
    #[serde(default)]
    pub offset: usize,

    /// Filter by exact session id.
    #[serde(default)]
    pub session_id: Option<String>,

    /// Exclude exact session id.
    #[serde(default)]
    pub exclude_session_id: Option<String>,

    /// Filter by model id.
    #[serde(default)]
    pub model: Option<String>,

    /// Filter by exact profile id.
    #[serde(default)]
    pub profile_id: Option<String>,

    /// Filter by message role.
    #[serde(default)]
    pub role: Option<String>,

    /// Filter by timestamp >= value (RFC3339-like string).
    #[serde(default)]
    pub since: Option<String>,

    /// Filter by timestamp <= value (RFC3339-like string).
    #[serde(default)]
    pub until: Option<String>,

    /// Filter by session title contains text.
    #[serde(default)]
    pub title_contains: Option<String>,

    /// Include full message content in output.
    #[serde(default)]
    pub include_content: bool,

    /// Include current session. Default false.
    #[serde(default)]
    pub include_current: bool,

    /// Include archived sessions. Default false.
    #[serde(default)]
    pub include_archived: bool,
}

const fn default_output_mode() -> SessionOutputMode {
    SessionOutputMode::Sessions
}

const fn default_limit() -> usize {
    20
}

/// `session` tool: natural-language search and inspection over session history.
#[derive(Debug, Clone)]
pub struct SessionTool {
    may_dir: PathBuf,
    current_session_id: String,
    index: Option<SessionSearchIndex>,
}

#[derive(Debug, thiserror::Error)]
enum SessionToolError {
    #[error("database error: {0}")]
    Database(#[from] crate::db::ChatDbError),
    #[error("session command '{command}' contains invalid shell quoting: {source}")]
    CommandWords {
        command: String,
        #[source]
        source: shell_words::ParseError,
    },
    #[error("session search is unavailable: {0}")]
    Index(#[from] IndexerError),
    #[error("session search is unavailable: {0}")]
    RepoSearch(#[from] crate::repo_search::RepoSearchError),
    #[error("this session has no search index")]
    NoIndex,
    #[error("{command_name} command must not be empty")]
    EmptyCommand { command_name: String },
    #[error("{command_name} command must include a search prompt")]
    MissingCommandPrompt { command_name: String },
    #[error("invalid session command: missing prompt after 'search'")]
    MissingSearchPrompt,
    #[error("session search takes a description of what to find; use `session list` to list sessions by filters")]
    WildcardQuery,
    #[error("session action '{action}' requires a session id")]
    MissingActionSessionId { action: &'static str },
    #[error("session '{session_id}' was not found")]
    SessionNotFound { session_id: String },
    #[error(
        "every matching message is already present in the current context window; refine the query to recall older or other-session content"
    )]
    AllResultsAlreadyInContext,
}

#[derive(Debug, Clone)]
struct SearchDefaults {
    current_session_id: String,
}

#[derive(Debug, Clone)]
enum SessionExecutionPlan {
    Search(SessionArgs),
    Inspect(SessionInspectRequest),
    Messages(SessionMessagesRequest),
    List(SessionListRequest),
}

#[derive(Debug, Clone)]
struct SessionInspectRequest {
    session_id: String,
}

#[derive(Debug, Clone)]
struct SessionMessagesRequest {
    session_id: String,
}

#[derive(Debug, Clone)]
struct SessionListRequest {
    limit: usize,
    offset: usize,
    include_current: bool,
    include_archived: bool,
    current_session_id: String,
    profile_id: Option<String>,
    model: Option<String>,
    title_contains: Option<String>,
    since: Option<String>,
    until: Option<String>,
}

enum SessionCommandInput {
    RawQuery,
    Tokens(Vec<String>),
}

impl SessionTool {
    /// Bind the tool to a session; `index` serves `session search`.
    #[must_use]
    pub const fn new(
        may_dir: PathBuf,
        current_session_id: String,
        index: Option<SessionSearchIndex>,
    ) -> Self {
        Self {
            may_dir,
            current_session_id,
            index,
        }
    }

    fn db_path(&self) -> PathBuf {
        self.may_dir.join("history.db")
    }

    async fn normalize_args(
        &self,
        mut args: SessionArgs,
    ) -> Result<SessionExecutionPlan, SessionToolError> {
        let defaults = SearchDefaults {
            current_session_id: self.current_session_id.clone(),
        };

        if let Some(command) = args.command.take() {
            let tokens = split_command_form_tokens(&command, "session")?;
            let plan = build_execution_plan(args, SessionCommandInput::Tokens(tokens), &defaults)?;
            return resolve_implicit_session_lookup(self.db_path(), plan).await;
        }

        let plan = build_execution_plan(args, SessionCommandInput::RawQuery, &defaults)?;
        resolve_implicit_session_lookup(self.db_path(), plan).await
    }
}

impl Tool for SessionTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("session")
    }

    type Arguments = SessionArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        arguments: Self::Arguments,
        _cx: aither_core::llm::ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let plan = self.normalize_args(arguments).await?;
        execute_plan(
            self.db_path(),
            self.index.as_ref(),
            self.current_session_id.clone(),
            plan,
        )
        .await
        .map_err(Into::into)
    }
}

fn apply_search_defaults(args: &mut SessionArgs, defaults: &SearchDefaults) {
    if !args.include_current && args.exclude_session_id.is_none() {
        args.exclude_session_id = Some(defaults.current_session_id.clone());
    }
}

fn normalize_common_args(mut args: SessionArgs) -> SessionArgs {
    args.limit = args.limit.clamp(1, 200);
    args
}

async fn execute_plan(
    db_path: PathBuf,
    index: Option<&SessionSearchIndex>,
    current_session_id: String,
    plan: SessionExecutionPlan,
) -> Result<ToolResult, SessionToolError> {
    match plan {
        SessionExecutionPlan::Search(args) => {
            let index = index.ok_or(SessionToolError::NoIndex)?;
            execute_search(index, &current_session_id, args).await
        }
        SessionExecutionPlan::Inspect(request) => execute_inspect(db_path, request).await,
        SessionExecutionPlan::Messages(request) => execute_messages(db_path, request).await,
        SessionExecutionPlan::List(request) => execute_list(db_path, request).await,
    }
}

async fn execute_search(
    index: &SessionSearchIndex,
    current_session_id: &str,
    args: SessionArgs,
) -> Result<ToolResult, SessionToolError> {
    let query = args.query.trim();
    if query.is_empty() || query == "*" {
        return Err(SessionToolError::WildcardQuery);
    }
    // Messages written since the last drain become searchable first.
    index.drain().await?;
    let hits = index.backend().search(SESSIONS_COLLECTION, query).await?;
    let ranked: Vec<(i64, f64)> = hits
        .iter()
        .filter_map(|hit| {
            parse_message_doc_id(&hit.id).map(|(_, message_id)| (message_id, hit.score))
        })
        .collect();
    let ids: Vec<i64> = ranked.iter().map(|(id, _)| *id).collect();
    let mut by_id: HashMap<i64, IndexedMessageRow> = index
        .db()
        .indexed_messages(&ids)
        .await?
        .into_iter()
        .map(|row| (row.message_id, row))
        .collect();
    let rows: Vec<SessionMatchRow> = ranked
        .into_iter()
        .filter_map(|(id, score)| by_id.remove(&id).map(|row| SessionMatchRow::from_indexed(row, score)))
        .filter(|row| row_matches_session_filters(row, &args))
        .collect();

    let total_matches = rows.len();
    let rows = drop_rows_already_in_context(index.db(), current_session_id, rows).await?;
    if total_matches > 0 && rows.is_empty() {
        return Err(SessionToolError::AllResultsAlreadyInContext);
    }
    Ok(finalize_rows_with_total(&rows, total_matches, &args))
}

/// Removes current-session hits whose content already appears in the exact
/// persisted context window from the latest checkpoint.
async fn drop_rows_already_in_context(
    db: &ChatDb,
    current_session_id: &str,
    rows: Vec<SessionMatchRow>,
) -> Result<Vec<SessionMatchRow>, SessionToolError> {
    if !rows.iter().any(|row| row.session_id == current_session_id) {
        return Ok(rows);
    }
    let Some(snapshot) = db.get_session_context_snapshot(current_session_id).await? else {
        return Ok(rows);
    };
    Ok(filter_rows_against_messages(
        current_session_id,
        &snapshot.checkpoint.context_window.messages,
        rows,
    ))
}

fn filter_rows_against_messages(
    current_session_id: &str,
    window: &[aither_core::llm::Message],
    rows: Vec<SessionMatchRow>,
) -> Vec<SessionMatchRow> {
    rows.into_iter()
        .filter(|row| {
            if row.session_id != current_session_id {
                return true;
            }
            let content = row.content.trim();
            content.is_empty()
                || !window
                    .iter()
                    .any(|message| message.content().contains(content))
        })
        .collect()
}

#[derive(Debug, Clone)]
struct SessionSummaryView {
    session_id: String,
    title: String,
    model: String,
    profile_id: Option<String>,
    archived: bool,
    created_at: String,
    updated_at: String,
    message_count: i32,
    preview: String,
}

impl From<crate::db::entity::sessions::Model> for SessionSummaryView {
    fn from(value: crate::db::entity::sessions::Model) -> Self {
        Self {
            session_id: value.id,
            title: value.title.unwrap_or_default(),
            model: value.model.unwrap_or_default(),
            profile_id: value.profile_id,
            archived: value.archived,
            created_at: value.created_at,
            updated_at: value.updated_at,
            message_count: value.message_count,
            preview: value.preview.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone)]
struct SessionMessageView {
    message_id: i32,
    role: String,
    timestamp: Option<String>,
    content: String,
}

impl From<crate::db::entity::messages::Model> for SessionMessageView {
    fn from(value: crate::db::entity::messages::Model) -> Self {
        Self {
            message_id: value.id,
            role: value.role,
            timestamp: value.timestamp,
            content: value.content,
        }
    }
}

/// What the model reads for `session inspect`.
#[derive(Debug, Template)]
#[template(path = "session_inspect.txt", escape = "none")]
struct SessionInspectText {
    session: SessionSummaryView,
    final_assistant_message: Option<SessionMessageView>,
    final_user_message: Option<SessionMessageView>,
    latest_message: Option<SessionMessageView>,
}

/// What the model reads for `session messages`.
#[derive(Debug, Template)]
#[template(path = "session_messages.txt", escape = "none")]
struct SessionMessagesText {
    session: SessionSummaryView,
    messages: Vec<SessionMessageView>,
}

/// What the model reads for `session list`.
#[derive(Debug, Template)]
#[template(path = "session_list.txt", escape = "none")]
struct SessionListText {
    total_matches: usize,
    results: Vec<SessionSummaryView>,
}

/// What the model reads for a message-mode `session search`.
#[derive(Debug, Template)]
#[template(path = "session_search_messages.txt", escape = "none")]
struct SessionSearchMessagesText {
    query: String,
    total_matches: usize,
    items: Vec<MessageResult>,
}

/// What the model reads for a session-mode `session search`.
#[derive(Debug, Template)]
#[template(path = "session_search_sessions.txt", escape = "none")]
struct SessionSearchSessionsText {
    query: String,
    total_matches: usize,
    items: Vec<SessionResult>,
}

async fn execute_inspect(
    db_path: PathBuf,
    request: SessionInspectRequest,
) -> Result<ToolResult, SessionToolError> {
    let db = ChatDb::open(&db_path).await?;
    let session = db.get_session(&request.session_id).await?.ok_or_else(|| {
        SessionToolError::SessionNotFound {
            session_id: request.session_id.clone(),
        }
    })?;
    let messages = db.get_messages(&request.session_id).await?;
    let latest_message = messages.last().cloned().map(SessionMessageView::from);
    let final_assistant_message = messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant")
        .cloned()
        .map(SessionMessageView::from);
    let final_user_message = messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .cloned()
        .map(SessionMessageView::from);

    let text = SessionInspectText {
        session: SessionSummaryView::from(session),
        final_assistant_message,
        final_user_message,
        latest_message,
    }
    .render()
    .expect("session inspect template must render");
    Ok(ToolResult::text(text))
}

async fn execute_messages(
    db_path: PathBuf,
    request: SessionMessagesRequest,
) -> Result<ToolResult, SessionToolError> {
    let db = ChatDb::open(&db_path).await?;
    let session = db.get_session(&request.session_id).await?.ok_or_else(|| {
        SessionToolError::SessionNotFound {
            session_id: request.session_id.clone(),
        }
    })?;
    let messages = db
        .get_messages(&request.session_id)
        .await?
        .into_iter()
        .map(SessionMessageView::from)
        .collect::<Vec<_>>();

    let text = SessionMessagesText {
        session: SessionSummaryView::from(session),
        messages,
    }
    .render()
    .expect("session messages template must render");
    Ok(ToolResult::text(text))
}

async fn execute_list(
    db_path: PathBuf,
    request: SessionListRequest,
) -> Result<ToolResult, SessionToolError> {
    let db = ChatDb::open(&db_path).await?;
    let lowered_title = request.title_contains.as_deref().map(str::to_lowercase);
    let mut sessions = db
        .list_sessions()
        .await?
        .into_iter()
        .filter(|session| {
            if !request.include_current && session.id == request.current_session_id {
                return false;
            }
            if !request.include_archived && session.archived {
                return false;
            }
            if let Some(profile_id) = request.profile_id.as_deref()
                && session.profile_id.as_deref() != Some(profile_id)
            {
                return false;
            }
            if let Some(model) = request.model.as_deref()
                && session.model.as_deref() != Some(model)
            {
                return false;
            }
            if let Some(title_contains) = lowered_title.as_deref() {
                let title = session.title.as_deref().unwrap_or_default().to_lowercase();
                if !title.contains(title_contains) {
                    return false;
                }
            }
            if let Some(since) = request.since.as_deref()
                && session.updated_at.as_str() < since
            {
                return false;
            }
            if let Some(until) = request.until.as_deref()
                && session.updated_at.as_str() > until
            {
                return false;
            }
            true
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    let total_matches = sessions.len();
    let results = sessions
        .into_iter()
        .skip(request.offset)
        .take(request.limit)
        .map(SessionSummaryView::from)
        .collect::<Vec<_>>();

    let text = SessionListText {
        total_matches,
        results,
    }
    .render()
    .expect("session list template must render");
    Ok(ToolResult::text(text))
}

fn split_command_form_tokens(
    command: &str,
    command_name: &str,
) -> Result<Vec<String>, SessionToolError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(SessionToolError::EmptyCommand {
            command_name: command_name.to_string(),
        });
    }

    let mut prefix = String::with_capacity(command_name.len() + 1);
    prefix.push_str(command_name);
    prefix.push(' ');
    let body = trimmed
        .strip_prefix(prefix.as_str())
        .unwrap_or(trimmed)
        .trim();
    if body.is_empty() {
        return Err(SessionToolError::MissingCommandPrompt {
            command_name: command_name.to_string(),
        });
    }

    shell_words::split(body).map_err(|source| SessionToolError::CommandWords {
        command: command_name.to_string(),
        source,
    })
}

fn build_execution_plan(
    mut args: SessionArgs,
    input: SessionCommandInput,
    defaults: &SearchDefaults,
) -> Result<SessionExecutionPlan, SessionToolError> {
    match input {
        SessionCommandInput::Tokens(tokens) => {
            if let Some(plan) = plan_from_tokens(&tokens, &args, defaults)? {
                return Ok(plan);
            }
            args.query = tokens.join(" ");
        }
        SessionCommandInput::RawQuery => {
            if let Some(plan) = plan_from_raw_query(&args, defaults) {
                return Ok(plan);
            }
        }
    }

    apply_search_defaults(&mut args, defaults);
    Ok(SessionExecutionPlan::Search(normalize_common_args(args)))
}

async fn resolve_implicit_session_lookup(
    db_path: PathBuf,
    plan: SessionExecutionPlan,
) -> Result<SessionExecutionPlan, SessionToolError> {
    let SessionExecutionPlan::Search(args) = plan else {
        return Ok(plan);
    };
    let Some(session_id) = implicit_session_lookup_candidate(&args) else {
        return Ok(SessionExecutionPlan::Search(args));
    };
    let db = ChatDb::open(&db_path).await?;
    if db.get_session(session_id).await?.is_some() {
        return Ok(SessionExecutionPlan::Inspect(SessionInspectRequest {
            session_id: session_id.to_string(),
        }));
    }
    Ok(SessionExecutionPlan::Search(args))
}

fn implicit_session_lookup_candidate(args: &SessionArgs) -> Option<&str> {
    if args.session_id.is_some() {
        return None;
    }
    let candidate = args.query.trim();
    if candidate.is_empty() || candidate == "*" || candidate.chars().any(char::is_whitespace) {
        return None;
    }
    Some(candidate)
}

fn plan_from_tokens(
    tokens: &[String],
    args: &SessionArgs,
    defaults: &SearchDefaults,
) -> Result<Option<SessionExecutionPlan>, SessionToolError> {
    let Some(first) = tokens.first().map(String::as_str) else {
        return Ok(None);
    };

    if let [session_id, keyword, prompt_tokens @ ..] = tokens
        && keyword == "search"
    {
        let prompt = prompt_tokens.join(" ");
        if prompt.is_empty() {
            return Err(SessionToolError::MissingSearchPrompt);
        }
        let mut next_args = args.clone();
        next_args.session_id = Some(session_id.clone());
        next_args.query = prompt;
        apply_search_defaults(&mut next_args, defaults);
        return Ok(Some(SessionExecutionPlan::Search(normalize_common_args(
            next_args,
        ))));
    }

    match first {
        "search" => {
            let prompt = tokens
                .iter()
                .skip(1)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            if prompt.is_empty() {
                return Err(SessionToolError::MissingSearchPrompt);
            }
            let mut next_args = args.clone();
            next_args.query = prompt;
            apply_search_defaults(&mut next_args, defaults);
            Ok(Some(SessionExecutionPlan::Search(normalize_common_args(
                next_args,
            ))))
        }
        "inspect" | "show" | "read" | "get" | "view" if tokens.len() == 2 => {
            let session_id = tokens
                .get(1)
                .ok_or(SessionToolError::MissingActionSessionId { action: "inspect" })?;
            Ok(Some(SessionExecutionPlan::Inspect(SessionInspectRequest {
                session_id: session_id.clone(),
            })))
        }
        "messages" if tokens.len() == 2 => {
            let session_id = tokens
                .get(1)
                .ok_or(SessionToolError::MissingActionSessionId { action: "messages" })?;
            Ok(Some(SessionExecutionPlan::Messages(
                SessionMessagesRequest {
                    session_id: session_id.clone(),
                },
            )))
        }
        "list" if tokens.len() == 1 => Ok(Some(SessionExecutionPlan::List(SessionListRequest {
            limit: args.limit.clamp(1, 200),
            offset: args.offset,
            include_current: args.include_current,
            include_archived: args.include_archived,
            current_session_id: defaults.current_session_id.clone(),
            profile_id: args.profile_id.clone(),
            model: args.model.clone(),
            title_contains: args.title_contains.clone(),
            since: args.since.clone(),
            until: args.until.clone(),
        }))),
        _ => Ok(None),
    }
}

fn plan_from_raw_query(
    args: &SessionArgs,
    defaults: &SearchDefaults,
) -> Option<SessionExecutionPlan> {
    let trimmed = args.query.trim();
    if trimmed.is_empty() {
        return None;
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let first = tokens.first().copied()?;

    match first {
        "inspect" | "show" | "read" | "get" | "view" if tokens.len() == 2 => {
            Some(SessionExecutionPlan::Inspect(SessionInspectRequest {
                session_id: tokens[1].to_string(),
            }))
        }
        "messages" if tokens.len() == 2 => {
            Some(SessionExecutionPlan::Messages(SessionMessagesRequest {
                session_id: tokens[1].to_string(),
            }))
        }
        "list" if tokens.len() == 1 => Some(SessionExecutionPlan::List(SessionListRequest {
            limit: args.limit.clamp(1, 200),
            offset: args.offset,
            include_current: args.include_current,
            include_archived: args.include_archived,
            current_session_id: defaults.current_session_id.clone(),
            profile_id: args.profile_id.clone(),
            model: args.model.clone(),
            title_contains: args.title_contains.clone(),
            since: args.since.clone(),
            until: args.until.clone(),
        })),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct SessionMatchRow {
    session_id: String,
    profile_id: String,
    title: String,
    model: String,
    archived: bool,
    role: String,
    content: String,
    preview: String,
    timestamp: String,
    updated_at: String,
    message_id: i64,
    message_count: i32,
    score: f64,
}

impl SessionMatchRow {
    fn from_indexed(row: IndexedMessageRow, score: f64) -> Self {
        Self {
            session_id: row.session_id,
            profile_id: row.profile_id.unwrap_or_default(),
            title: row.title.unwrap_or_default(),
            model: row.model.unwrap_or_default(),
            archived: row.archived,
            role: row.role,
            content: row.content,
            preview: row.preview.unwrap_or_default(),
            timestamp: row.timestamp.unwrap_or_default(),
            updated_at: row.updated_at,
            message_id: row.message_id,
            message_count: row.message_count,
            score,
        }
    }
}

#[derive(Debug, Clone)]
struct MessageResult {
    session_id: String,
    title: String,
    model: String,
    role: String,
    timestamp: String,
    message_id: i64,
    score: f64,
    snippet: String,
    content: Option<String>,
}

impl MessageResult {
    fn from_row(row: SessionMatchRow, include_content: bool) -> Self {
        let snippet = if row.content.is_empty() {
            truncate_text(&row.preview, 240)
        } else {
            truncate_text(&row.content, 240)
        };

        Self {
            session_id: row.session_id,
            title: row.title,
            model: row.model,
            role: row.role,
            timestamp: row.timestamp,
            message_id: row.message_id,
            score: (row.score * 100.0).round() / 100.0,
            snippet,
            content: include_content.then_some(row.content),
        }
    }
}

#[derive(Debug, Clone)]
struct SessionResult {
    session_id: String,
    title: String,
    model: String,
    updated_at: String,
    message_count: i32,
    match_count: usize,
    top_score: f64,
    highlights: Vec<MessageResult>,
}

fn row_matches_session_filters(row: &SessionMatchRow, args: &SessionArgs) -> bool {
    if let Some(session_id) = &args.session_id
        && &row.session_id != session_id
    {
        return false;
    }
    if let Some(excluded) = &args.exclude_session_id
        && &row.session_id == excluded
    {
        return false;
    }
    if let Some(model) = &args.model
        && &row.model != model
    {
        return false;
    }
    if let Some(profile_id) = &args.profile_id
        && &row.profile_id != profile_id
    {
        return false;
    }
    if let Some(role) = &args.role
        && &row.role != role
    {
        return false;
    }
    if let Some(since) = &args.since
        && &row.timestamp < since
    {
        return false;
    }
    if let Some(until) = &args.until
        && &row.timestamp > until
    {
        return false;
    }
    if let Some(title_contains) = &args.title_contains
        && !row
            .title
            .to_lowercase()
            .contains(&title_contains.to_lowercase())
    {
        return false;
    }
    args.include_archived || !row.archived
}

fn truncate_text(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn finalize_rows_with_total(
    rows: &[SessionMatchRow],
    total_matches: usize,
    args: &SessionArgs,
) -> ToolResult {
    let start = args.offset.min(rows.len());
    let end = (start + args.limit).min(rows.len());
    let rows = rows[start..end].to_vec();

    let text = if args.output_mode == SessionOutputMode::Messages {
        let items: Vec<MessageResult> = rows
            .into_iter()
            .map(|row| MessageResult::from_row(row, args.include_content))
            .collect();
        SessionSearchMessagesText {
            query: args.query.clone(),
            total_matches,
            items,
        }
        .render()
        .expect("session search template must render")
    } else {
        let mut grouped: HashMap<String, SessionResult> = HashMap::new();
        for row in rows {
            let entry = grouped
                .entry(row.session_id.clone())
                .or_insert_with(|| SessionResult {
                    session_id: row.session_id.clone(),
                    title: row.title.clone(),
                    model: row.model.clone(),
                    updated_at: row.updated_at.clone(),
                    message_count: row.message_count,
                    match_count: 0,
                    top_score: row.score,
                    highlights: Vec::new(),
                });
            entry.match_count += 1;
            if row.score > entry.top_score {
                entry.top_score = row.score;
            }
            if entry.highlights.len() < 3 {
                entry
                    .highlights
                    .push(MessageResult::from_row(row, args.include_content));
            }
        }

        let mut items: Vec<SessionResult> = grouped.into_values().collect();
        items.sort_by(|a, b| {
            b.top_score
                .partial_cmp(&a.top_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });

        SessionSearchSessionsText {
            query: args.query.clone(),
            total_matches,
            items,
        }
        .render()
        .expect("session search template must render")
    };

    ToolResult::text(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::future::block_on;
    use tempfile::tempdir;

    fn base_args() -> SessionArgs {
        SessionArgs {
            command: None,
            query: "*".to_string(),
            output_mode: SessionOutputMode::Sessions,
            limit: 20,
            offset: 0,
            session_id: None,
            exclude_session_id: None,
            model: None,
            profile_id: None,
            role: None,
            since: None,
            until: None,
            title_contains: None,
            include_content: false,
            include_current: false,
            include_archived: false,
        }
    }

    #[test]
    fn parses_scoped_session_command_form() {
        let tokens =
            split_command_form_tokens("session abc123 search investigate failures", "session")
                .expect("command form should parse");
        let plan = plan_from_tokens(
            &tokens,
            &base_args(),
            &SearchDefaults {
                current_session_id: "current".to_string(),
            },
        )
        .expect("plan should parse")
        .expect("search plan should be produced");
        let SessionExecutionPlan::Search(args) = plan else {
            panic!("expected search plan");
        };
        assert_eq!(args.session_id.as_deref(), Some("abc123"));
        assert_eq!(args.query, "investigate failures");
    }

    #[test]
    fn parses_global_session_command_form() {
        let tokens = split_command_form_tokens("session recall onboarding decision", "session")
            .expect("global command form should parse");
        let plan = build_execution_plan(
            base_args(),
            SessionCommandInput::Tokens(tokens),
            &SearchDefaults {
                current_session_id: "current".to_string(),
            },
        )
        .expect("search plan should parse");
        let SessionExecutionPlan::Search(args) = plan else {
            panic!("expected search plan");
        };
        assert_eq!(args.session_id, None);
        assert_eq!(args.query, "recall onboarding decision");
    }

    #[test]
    fn parses_quoted_scoped_session_command_form() {
        let tokens = split_command_form_tokens(
            "session abc123 search \"investigate spaced failures\"",
            "session",
        )
        .expect("quoted scoped command form should parse");
        let plan = plan_from_tokens(
            &tokens,
            &base_args(),
            &SearchDefaults {
                current_session_id: "current".to_string(),
            },
        )
        .expect("plan should parse")
        .expect("search plan should be produced");
        let SessionExecutionPlan::Search(args) = plan else {
            panic!("expected search plan");
        };
        assert_eq!(args.session_id.as_deref(), Some("abc123"));
        assert_eq!(args.query, "investigate spaced failures");
    }

    #[test]
    fn parses_quoted_global_session_command_form() {
        let tokens = split_command_form_tokens("session \"recall onboarding decision\"", "session")
            .expect("quoted global command form should parse");
        let plan = build_execution_plan(
            base_args(),
            SessionCommandInput::Tokens(tokens),
            &SearchDefaults {
                current_session_id: "current".to_string(),
            },
        )
        .expect("search plan should parse");
        let SessionExecutionPlan::Search(args) = plan else {
            panic!("expected search plan");
        };
        assert_eq!(args.session_id, None);
        assert_eq!(args.query, "recall onboarding decision");
    }

    #[test]
    fn rejects_missing_prompt_in_scoped_command() {
        let tokens =
            split_command_form_tokens("session abc123 search", "session").expect("tokens parse");
        let err = plan_from_tokens(
            &tokens,
            &base_args(),
            &SearchDefaults {
                current_session_id: "current".to_string(),
            },
        )
        .expect_err("missing prompt must fail");
        assert!(
            err.to_string().contains("missing prompt"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parses_inspect_command_form() {
        let tokens = split_command_form_tokens("session inspect abc123", "session")
            .expect("inspect command form should parse");
        let plan = plan_from_tokens(
            &tokens,
            &base_args(),
            &SearchDefaults {
                current_session_id: "current".to_string(),
            },
        )
        .expect("plan should parse")
        .expect("inspect plan should be produced");
        let SessionExecutionPlan::Inspect(request) = plan else {
            panic!("expected inspect plan");
        };
        assert_eq!(request.session_id, "abc123");
    }

    #[test]
    fn parses_raw_query_inspect_action() {
        let mut args = base_args();
        args.query = "inspect abc123".to_string();
        let plan = plan_from_raw_query(
            &args,
            &SearchDefaults {
                current_session_id: "current".to_string(),
            },
        )
        .expect("inspect plan should be produced");
        let SessionExecutionPlan::Inspect(request) = plan else {
            panic!("expected inspect plan");
        };
        assert_eq!(request.session_id, "abc123");
    }

    #[test]
    fn single_token_query_becomes_implicit_inspect_when_session_exists() {
        block_on(async {
            let temp = tempdir().expect("tempdir");
            let db_path = temp.path().join("history.db");
            let db = ChatDb::open(&db_path).await.expect("db should open");
            db.create_session("abc123", Some("session"), Some("gpt-5"))
                .await
                .expect("session should be created");

            let mut args = base_args();
            args.query = "abc123".to_string();

            let plan = resolve_implicit_session_lookup(db_path, SessionExecutionPlan::Search(args))
                .await
                .expect("implicit inspect should resolve");

            let SessionExecutionPlan::Inspect(request) = plan else {
                panic!("expected inspect plan");
            };
            assert_eq!(request.session_id, "abc123");
        });
    }

    #[test]
    fn single_token_query_stays_search_when_session_missing() {
        block_on(async {
            let temp = tempdir().expect("tempdir");
            let db_path = temp.path().join("history.db");
            let _db = ChatDb::open(&db_path).await.expect("db should open");

            let mut args = base_args();
            args.query = "abc123".to_string();

            let plan = resolve_implicit_session_lookup(db_path, SessionExecutionPlan::Search(args))
                .await
                .expect("search should remain valid");

            let SessionExecutionPlan::Search(args) = plan else {
                panic!("expected search plan");
            };
            assert_eq!(args.query, "abc123");
        });
    }

    fn match_row(session_id: &str, message_id: i64, content: &str) -> SessionMatchRow {
        SessionMatchRow {
            session_id: session_id.to_string(),
            profile_id: "profile".to_string(),
            title: "title".to_string(),
            model: "model".to_string(),
            archived: false,
            role: "user".to_string(),
            content: content.to_string(),
            preview: String::new(),
            timestamp: String::new(),
            updated_at: String::new(),
            message_id,
            message_count: 1,
            score: 1.0,
        }
    }

    #[test]
    fn current_context_filter_removes_only_visible_current_session_hits() {
        let window = vec![aither_core::llm::Message::user(
            "Already visible current-session content",
        )];
        let rows = vec![
            match_row("current", 1, "Already visible current-session content"),
            match_row("current", 2, "Older content not in the window"),
            match_row("other", 3, "Already visible current-session content"),
        ];

        let filtered = filter_rows_against_messages("current", &window, rows);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].message_id, 2);
        assert_eq!(filtered[1].session_id, "other");
    }

    fn summary(session_id: &str, archived: bool) -> SessionSummaryView {
        SessionSummaryView {
            session_id: session_id.to_string(),
            title: "Plan".to_string(),
            model: "m1".to_string(),
            profile_id: Some("p1".to_string()),
            archived,
            created_at: "2026-01-01".to_string(),
            updated_at: "2026-01-02".to_string(),
            message_count: 4,
            preview: "first words".to_string(),
        }
    }

    fn message(message_id: i32, role: &str, content: &str) -> SessionMessageView {
        SessionMessageView {
            message_id,
            role: role.to_string(),
            timestamp: Some("2026-01-02T10:00:00Z".to_string()),
            content: content.to_string(),
        }
    }

    #[test]
    fn list_result_reads_as_sentences_and_session_lines() {
        let text = SessionListText {
            total_matches: 5,
            results: vec![summary("s1", false), summary("s2", true)],
        }
        .render()
        .expect("session list template must render");
        assert_eq!(
            text,
            "Showing 2 of 5 matching session(s).\n\nSession s1 \"Plan\" (model: m1, 4 messages, updated 2026-01-02, profile p1, created 2026-01-01)\nPreview: first words\n\nSession s2 \"Plan\" (model: m1, 4 messages, updated 2026-01-02, archived, profile p1, created 2026-01-01)\nPreview: first words\n"
        );
    }

    #[test]
    fn messages_result_lists_each_message_with_role_and_time() {
        let text = SessionMessagesText {
            session: summary("s1", false),
            messages: vec![message(1, "user", "hi"), message(2, "assistant", "hello")],
        }
        .render()
        .expect("session messages template must render");
        assert_eq!(
            text,
            "Session s1 \"Plan\" (model: m1, 4 messages, updated 2026-01-02, profile p1, created 2026-01-01)\nPreview: first words\n2 message(s):\n\nMessage 1 (user, 2026-01-02T10:00:00Z):\nhi\n\nMessage 2 (assistant, 2026-01-02T10:00:00Z):\nhello\n"
        );
    }

    #[test]
    fn inspect_result_names_the_final_messages() {
        let text = SessionInspectText {
            session: summary("s1", false),
            final_assistant_message: Some(message(2, "assistant", "done")),
            final_user_message: Some(message(1, "user", "go")),
            latest_message: None,
        }
        .render()
        .expect("session inspect template must render");
        assert_eq!(
            text,
            "Session s1 \"Plan\" (model: m1, 4 messages, updated 2026-01-02, profile p1, created 2026-01-01)\nPreview: first words\n\nFinal assistant message:\nMessage 2 (assistant, 2026-01-02T10:00:00Z):\ndone\n\nFinal user message:\nMessage 1 (user, 2026-01-02T10:00:00Z):\ngo\n"
        );
    }

    #[test]
    fn search_results_state_query_mode_and_matches() {
        let item = MessageResult::from_row(match_row("s1", 7, "needle in a haystack"), true);
        let text = SessionSearchMessagesText {
            query: "needle".to_string(),
            total_matches: 1,
            items: vec![item],
        }
        .render()
        .expect("session search template must render");
        assert!(
            text.starts_with(
                "Search \"needle\": showing 1 of 1 matching message(s).\n\nMessage 7 ("
            ),
            "unexpected search text: {text}"
        );
        assert!(text.contains("score 1."), "{text}");
        assert!(text.contains("Snippet: needle in a haystack"), "{text}");
        assert!(text.contains("Content:\nneedle in a haystack"), "{text}");
    }
}
