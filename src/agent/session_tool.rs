use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aither_core::{
    EmbeddingModel,
    llm::{Tool, ToolResult},
};
use aither_rag::{Document, Rag};
use arrow_array::{
    Array, ArrayRef, Float32Array, Int32Array, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures_lite::StreamExt;
use lance_index::scalar::FullTextSearchQuery;
use lancedb::index::Index;
use lancedb::index::scalar::{BTreeIndexBuilder, InvertedIndexParams};
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::{Table, connect};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlparser::ast::{BinaryOperator, Expr as SqlExpr, Ident, Value as SqlValue};

use crate::agent::search_match::{extend_unique, push_unique};
use crate::db::ChatDb;
use crate::embedding_provider::{
    EmbeddingModelRequireError, EmbeddingModelResolution, MayEmbeddingModel,
};

const TABLE_NAME: &str = "session_messages";
const INDEX_META_FILE: &str = "session_index.meta.json";
const SEMANTIC_INDEX_FILE: &str = "session_index.semantic.redb";

/// Search across May sessions and messages.
///
/// Query rules:
/// - Use `*` as query to disable text match and only apply filters.
/// - `search_mode`: `hybrid` | `text` | `regex` | `fulltext` | `fuzzy` | `semantic`.
/// - Default excludes current session so you can inspect other sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionSearchMode {
    /// Combine full-text and semantic ranking.
    Hybrid,
    /// Plain substring match.
    Text,
    /// Regular-expression match.
    Regex,
    /// Lance FTS index search.
    Fulltext,
    /// Fuzzy full-text search with typo tolerance.
    Fuzzy,
    /// Embedding-similarity search.
    Semantic,
}

/// Output shape for the `session list` tool command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionOutputMode {
    /// Return grouped session summaries.
    Sessions,
    /// Return matching messages flat.
    Messages,
}

/// Parsed arguments of a `session` tool call.
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

    /// Query string. Use `*` for filter-only listing.
    pub query: String,

    /// Search mode: `hybrid`, `text`, `regex`, `fulltext`, `fuzzy`, `semantic`.
    #[serde(default = "default_search_mode")]
    pub search_mode: SessionSearchMode,

    /// Output mode: `sessions` (aggregated) or `messages`.
    #[serde(default = "default_output_mode")]
    pub output_mode: SessionOutputMode,

    /// Max returned rows.
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// Skip N rows after ranking.
    #[serde(default)]
    pub offset: usize,

    /// Max rows to scan from `LanceDB` before local post-filtering.
    #[serde(default = "default_scan_limit")]
    pub scan_limit: usize,

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

    /// Case sensitive matching for `text` / `regex` modes.
    #[serde(default)]
    pub case_sensitive: bool,

    /// Regex pattern for `regex` mode (falls back to `query` if omitted).
    #[serde(default)]
    pub regex: Option<String>,

    /// Max edit distance for `fuzzy` mode (0-2 recommended).
    #[serde(default = "default_fuzzy_distance")]
    pub fuzzy_distance: u32,

    /// Search columns for fulltext/fuzzy mode. Defaults to `content,title,preview`.
    #[serde(default)]
    pub fields: Vec<String>,

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

const fn default_search_mode() -> SessionSearchMode {
    SessionSearchMode::Hybrid
}

const fn default_output_mode() -> SessionOutputMode {
    SessionOutputMode::Sessions
}

const fn default_limit() -> usize {
    20
}

const fn default_scan_limit() -> usize {
    2000
}

const fn default_fuzzy_distance() -> u32 {
    1
}

