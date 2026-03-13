use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aither_core::llm::{Tool, ToolOutput};
use arrow_array::{
    Array, ArrayRef, Float32Array, Int32Array, Int64Array, RecordBatch, RecordBatchIterator,
    RecordBatchReader, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures_lite::StreamExt;
use lance_index::scalar::FullTextSearchQuery;
use lancedb::index::Index;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::{Table, connect};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlparser::ast::{BinaryOperator, Expr as SqlExpr, Ident, Value as SqlValue};

use crate::db::ChatDb;

const TABLE_NAME: &str = "session_messages";
const INDEX_META_FILE: &str = "session_index.meta.json";

/// Search across May sessions and messages.
///
/// Query rules:
/// - Use `*` as query to disable text match and only apply filters.
/// - `search_mode`: `hybrid` | `text` | `regex` | `fulltext` | `fuzzy`.
/// - Default excludes current session so you can inspect other sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionSearchMode {
    Hybrid,
    Text,
    Regex,
    Fulltext,
    Fuzzy,
}

impl SessionSearchMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::Text => "text",
            Self::Regex => "regex",
            Self::Fulltext => "fulltext",
            Self::Fuzzy => "fuzzy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionOutputMode {
    Sessions,
    Messages,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SessionArgs {
    /// Optional command-form input. Supported:
    /// - `session <id> search <prompt>`
    /// - `session <search>`
    #[serde(default)]
    pub command: Option<String>,

    /// Query string. Use `*` for filter-only listing.
    pub query: String,

    /// Search mode: `hybrid`, `text`, `regex`, `fulltext`, `fuzzy`.
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

    /// Max rows to scan from LanceDB before local post-filtering.
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

fn default_search_mode() -> SessionSearchMode {
    SessionSearchMode::Hybrid
}

fn default_output_mode() -> SessionOutputMode {
    SessionOutputMode::Sessions
}

fn default_limit() -> usize {
    20
}

fn default_scan_limit() -> usize {
    2000
}

fn default_fuzzy_distance() -> u32 {
    1
}

#[derive(Debug, Clone)]
pub struct SessionTool {
    may_dir: PathBuf,
    current_session_id: String,
}

#[derive(Debug, Clone)]
pub struct EnvironmentTool {
    may_dir: PathBuf,
    current_session_id: String,
    current_profile_id: Option<String>,
}

#[derive(Debug, Clone)]
struct SearchDefaults {
    current_session_id: String,
    default_profile_id: Option<String>,
    include_current_by_default: bool,
}

impl SessionTool {
    pub fn new(may_dir: PathBuf, current_session_id: String) -> Self {
        Self {
            may_dir,
            current_session_id,
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

    fn normalize_args(&self, mut args: SessionArgs) -> aither_core::Result<SessionArgs> {
        if let Some(command) = args.command.take() {
            apply_command_form(&mut args, &command, "session")?;
        }
        apply_search_defaults(
            &mut args,
            &SearchDefaults {
                current_session_id: self.current_session_id.clone(),
                default_profile_id: None,
                include_current_by_default: false,
            },
        );
        normalize_common_args(args)
    }
}

impl EnvironmentTool {
    pub fn new(
        may_dir: PathBuf,
        current_session_id: String,
        current_profile_id: Option<String>,
    ) -> Self {
        Self {
            may_dir,
            current_session_id,
            current_profile_id,
        }
    }

    fn normalize_args(&self, mut args: SessionArgs) -> aither_core::Result<SessionArgs> {
        if let Some(command) = args.command.take() {
            apply_command_form(&mut args, &command, "environment")?;
        }
        apply_search_defaults(
            &mut args,
            &SearchDefaults {
                current_session_id: self.current_session_id.clone(),
                default_profile_id: self.current_profile_id.clone(),
                include_current_by_default: true,
            },
        );
        normalize_common_args(args)
    }
}

impl Tool for SessionTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("session")
    }

    type Arguments = SessionArgs;

    async fn call(&self, arguments: Self::Arguments) -> aither_core::Result<ToolOutput> {
        let args = self.normalize_args(arguments)?;
        execute_search(self.db_path(), self.index_path(), self.index_meta_path(), args).await
    }
}

impl Tool for EnvironmentTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("environment")
    }

    type Arguments = SessionArgs;

    async fn call(&self, arguments: Self::Arguments) -> aither_core::Result<ToolOutput> {
        let args = self.normalize_args(arguments)?;
        execute_search(self.db_path(), self.index_path(), self.index_meta_path(), args).await
    }
}

impl EnvironmentTool {
    fn db_path(&self) -> PathBuf {
        self.may_dir.join("history.db")
    }

    fn index_path(&self) -> PathBuf {
        self.may_dir.join("session_index.lancedb")
    }

    fn index_meta_path(&self) -> PathBuf {
        self.may_dir.join(INDEX_META_FILE)
    }
}