/// `session` tool: full-text/semantic search and inspection over session history.
#[derive(Debug, Clone)]
pub struct SessionTool {
    may_dir: PathBuf,
    current_session_id: String,
    embedder: EmbeddingModelResolution,
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
    #[error("{0}")]
    EmbeddingModel(#[from] EmbeddingModelRequireError),
    #[error("LanceDB error: {0}")]
    LanceDb(#[from] lancedb::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("tool result serialization error: {0}")]
    ToolResult(#[from] aither_core::Error),
    #[error("RAG error: {0}")]
    Rag(#[from] aither_rag::RagError),
    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("invalid regex pattern: {0}")]
    InvalidRegex(#[source] regex::Error),
    #[error("failed to parse semantic metadata integer: {0}")]
    ParseInt(#[from] std::num::ParseIntError),
    #[error("{command_name} command must not be empty")]
    EmptyCommand { command_name: String },
    #[error("{command_name} command must include a search prompt")]
    MissingCommandPrompt { command_name: String },
    #[error("invalid session command: missing prompt after 'search'")]
    MissingSearchPrompt,
    #[error("failed to remove stale session semantic index {path}: {source}")]
    RemoveStaleSemanticIndex {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("session semantic index entry is missing '{key}' metadata")]
    MissingSemanticMetadata { key: String },
    #[error("session semantic search does not support wildcard queries")]
    SemanticWildcardUnsupported,
    #[error("session semantic index is missing; rerun the query after indexing data")]
    MissingSemanticIndex,
    #[error("failed to inspect session semantic index {path}: {source}")]
    InspectSemanticIndex {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("missing or invalid '{name}' column")]
    MissingOrInvalidColumn { name: String },
    #[error("invalid full-text search column selection: {0}")]
    InvalidFullTextColumns(String),
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
    /// Bind the tool to a session and embedder resolution.
    #[must_use]
    pub const fn new(
        may_dir: PathBuf,
        current_session_id: String,
        embedder: EmbeddingModelResolution,
    ) -> Self {
        Self {
            may_dir,
            current_session_id,
            embedder,
        }
    }

    fn db_path(&self) -> PathBuf {
        self.may_dir.join("history.db")
    }

    fn index_path(&self) -> PathBuf {
        self.may_dir.join("session_index.lancedb")
    }

    fn index_meta_path(&self) -> PathBuf {
        self.may_dir.join(INDEX_META_FILE)
    }

    fn semantic_index_path(&self) -> PathBuf {
        self.may_dir.join(SEMANTIC_INDEX_FILE)
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
            self.index_path(),
            self.index_meta_path(),
            self.semantic_index_path(),
            self.embedder.clone(),
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
    args.scan_limit = args.scan_limit.clamp(args.limit, 50_000);
    args.fuzzy_distance = args.fuzzy_distance.min(3);
    if args.fields.is_empty() {
        args.fields = vec![
            "content".to_string(),
            "title".to_string(),
            "preview".to_string(),
        ];
    } else {
        args.fields = args
            .fields
            .into_iter()
            .map(|v| v.to_lowercase())
            .filter(|f| {
                matches!(
                    f.as_str(),
                    "content" | "title" | "preview" | "session_id" | "model" | "role"
                )
            })
            .collect();
        if args.fields.is_empty() {
            args.fields = vec!["content".to_string()];
        }
    }
    args
}

async fn execute_plan(
    db_path: PathBuf,
    index_path: PathBuf,
    index_meta_path: PathBuf,
    semantic_index_path: PathBuf,
    embedder: EmbeddingModelResolution,
    current_session_id: String,
    plan: SessionExecutionPlan,
) -> Result<ToolResult, SessionToolError> {
    match plan {
        SessionExecutionPlan::Search(args) => {
            execute_search(
                db_path,
                index_path,
                index_meta_path,
                semantic_index_path,
                embedder,
                current_session_id,
                args,
            )
            .await
        }
        SessionExecutionPlan::Inspect(request) => execute_inspect(db_path, request).await,
        SessionExecutionPlan::Messages(request) => execute_messages(db_path, request).await,
        SessionExecutionPlan::List(request) => execute_list(db_path, request).await,
    }
}

async fn execute_search(
    db_path: PathBuf,
    index_path: PathBuf,
    index_meta_path: PathBuf,
    semantic_index_path: PathBuf,
    embedder: EmbeddingModelResolution,
    current_session_id: String,
    args: SessionArgs,
) -> Result<ToolResult, SessionToolError> {
    let query_enabled = args.query.trim() != "*" && !args.query.trim().is_empty();
    let semantic_index_required = args.search_mode == SessionSearchMode::Semantic;

    let db = ChatDb::open(&db_path).await?;
    let fingerprint = compute_fingerprint(&db).await?;

    let table = ensure_index(
        &index_path,
        &index_meta_path,
        &semantic_index_path,
        &db,
        &fingerprint,
        embedder.clone(),
        semantic_index_required,
    )
    .await?;

    if args.search_mode == SessionSearchMode::Semantic {
        let embedder = embedder.require_ready(
            "session semantic search requires a global models.embedding_model and at least one enabled provider that supports it",
        )?;
        return run_semantic_query(
            semantic_index_path,
            embedder,
            &db,
            &current_session_id,
            args,
        )
        .await;
    }

    let mut candidates = match args.search_mode {
        SessionSearchMode::Fulltext => run_full_text_query(&table, &args, false).await?,
        SessionSearchMode::Fuzzy => run_full_text_query(&table, &args, true).await?,
        SessionSearchMode::Regex | SessionSearchMode::Text => {
            run_plain_query(&table, &args).await?
        }
        SessionSearchMode::Hybrid => {
            if query_enabled {
                let mut merged = Vec::new();
                let mut fts = run_full_text_query(&table, &args, false).await?;
                let mut fuzzy = run_full_text_query(&table, &args, true).await?;
                let mut plain = run_plain_query(&table, &args).await?;
                merged.append(&mut fts);
                merged.append(&mut fuzzy);
                merged.append(&mut plain);
                merged
            } else {
                run_plain_query(&table, &args).await?
            }
        }
        SessionSearchMode::Semantic => unreachable!("semantic search handled earlier"),
    };

    let query_norm = if args.case_sensitive {
        args.query.clone()
    } else {
        args.query.to_lowercase()
    };

    let regex_enabled =
        args.search_mode == SessionSearchMode::Regex && (query_enabled || args.regex.is_some());
    let regex = if regex_enabled {
        let pattern = args
            .regex
            .as_deref()
            .unwrap_or(args.query.as_str())
            .to_string();
        Some(
            RegexBuilder::new(&pattern)
                .case_insensitive(!args.case_sensitive)
                .build()
                .map_err(SessionToolError::InvalidRegex)?,
        )
    } else {
        None
    };

    for row in &mut candidates {
        if row.matched_by.is_empty() {
            push_unique(&mut row.matched_by, args.search_mode);
        }

        let haystack = searchable_text(row, &args.fields);
        let haystack_norm = if args.case_sensitive {
            haystack.clone()
        } else {
            haystack.to_lowercase()
        };

        if query_enabled
            && args.search_mode != SessionSearchMode::Fulltext
            && args.search_mode != SessionSearchMode::Fuzzy
            && haystack_norm.contains(&query_norm)
        {
            row.score += 10.0;
            push_unique(&mut row.matched_by, SessionSearchMode::Text);
        }

        if let Some(re) = &regex
            && re.is_match(&haystack)
        {
            row.score += 15.0;
            push_unique(&mut row.matched_by, SessionSearchMode::Regex);
        }
    }

    candidates.retain(|row| {
        if query_enabled && args.search_mode == SessionSearchMode::Text {
            let haystack = searchable_text(row, &args.fields);
            let haystack = if args.case_sensitive {
                haystack
            } else {
                haystack.to_lowercase()
            };
            return haystack.contains(&query_norm);
        }

        if regex_enabled {
            let Some(re) = &regex else {
                return false;
            };
            return re.is_match(&searchable_text(row, &args.fields));
        }

        true
    });

    let mut deduped: HashMap<String, SessionMatchRow> = HashMap::new();
    for row in candidates {
        let key = format!("{}:{}", row.session_id, row.message_id);
        match deduped.get_mut(&key) {
            Some(existing) => {
                if row.score > existing.score {
                    *existing = row;
                } else {
                    extend_unique(&mut existing.matched_by, row.matched_by);
                }
            }
            None => {
                deduped.insert(key, row);
            }
        }
    }

    let mut rows: Vec<SessionMatchRow> = deduped.into_values().collect();
    rows.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.timestamp.cmp(&a.timestamp))
    });

    let total_matches = rows.len();
    let rows = drop_rows_already_in_context(&db, &current_session_id, rows).await?;
    if total_matches > 0 && rows.is_empty() {
        return Err(SessionToolError::AllResultsAlreadyInContext);
    }
    finalize_rows_with_total(&rows, total_matches, &args)
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

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
struct SessionInspectView {
    session: SessionSummaryView,
    final_assistant_message: Option<SessionMessageView>,
    final_user_message: Option<SessionMessageView>,
    latest_message: Option<SessionMessageView>,
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

    ToolResult::json(&serde_json::json!({
        "action": "inspect",
        "result": SessionInspectView {
            session: SessionSummaryView::from(session),
            final_assistant_message,
            final_user_message,
            latest_message,
        }
    }))
    .map_err(SessionToolError::ToolResult)
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

    ToolResult::json(&serde_json::json!({
        "action": "messages",
        "session": SessionSummaryView::from(session),
        "returned": messages.len(),
        "messages": messages,
    }))
    .map_err(SessionToolError::ToolResult)
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

    ToolResult::json(&serde_json::json!({
        "action": "list",
        "returned": results.len(),
        "total_matches": total_matches,
        "results": results,
    }))
    .map_err(SessionToolError::ToolResult)
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionIndexMeta {
    session_count: usize,
    total_messages: usize,
    latest_updated_at: String,
    semantic_embedding_model: Option<String>,
    semantic_embedding_dimensions: Option<usize>,
}

#[derive(Debug, Clone)]
struct SessionIndexRow {
    session_id: String,
    profile_id: String,
    title: String,
    model: String,
    archived: String,
    role: String,
    content: String,
    preview: String,
    timestamp: String,
    created_at: String,
    updated_at: String,
    message_id: i64,
    message_count: i32,
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
    matched_by: Vec<SessionSearchMode>,
}

#[derive(Debug, Clone, Serialize)]
struct MessageResult {
    session_id: String,
    title: String,
    model: String,
    role: String,
    timestamp: String,
    message_id: i64,
    score: f64,
    matched_by: Vec<SessionSearchMode>,
    snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
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
            matched_by: row.matched_by,
            snippet,
            content: include_content.then_some(row.content),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
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

async fn compute_fingerprint(db: &ChatDb) -> Result<SessionIndexMeta, SessionToolError> {
    let sessions = db.list_sessions().await?;
    let session_count = sessions.len();
    let total_messages: usize = sessions
        .iter()
        .map(|s| usize::try_from(s.message_count.max(0)).unwrap_or_default())
        .sum();

    let latest_updated_at = sessions
        .iter()
        .map(|s| s.updated_at.as_str())
        .max()
        .unwrap_or("")
        .to_string();

    Ok(SessionIndexMeta {
        session_count,
        total_messages,
        latest_updated_at,
        semantic_embedding_model: None,
        semantic_embedding_dimensions: None,
    })
}

async fn ensure_index(
    index_path: &Path,
    meta_path: &Path,
    semantic_index_path: &Path,
    db: &ChatDb,
    fingerprint: &SessionIndexMeta,
    embedder: EmbeddingModelResolution,
    semantic_index_required: bool,
) -> Result<Table, SessionToolError> {
    let conn = connect(index_path.to_string_lossy().as_ref())
        .execute()
        .await?;

    let mut should_rebuild = true;
    let semantic_index_missing = matches!(
        async_fs::metadata(semantic_index_path).await,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    );
    let table_names = conn.table_names().execute().await?;
    let has_table = table_names.iter().any(|name| name == TABLE_NAME);

    if has_table
        && let Ok(meta_raw) = async_fs::read_to_string(meta_path).await
        && let Ok(saved_meta) = serde_json::from_str::<SessionIndexMeta>(&meta_raw)
        && saved_meta.session_count == fingerprint.session_count
        && saved_meta.total_messages == fingerprint.total_messages
        && saved_meta.latest_updated_at == fingerprint.latest_updated_at
    {
        should_rebuild = false;
    }

    if should_rebuild {
        if has_table {
            conn.drop_table(TABLE_NAME, &[]).await?;
        }

        let rows = build_index_rows(db).await?;
        let reader = rows_to_record_reader(&rows)?;
        let table = conn.create_table(TABLE_NAME, reader).execute().await?;

        for column in ["content", "title", "preview"] {
            table
                .create_index(&[column], Index::FTS(InvertedIndexParams::default()))
                .execute()
                .await?;
        }
        table
            .create_index(&["session_id"], Index::BTree(BTreeIndexBuilder::default()))
            .execute()
            .await?;
        table
            .create_index(&["model"], Index::BTree(BTreeIndexBuilder::default()))
            .execute()
            .await?;
        table
            .create_index(&["role"], Index::BTree(BTreeIndexBuilder::default()))
            .execute()
            .await?;

        let next_meta = SessionIndexMeta {
            session_count: fingerprint.session_count,
            total_messages: fingerprint.total_messages,
            latest_updated_at: fingerprint.latest_updated_at.clone(),
            semantic_embedding_model: None,
            semantic_embedding_dimensions: None,
        };
        if semantic_index_required && let Some(embedder) = embedder.clone_ready() {
            rebuild_semantic_index(semantic_index_path, embedder.clone(), &rows).await?;
            let next_meta = SessionIndexMeta {
                semantic_embedding_model: Some(embedder.selector().to_string()),
                semantic_embedding_dimensions: Some(embedder.dim()),
                ..next_meta
            };
            async_fs::write(meta_path, serde_json::to_vec_pretty(&next_meta)?).await?;
        } else {
            async_fs::write(meta_path, serde_json::to_vec_pretty(&next_meta)?).await?;
        }

        Ok(table)
    } else {
        if semantic_index_required && let Some(embedder) = embedder.clone_ready() {
            let saved_meta = async_fs::read_to_string(meta_path)
                .await
                .ok()
                .and_then(|raw| serde_json::from_str::<SessionIndexMeta>(&raw).ok());
            let semantic_needs_rebuild = semantic_index_missing
                || saved_meta.as_ref().is_none_or(|meta| {
                    meta.semantic_embedding_model.as_deref() != Some(embedder.selector())
                        || meta.semantic_embedding_dimensions != Some(embedder.dim())
                });
            if semantic_needs_rebuild {
                let rows = build_index_rows(db).await?;
                rebuild_semantic_index(semantic_index_path, embedder.clone(), &rows).await?;
                let next_meta = SessionIndexMeta {
                    session_count: fingerprint.session_count,
                    total_messages: fingerprint.total_messages,
                    latest_updated_at: fingerprint.latest_updated_at.clone(),
                    semantic_embedding_model: Some(embedder.selector().to_string()),
                    semantic_embedding_dimensions: Some(embedder.dim()),
                };
                async_fs::write(meta_path, serde_json::to_vec_pretty(&next_meta)?).await?;
            }
        }
        Ok(conn.open_table(TABLE_NAME).execute().await?)
    }
}

async fn rebuild_semantic_index(
    semantic_index_path: &Path,
    embedder: MayEmbeddingModel,
    rows: &[SessionIndexRow],
) -> Result<(), SessionToolError> {
    match async_fs::remove_file(semantic_index_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(SessionToolError::RemoveStaleSemanticIndex {
                path: semantic_index_path.to_path_buf(),
                source: error,
            });
        }
    }

    let rag = Rag::builder(embedder)
        .index_path(semantic_index_path.to_path_buf())
        .code_chunking(2400)
        .top_k(2000)
        .deduplication(false)
        .auto_save(false)
        .build()?;

    for row in rows {
        let mut metadata = aither_rag::Metadata::new();
        metadata.insert("session_id".to_string(), row.session_id.clone());
        metadata.insert("profile_id".to_string(), row.profile_id.clone());
        metadata.insert("title".to_string(), row.title.clone());
        metadata.insert("model".to_string(), row.model.clone());
        metadata.insert("archived".to_string(), row.archived.clone());
        metadata.insert("role".to_string(), row.role.clone());
        metadata.insert("preview".to_string(), row.preview.clone());
        metadata.insert("timestamp".to_string(), row.timestamp.clone());
        metadata.insert("updated_at".to_string(), row.updated_at.clone());
        metadata.insert("message_id".to_string(), row.message_id.to_string());
        metadata.insert("message_count".to_string(), row.message_count.to_string());

        rag.insert(Document::with_metadata(
            format!("{}:{}", row.session_id, row.message_id),
            row.content.clone(),
            metadata,
        ))
        .await?;
    }

    rag.save().await?;
    Ok(())
}

async fn build_index_rows(db: &ChatDb) -> Result<Vec<SessionIndexRow>, SessionToolError> {
    let sessions = db.list_sessions().await?;
    let mut rows = Vec::new();

    for session in sessions {
        let session_id = session.id;
        let title = session.title.unwrap_or_default();
        let model = session.model.unwrap_or_default();
        let preview = session.preview.unwrap_or_default();
        let created_at = session.created_at;
        let updated_at = session.updated_at;
        let message_count = session.message_count;

        let messages = db.get_messages(&session_id).await?;
        for msg in messages {
            if matches!(
                msg.role.as_str(),
                "permission_request"
                    | "permission_response"
                    | "ask_user_request"
                    | "ask_user_response"
                    | "hidden_context"
            ) {
                continue;
            }
            rows.push(SessionIndexRow {
                session_id: session_id.clone(),
                profile_id: session.profile_id.clone().unwrap_or_default(),
                title: title.clone(),
                model: model.clone(),
                archived: if session.archived { "1" } else { "0" }.to_string(),
                role: msg.role,
                content: msg.content,
                preview: preview.clone(),
                timestamp: msg.timestamp.unwrap_or_default(),
                created_at: created_at.clone(),
                updated_at: updated_at.clone(),
                message_id: i64::from(msg.id),
                message_count,
            });
        }
    }

    Ok(rows)
}

fn rows_to_record_reader(
    rows: &[SessionIndexRow],
) -> Result<Box<dyn RecordBatchReader + Send>, SessionToolError> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("session_id", DataType::Utf8, false),
        Field::new("profile_id", DataType::Utf8, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("model", DataType::Utf8, false),
        Field::new("archived", DataType::Utf8, false),
        Field::new("role", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("preview", DataType::Utf8, false),
        Field::new("timestamp", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
        Field::new("message_id", DataType::Int64, false),
        Field::new("message_count", DataType::Int32, false),
    ]));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.session_id.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.profile_id.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.title.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.model.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.archived.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.role.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.content.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.preview.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.timestamp.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.created_at.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.updated_at.as_str()),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.message_id),
            )),
            Arc::new(Int32Array::from_iter_values(
                rows.iter().map(|r| r.message_count),
            )),
        ],
    )?;

    let iter = RecordBatchIterator::new(vec![Ok(batch)], schema);
    Ok(Box::new(iter))
}

fn build_sql_filter(args: &SessionArgs) -> Option<String> {
    let mut filters = Vec::new();

    if let Some(session_id) = &args.session_id {
        filters.push(binary_sql_expr(
            "session_id",
            BinaryOperator::Eq,
            session_id,
        ));
    }

    if let Some(excluded) = &args.exclude_session_id {
        filters.push(binary_sql_expr(
            "session_id",
            BinaryOperator::NotEq,
            excluded,
        ));
    }

    if let Some(model) = &args.model {
        filters.push(binary_sql_expr("model", BinaryOperator::Eq, model));
    }

    if let Some(profile_id) = &args.profile_id {
        filters.push(binary_sql_expr(
            "profile_id",
            BinaryOperator::Eq,
            profile_id,
        ));
    }

    if let Some(role) = &args.role {
        filters.push(binary_sql_expr("role", BinaryOperator::Eq, role));
    }

    if let Some(since) = &args.since {
        filters.push(binary_sql_expr("timestamp", BinaryOperator::GtEq, since));
    }

    if let Some(until) = &args.until {
        filters.push(binary_sql_expr("timestamp", BinaryOperator::LtEq, until));
    }

    if let Some(title_contains) = &args.title_contains {
        filters.push(SqlExpr::Like {
            negated: false,
            any: false,
            expr: Box::new(identifier_sql_expr("title")),
            pattern: Box::new(string_sql_expr(contains_match_pattern(title_contains))),
            escape_char: None,
        });
    }

    if !args.include_archived {
        filters.push(binary_sql_expr("archived", BinaryOperator::Eq, "0"));
    }

    filters
        .into_iter()
        .reduce(|left, right| SqlExpr::BinaryOp {
            left: Box::new(left),
            op: BinaryOperator::And,
            right: Box::new(right),
        })
        .map(|expr| expr.to_string())
}