fn apply_search_defaults(args: &mut SessionArgs, defaults: &SearchDefaults) {
    if args.profile_id.is_none() {
        args.profile_id = defaults.default_profile_id.clone();
    }

    if !args.include_current
        && !defaults.include_current_by_default
        && args.exclude_session_id.is_none()
    {
        args.exclude_session_id = Some(defaults.current_session_id.clone());
    }
}

fn normalize_common_args(mut args: SessionArgs) -> aither_core::Result<SessionArgs> {
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
    Ok(args)
}

async fn execute_search(
    db_path: PathBuf,
    index_path: PathBuf,
    index_meta_path: PathBuf,
    args: SessionArgs,
) -> aither_core::Result<ToolOutput> {
    let query_enabled = args.query.trim() != "*" && !args.query.trim().is_empty();

    let db = ChatDb::open(&db_path).await?;
    let fingerprint = compute_fingerprint(&db).await?;

    let table = ensure_index(&index_path, &index_meta_path, &db, &fingerprint).await?;

    let mut candidates = match args.search_mode {
        SessionSearchMode::Fulltext => run_full_text_query(&table, &args, false).await?,
        SessionSearchMode::Fuzzy => run_full_text_query(&table, &args, true).await?,
        SessionSearchMode::Regex => run_plain_query(&table, &args).await?,
        SessionSearchMode::Text => run_plain_query(&table, &args).await?,
        SessionSearchMode::Hybrid => {
            if !query_enabled {
                run_plain_query(&table, &args).await?
            } else {
                let mut merged = Vec::new();
                let mut fts = run_full_text_query(&table, &args, false).await?;
                let mut fuzzy = run_full_text_query(&table, &args, true).await?;
                let mut plain = run_plain_query(&table, &args).await?;
                merged.append(&mut fts);
                merged.append(&mut fuzzy);
                merged.append(&mut plain);
                merged
            }
        }
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
                .map_err(|e| anyhow::anyhow!("invalid regex pattern: {e}"))?,
        )
    } else {
        None
    };

    for row in &mut candidates {
        if row.matched_by.is_empty() {
            row.matched_by.push(args.search_mode.as_str().to_string());
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
        {
            if haystack_norm.contains(&query_norm) {
                row.score += 10.0;
                if !row.matched_by.iter().any(|m| m == "text") {
                    row.matched_by.push("text".to_string());
                }
            }
        }

        if let Some(re) = &regex
            && re.is_match(&haystack)
        {
            row.score += 15.0;
            if !row.matched_by.iter().any(|m| m == "regex") {
                row.matched_by.push("regex".to_string());
            }
        }
    }

    candidates = candidates
        .into_iter()
        .filter(|row| {
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
        })
        .collect();

    let mut deduped: HashMap<String, SessionMatchRow> = HashMap::new();
    for row in candidates {
        let key = format!("{}:{}", row.session_id, row.message_id);
        match deduped.get_mut(&key) {
            Some(existing) => {
                if row.score > existing.score {
                    *existing = row;
                } else {
                    for source in row.matched_by {
                        if !existing.matched_by.iter().any(|s| s == &source) {
                            existing.matched_by.push(source);
                        }
                    }
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
    finalize_rows_with_total(rows, total_matches, &args)
}

fn apply_command_form(
    args: &mut SessionArgs,
    command: &str,
    command_name: &str,
) -> aither_core::Result<()> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!(
            "{command_name} command must not be empty"
        ));
    }

    let prefix = [command_name, " "].concat();
    let body = trimmed.strip_prefix(prefix.as_str()).unwrap_or(trimmed).trim();
    if body.is_empty() {
        return Err(anyhow::anyhow!(
            "{command_name} command must include a search prompt"
        ));
    }

    let mut tokens = body.split_whitespace();
    if let (Some(session_id), Some(keyword)) = (tokens.next(), tokens.next()) {
        if keyword == "search" {
            let prompt = tokens.collect::<Vec<_>>().join(" ");
            if prompt.is_empty() {
                return Err(anyhow::anyhow!(
                    "invalid session command: missing prompt after 'search'"
                ));
            }
            args.session_id = Some(session_id.to_string());
            args.query = prompt;
            return Ok(());
        }
    }

    args.query = body.to_string();
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionIndexMeta {
    session_count: usize,
    total_messages: usize,
    latest_updated_at: String,
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
    title: String,
    model: String,
    role: String,
    content: String,
    preview: String,
    timestamp: String,
    updated_at: String,
    message_id: i64,
    message_count: i32,
    score: f64,
    matched_by: Vec<String>,
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
    matched_by: Vec<String>,
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

async fn compute_fingerprint(db: &ChatDb) -> anyhow::Result<SessionIndexMeta> {
    let sessions = db.list_sessions().await?;
    let session_count = sessions.len();
    let total_messages: usize = sessions
        .iter()
        .map(|s| s.message_count.max(0) as usize)
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
    })
}

async fn ensure_index(
    index_path: &Path,
    meta_path: &Path,
    db: &ChatDb,
    fingerprint: &SessionIndexMeta,
) -> anyhow::Result<Table> {
    let conn = connect(index_path.to_string_lossy().as_ref())
        .execute()
        .await?;

    let mut should_rebuild = true;
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
                .create_index(&[column], Index::FTS(Default::default()))
                .execute()
                .await?;
        }
        table
            .create_index(&["session_id"], Index::BTree(Default::default()))
            .execute()
            .await?;
        table
            .create_index(&["model"], Index::BTree(Default::default()))
            .execute()
            .await?;
        table
            .create_index(&["role"], Index::BTree(Default::default()))
            .execute()
            .await?;

        async_fs::write(meta_path, serde_json::to_vec_pretty(fingerprint)?).await?;

        Ok(table)
    } else {
        Ok(conn.open_table(TABLE_NAME).execute().await?)
    }
}