fn identifier_sql_expr(column: &str) -> SqlExpr {
    SqlExpr::Identifier(Ident::new(column))
}

fn string_sql_expr(value: String) -> SqlExpr {
    SqlExpr::value(SqlValue::SingleQuotedString(value))
}

fn contains_match_pattern(value: &str) -> String {
    let mut pattern = String::with_capacity(value.len() + 2);
    pattern.push('%');
    pattern.push_str(value);
    pattern.push('%');
    pattern
}

fn binary_sql_expr(column: &str, op: BinaryOperator, value: &str) -> SqlExpr {
    SqlExpr::BinaryOp {
        left: Box::new(identifier_sql_expr(column)),
        op,
        right: Box::new(string_sql_expr(value.to_string())),
    }
}

async fn run_plain_query(
    table: &Table,
    args: &SessionArgs,
) -> Result<Vec<SessionMatchRow>, SessionToolError> {
    let mut query = table
        .query()
        .select(Select::columns(&[
            "session_id",
            "profile_id",
            "title",
            "model",
            "archived",
            "role",
            "content",
            "preview",
            "timestamp",
            "updated_at",
            "message_id",
            "message_count",
        ]))
        .limit(args.scan_limit);

    if let Some(filter) = build_sql_filter(args) {
        query = query.only_if(filter);
    }

    let mut stream = query.execute().await?;
    read_rows_from_stream(&mut stream).await
}

async fn run_full_text_query(
    table: &Table,
    args: &SessionArgs,
    fuzzy: bool,
) -> Result<Vec<SessionMatchRow>, SessionToolError> {
    if args.query.trim() == "*" || args.query.trim().is_empty() {
        return run_plain_query(table, args).await;
    }

    let mut fts = if fuzzy {
        FullTextSearchQuery::new_fuzzy(args.query.clone(), Some(args.fuzzy_distance))
    } else {
        FullTextSearchQuery::new(args.query.clone())
    };

    let fts_columns: Vec<String> = args
        .fields
        .iter()
        .filter(|f| matches!(f.as_str(), "content" | "title" | "preview"))
        .cloned()
        .collect();
    if !fts_columns.is_empty() {
        fts = fts
            .with_columns(&fts_columns)
            .map_err(|error| SessionToolError::InvalidFullTextColumns(error.to_string()))?;
    }

    let mut query = table
        .query()
        .full_text_search(fts)
        .select(Select::columns(&[
            "session_id",
            "profile_id",
            "title",
            "model",
            "archived",
            "role",
            "content",
            "preview",
            "timestamp",
            "updated_at",
            "message_id",
            "message_count",
            "_score",
        ]))
        .limit(args.scan_limit);

    if let Some(filter) = build_sql_filter(args) {
        query = query.only_if(filter);
    }

    let mut stream = query.execute().await?;
    let mut rows = read_rows_from_stream(&mut stream).await?;
    for row in &mut rows {
        push_unique(
            &mut row.matched_by,
            if fuzzy {
                SessionSearchMode::Fuzzy
            } else {
                SessionSearchMode::Fulltext
            },
        );
    }
    Ok(rows)
}