async fn build_index_rows(db: &ChatDb) -> anyhow::Result<Vec<SessionIndexRow>> {
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
                message_id: msg.id as i64,
                message_count,
            });
        }
    }

    Ok(rows)
}

fn rows_to_record_reader(
    rows: &[SessionIndexRow],
) -> anyhow::Result<Box<dyn RecordBatchReader + Send>> {
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
            pattern: Box::new(string_sql_expr(["%", title_contains, "%"].concat())),
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
) -> anyhow::Result<Vec<SessionMatchRow>> {
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
) -> anyhow::Result<Vec<SessionMatchRow>> {
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
        fts = fts.with_columns(&fts_columns)?;
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
        row.matched_by
            .push(if fuzzy { "fuzzy" } else { "fulltext" }.to_string());
    }
    Ok(rows)
}

async fn read_rows_from_stream(
    stream: &mut lancedb::arrow::SendableRecordBatchStream,
) -> anyhow::Result<Vec<SessionMatchRow>> {
    let mut rows = Vec::new();

    while let Some(batch_res) = stream.next().await {
        let batch = batch_res?;

        let session_id = as_string(batch.column_by_name("session_id"), "session_id")?;
        let title = as_string(batch.column_by_name("title"), "title")?;
        let model = as_string(batch.column_by_name("model"), "model")?;
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
            .map(|arr| {
                (0..arr.len())
                    .map(|i| {
                        if arr.is_null(i) {
                            0.0
                        } else {
                            arr.value(i) as f64
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![0.0; batch.num_rows()]);

        for i in 0..batch.num_rows() {
            rows.push(SessionMatchRow {
                session_id: session_id.value(i).to_string(),
                title: title.value(i).to_string(),
                model: model.value(i).to_string(),
                role: role.value(i).to_string(),
                content: content.value(i).to_string(),
                preview: preview.value(i).to_string(),
                timestamp: timestamp.value(i).to_string(),
                updated_at: updated_at.value(i).to_string(),
                message_id: message_id.value(i),
                message_count: message_count.value(i),
                score: scores[i],
                matched_by: Vec::new(),
            });
        }
    }

    Ok(rows)
}

fn as_string<'a>(column: Option<&'a ArrayRef>, name: &str) -> anyhow::Result<&'a StringArray> {
    column
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid '{}' column", name))
}

fn as_i64<'a>(column: Option<&'a ArrayRef>, name: &str) -> anyhow::Result<&'a Int64Array> {
    column
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid '{}' column", name))
}

fn as_i32<'a>(column: Option<&'a ArrayRef>, name: &str) -> anyhow::Result<&'a Int32Array> {
    column
        .and_then(|c| c.as_any().downcast_ref::<Int32Array>())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid '{}' column", name))
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
    rows: Vec<SessionMatchRow>,
    total_matches: usize,
    args: &SessionArgs,
) -> anyhow::Result<ToolOutput> {
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

    ToolOutput::json(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut args = base_args();
        apply_command_form(&mut args, "session abc123 search investigate failures", "session")
            .expect("command form should parse");
        assert_eq!(args.session_id.as_deref(), Some("abc123"));
        assert_eq!(args.query, "investigate failures");
    }

    #[test]
    fn parses_global_session_command_form() {
        let mut args = base_args();
        apply_command_form(&mut args, "session recall onboarding decision", "session")
            .expect("global command form should parse");
        assert_eq!(args.session_id, None);
        assert_eq!(args.query, "recall onboarding decision");
    }

    #[test]
    fn rejects_missing_prompt_in_scoped_command() {
        let mut args = base_args();
        let err = apply_command_form(&mut args, "session abc123 search", "session")
            .expect_err("missing prompt must fail");
        assert!(
            err.to_string().contains("missing prompt"),
            "unexpected error: {err}"
        );
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
}