fn require_semantic_metadata<'a>(
    metadata: &'a aither_rag::Metadata,
    key: &str,
) -> Result<&'a str, SessionToolError> {
    metadata
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| SessionToolError::MissingSemanticMetadata {
            key: key.to_string(),
        })
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
    if let Some(title_contains) = &args.title_contains {
        let title = if args.case_sensitive {
            row.title.clone()
        } else {
            row.title.to_lowercase()
        };
        let needle = if args.case_sensitive {
            title_contains.clone()
        } else {
            title_contains.to_lowercase()
        };
        if !title.contains(&needle) {
            return false;
        }
    }
    args.include_archived || !row.archived
}

async fn run_semantic_query(
    semantic_index_path: PathBuf,
    embedder: MayEmbeddingModel,
    db: &ChatDb,
    current_session_id: &str,
    args: SessionArgs,
) -> Result<ToolResult, SessionToolError> {
    if args.query.trim() == "*" {
        return Err(SessionToolError::SemanticWildcardUnsupported);
    }

    match async_fs::metadata(&semantic_index_path).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SessionToolError::MissingSemanticIndex);
        }
        Err(error) => {
            return Err(SessionToolError::InspectSemanticIndex {
                path: semantic_index_path,
                source: error,
            });
        }
    }

    let rag = Rag::builder(embedder)
        .index_path(semantic_index_path)
        .code_chunking(2400)
        .top_k(args.scan_limit)
        .deduplication(false)
        .auto_save(false)
        .build()?;
    rag.load().await?;

    let rows = rag
        .search_with_k(args.query.as_str(), args.scan_limit)
        .await?
        .into_iter()
        .map(|result| -> Result<SessionMatchRow, SessionToolError> {
            let metadata = &result.chunk.metadata;
            Ok(SessionMatchRow {
                session_id: require_semantic_metadata(metadata, "session_id")?.to_string(),
                profile_id: require_semantic_metadata(metadata, "profile_id")?.to_string(),
                title: require_semantic_metadata(metadata, "title")?.to_string(),
                model: require_semantic_metadata(metadata, "model")?.to_string(),
                archived: require_semantic_metadata(metadata, "archived")? == "1",
                role: require_semantic_metadata(metadata, "role")?.to_string(),
                content: result.chunk.text,
                preview: require_semantic_metadata(metadata, "preview")?.to_string(),
                timestamp: require_semantic_metadata(metadata, "timestamp")?.to_string(),
                updated_at: require_semantic_metadata(metadata, "updated_at")?.to_string(),
                message_id: require_semantic_metadata(metadata, "message_id")?.parse::<i64>()?,
                message_count: require_semantic_metadata(metadata, "message_count")?
                    .parse::<i32>()?,
                score: f64::from(result.score),
                matched_by: vec![SessionSearchMode::Semantic],
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|row| row_matches_session_filters(row, &args))
        .collect::<Vec<_>>();

    let total_matches = rows.len();
    let rows = drop_rows_already_in_context(db, current_session_id, rows).await?;
    if total_matches > 0 && rows.is_empty() {
        return Err(SessionToolError::AllResultsAlreadyInContext);
    }
    finalize_rows_with_total(&rows, total_matches, &args)
}

async fn read_rows_from_stream(
    stream: &mut lancedb::arrow::SendableRecordBatchStream,
) -> Result<Vec<SessionMatchRow>, SessionToolError> {
    let mut rows = Vec::new();

    while let Some(batch_res) = stream.next().await {
        let batch = batch_res?;

        let session_id = as_string(batch.column_by_name("session_id"), "session_id")?;
        let title = as_string(batch.column_by_name("title"), "title")?;
        let model = as_string(batch.column_by_name("model"), "model")?;
        let profile_id = as_string(batch.column_by_name("profile_id"), "profile_id")?;
        let archived = as_string(batch.column_by_name("archived"), "archived")?;
        let role = as_string(batch.column_by_name("role"), "role")?;
        let content = as_string(batch.column_by_name("content"), "content")?;
        let preview = as_string(batch.column_by_name("preview"), "preview")?;
        let timestamp = as_string(batch.column_by_name("timestamp"), "timestamp")?;
        let updated_at = as_string(batch.column_by_name("updated_at"), "updated_at")?;
        let message_id = as_i64(batch.column_by_name("message_id"), "message_id")?;
        let message_count = as_i32(batch.column_by_name("message_count"), "message_count")?;

        let score_col = batch.column_by_name("_score");
        let scores = score_col
            .and_then(|c| c.as_any().downcast_ref::<Float32Array>())
            .map_or_else(
                || vec![0.0; batch.num_rows()],
                |arr| {
                    (0..arr.len())
                        .map(|i| {
                            if arr.is_null(i) {
                                0.0
                            } else {
                                f64::from(arr.value(i))
                            }
                        })
                        .collect::<Vec<_>>()
                },
            );

        for (i, score) in scores.iter().copied().enumerate().take(batch.num_rows()) {
            rows.push(SessionMatchRow {
                session_id: session_id.value(i).to_string(),
                profile_id: profile_id.value(i).to_string(),
                title: title.value(i).to_string(),
                model: model.value(i).to_string(),
                archived: archived.value(i) == "1",
                role: role.value(i).to_string(),
                content: content.value(i).to_string(),
                preview: preview.value(i).to_string(),
                timestamp: timestamp.value(i).to_string(),
                updated_at: updated_at.value(i).to_string(),
                message_id: message_id.value(i),
                message_count: message_count.value(i),
                score,
                matched_by: Vec::new(),
            });
        }
    }

    Ok(rows)
}

fn as_string<'a>(
    column: Option<&'a ArrayRef>,
    name: &str,
) -> Result<&'a StringArray, SessionToolError> {
    column
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| SessionToolError::MissingOrInvalidColumn {
            name: name.to_string(),
        })
}

fn as_i64<'a>(
    column: Option<&'a ArrayRef>,
    name: &str,
) -> Result<&'a Int64Array, SessionToolError> {
    column
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
        .ok_or_else(|| SessionToolError::MissingOrInvalidColumn {
            name: name.to_string(),
        })
}

fn as_i32<'a>(
    column: Option<&'a ArrayRef>,
    name: &str,
) -> Result<&'a Int32Array, SessionToolError> {
    column
        .and_then(|c| c.as_any().downcast_ref::<Int32Array>())
        .ok_or_else(|| SessionToolError::MissingOrInvalidColumn {
            name: name.to_string(),
        })
}

fn searchable_text(row: &SessionMatchRow, fields: &[String]) -> String {
    let mut parts = Vec::new();
    for field in fields {
        match field.as_str() {
            "content" => parts.push(row.content.as_str()),
            "title" => parts.push(row.title.as_str()),
            "preview" => parts.push(row.preview.as_str()),
            "session_id" => parts.push(row.session_id.as_str()),
            "model" => parts.push(row.model.as_str()),
            "role" => parts.push(row.role.as_str()),
            _ => {}
        }
    }
    parts.join("\n")
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
) -> Result<ToolResult, SessionToolError> {
    let start = args.offset.min(rows.len());
    let end = (start + args.limit).min(rows.len());
    let rows = rows[start..end].to_vec();

    let value = if args.output_mode == SessionOutputMode::Messages {
        let items: Vec<MessageResult> = rows
            .into_iter()
            .map(|row| MessageResult::from_row(row, args.include_content))
            .collect();
        serde_json::json!({
            "query": args.query,
            "search_mode": args.search_mode,
            "output_mode": args.output_mode,
            "total_matches": total_matches,
            "returned": items.len(),
            "results": items,
        })
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

        serde_json::json!({
            "query": args.query,
            "search_mode": args.search_mode,
            "output_mode": args.output_mode,
            "total_matches": total_matches,
            "returned": items.len(),
            "results": items,
        })
    };

    ToolResult::json(&value).map_err(SessionToolError::ToolResult)
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
            search_mode: SessionSearchMode::Hybrid,
            output_mode: SessionOutputMode::Sessions,
            limit: 20,
            offset: 0,
            scan_limit: 2000,
            session_id: None,
            exclude_session_id: None,
            model: None,
            profile_id: None,
            role: None,
            since: None,
            until: None,
            title_contains: None,
            case_sensitive: false,
            regex: None,
            fuzzy_distance: 1,
            fields: Vec::new(),
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

    #[test]
    fn session_search_mode_serializes_as_lowercase_string() {
        let value =
            serde_json::to_value(vec![SessionSearchMode::Fulltext, SessionSearchMode::Regex])
                .expect("session search modes should serialize");
        assert_eq!(value, serde_json::json!(["fulltext", "regex"]));
    }

    #[test]
    fn sql_filter_ast_builds_expected_predicates() {
        let mut args = base_args();
        args.session_id = Some("session-1".to_string());
        args.model = Some("gpt-5".to_string());
        args.profile_id = Some("work".to_string());
        args.since = Some("2026-03-12T00:00:00Z".to_string());

        let filter = build_sql_filter(&args).expect("filter should be built");

        assert!(filter.contains("session_id = 'session-1'"));
        assert!(filter.contains("model = 'gpt-5'"));
        assert!(filter.contains("profile_id = 'work'"));
        assert!(filter.contains("timestamp >= '2026-03-12T00:00:00Z'"));
        assert!(filter.contains("archived = '0'"));
        assert!(filter.contains(" AND "));
    }

    #[test]
    fn sql_filter_ast_quotes_embedded_single_quotes() {
        let mut args = base_args();
        args.title_contains = Some("owner's update".to_string());

        let filter = build_sql_filter(&args).expect("title filter should be built");

        assert!(filter.contains("title LIKE '%owner''s update%'"));
        assert!(!filter.contains("owner's update%' OR"));
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
            matched_by: vec![SessionSearchMode::Text],
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
}
