use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aither_cloud::CloudEmbedder;
use aither_core::llm::{Tool, ToolOutput};
use aither_rag::{Document, Rag};
use arrow_array::{
    Array, ArrayRef, Float32Array, Int64Array, RecordBatch, RecordBatchIterator, RecordBatchReader,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures_lite::StreamExt;
use ignore::WalkBuilder;
use lance_index::scalar::FullTextSearchQuery;
use lancedb::index::Index;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::{Table, connect};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlparser::ast::{BinaryOperator, Expr as SqlExpr, Ident, Value as SqlValue};

use crate::agent::command_words::split_shell_words;
use crate::agent::search_match::push_unique;

const TABLE_NAME: &str = "file_chunks";
const INDEX_META_FILE: &str = "search_index.meta.json";
const MAX_FILE_BYTES: usize = 1024 * 1024;
const CHUNK_TARGET_CHARS: usize = 2400;
const CHUNK_OVERLAP_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchAction {
    Index,
    Query,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Hybrid,
    Text,
    Regex,
    Fulltext,
    Fuzzy,
    Semantic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchOutputMode {
    Files,
    Chunks,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchArgs {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default = "default_action")]
    pub action: SearchAction,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default = "default_search_mode")]
    pub search_mode: SearchMode,
    #[serde(default = "default_output_mode")]
    pub output_mode: SearchOutputMode,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_scan_limit")]
    pub scan_limit: usize,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default = "default_fuzzy_distance")]
    pub fuzzy_distance: u32,
    #[serde(default)]
    pub include_content: bool,
}

const fn default_action() -> SearchAction {
    SearchAction::Query
}

const fn default_search_mode() -> SearchMode {
    SearchMode::Hybrid
}

const fn default_output_mode() -> SearchOutputMode {
    SearchOutputMode::Files
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

#[derive(Debug, Clone)]
pub struct SearchTool {
    sandbox_dir: PathBuf,
    embedder: Option<CloudEmbedder>,
}

impl SearchTool {
    pub const fn new(sandbox_dir: PathBuf, embedder: Option<CloudEmbedder>) -> Self {
        Self {
            sandbox_dir,
            embedder,
        }
    }

    fn index_dir(&self) -> PathBuf {
        self.sandbox_dir.join("data").join("search")
    }

    fn db_path(&self) -> PathBuf {
        self.index_dir().join("index.lancedb")
    }

    fn meta_path(&self) -> PathBuf {
        self.index_dir().join(INDEX_META_FILE)
    }

    fn semantic_index_path(&self) -> PathBuf {
        self.index_dir().join("semantic.redb")
    }

    fn normalize_args(&self, mut args: SearchArgs) -> anyhow::Result<SearchArgs> {
        if let Some(command) = args.command.take() {
            apply_command_form(&mut args, &command)?;
        }

        args.limit = args.limit.clamp(1, 200);
        args.scan_limit = args.scan_limit.clamp(args.limit, 50_000);
        args.fuzzy_distance = args.fuzzy_distance.min(3);

        if args.action == SearchAction::Index {
            let Some(path) = args
                .path
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                return Err(anyhow::anyhow!("search index requires a non-empty path"));
            };
            args.path = Some(path.to_string());
        } else {
            let Some(query) = args
                .query
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                return Err(anyhow::anyhow!("search query requires a non-empty query"));
            };
            args.query = Some(query.to_string());
        }

        Ok(args)
    }
}

impl Tool for SearchTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("search")
    }

    type Arguments = SearchArgs;

    async fn call(&self, arguments: Self::Arguments) -> aither_core::Result<ToolOutput> {
        let args = self.normalize_args(arguments)?;
        if args.action == SearchAction::Index {
            return index_path(
                self.db_path(),
                self.meta_path(),
                self.semantic_index_path(),
                self.embedder.clone(),
                args.path.expect("validated"),
            )
            .await;
        }
        query_index(
            self.db_path(),
            self.meta_path(),
            self.semantic_index_path(),
            self.embedder.clone(),
            args,
        )
        .await
    }
}

pub fn apply_command_form(args: &mut SearchArgs, command: &str) -> anyhow::Result<()> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("search command must not be empty"));
    }
    let body = trimmed.strip_prefix("search ").unwrap_or(trimmed).trim();
    if body.is_empty() {
        return Err(anyhow::anyhow!(
            "search command must include an action or query"
        ));
    }

    let tokens = split_shell_words(body, "search")?;
    match tokens.as_slice() {
        [action, path] if action == "index" => {
            args.action = SearchAction::Index;
            args.path = Some(path.clone());
        }
        [action, ..] if action == "index" => {
            return Err(anyhow::anyhow!(
                "search index accepts exactly one path argument"
            ));
        }
        [action, prompt_tokens @ ..] if action == "query" => {
            if prompt_tokens.is_empty() {
                return Err(anyhow::anyhow!("search query requires a prompt"));
            }
            args.action = SearchAction::Query;
            args.query = Some(prompt_tokens.join(" "));
        }
        _ => {
            args.action = SearchAction::Query;
            args.query = Some(tokens.join(" "));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SearchIndexMeta {
    roots: Vec<String>,
    files_indexed: usize,
    chunks_indexed: usize,
    indexed_at: String,
}

#[derive(Debug, Clone)]
struct FileChunkRow {
    root_path: String,
    file_path: String,
    relative_path: String,
    file_name: String,
    modified_at: String,
    chunk_id: i64,
    content: String,
}

#[derive(Debug, Clone)]
struct SearchableFile {
    root_path: String,
    file_path: String,
    relative_path: String,
    file_name: String,
    modified_at: String,
    content: String,
}

#[derive(Debug, Clone)]
struct FileMatchRow {
    file_path: String,
    relative_path: String,
    modified_at: String,
    chunk_id: i64,
    content: String,
    score: f64,
    matched_by: Vec<SearchMode>,
}

#[derive(Debug, Clone, Serialize)]
struct ChunkResult {
    file_path: String,
    relative_path: String,
    modified_at: String,
    chunk_id: i64,
    score: f64,
    matched_by: Vec<SearchMode>,
    snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct FileResult {
    file_path: String,
    relative_path: String,
    modified_at: String,
    match_count: usize,
    top_score: f64,
    highlights: Vec<ChunkResult>,
}

async fn index_path(
    db_path: PathBuf,
    meta_path: PathBuf,
    semantic_index_path: PathBuf,
    embedder: Option<CloudEmbedder>,
    raw_path: String,
) -> anyhow::Result<ToolOutput> {
    let canonical = async_fs::canonicalize(PathBuf::from(raw_path.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("failed to resolve search path {raw_path}: {e}"))?;

    let mut meta = load_meta(&meta_path).await?;
    let canonical_string = canonical.display().to_string();
    if !meta.roots.iter().any(|root| root == &canonical_string) {
        meta.roots.push(canonical_string.clone());
        meta.roots.sort_unstable();
    }

    let files = collect_searchable_files(&meta.roots).await?;
    let rows = build_rows(&files);
    if rows.is_empty() {
        return Err(anyhow::anyhow!(
            "search index contains no text chunks; choose a text or code file/directory"
        ));
    }

    async_fs::create_dir_all(
        meta_path
            .parent()
            .expect("search metadata path must have a parent"),
    )
    .await?;

    let conn = connect(db_path.to_string_lossy().as_ref())
        .execute()
        .await?;
    let table_names = conn.table_names().execute().await?;
    if table_names.iter().any(|name| name == TABLE_NAME) {
        conn.drop_table(TABLE_NAME, &[]).await?;
    }

    let reader = rows_to_record_reader(&rows)?;
    let table = conn.create_table(TABLE_NAME, reader).execute().await?;
    for column in ["content", "relative_path", "file_name"] {
        table
            .create_index(&[column], Index::FTS(Default::default()))
            .execute()
            .await?;
    }
    table
        .create_index(&["file_path"], Index::BTree(Default::default()))
        .execute()
        .await?;

    meta.files_indexed = rows
        .iter()
        .map(|row| row.file_path.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    meta.chunks_indexed = rows.len();
    meta.indexed_at = jiff::Timestamp::now().to_string();
    async_fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?).await?;

    let semantic_indexed = if let Some(embedder) = embedder {
        rebuild_semantic_index(&semantic_index_path, embedder, &files).await?
    } else {
        false
    };

    ToolOutput::json(&serde_json::json!({
        "action": "index",
        "indexed_root": canonical_string,
        "roots": meta.roots,
        "files_indexed": meta.files_indexed,
        "chunks_indexed": meta.chunks_indexed,
        "semantic_indexed": semantic_indexed,
        "indexed_at": meta.indexed_at,
    }))
}

async fn load_meta(meta_path: &Path) -> anyhow::Result<SearchIndexMeta> {
    match async_fs::read_to_string(meta_path).await {
        Ok(content) => Ok(serde_json::from_str(&content)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SearchIndexMeta::default())
        }
        Err(error) => Err(anyhow::anyhow!(
            "failed to read search index metadata {}: {error}",
            meta_path.display()
        )),
    }
}

async fn rebuild_semantic_index(
    semantic_index_path: &Path,
    embedder: CloudEmbedder,
    files: &[SearchableFile],
) -> anyhow::Result<bool> {
    match async_fs::remove_file(semantic_index_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::anyhow!(
                "failed to remove stale semantic index {}: {error}",
                semantic_index_path.display()
            ));
        }
    }

    let rag = Rag::builder(embedder)
        .index_path(semantic_index_path.to_path_buf())
        .code_chunking(CHUNK_TARGET_CHARS)
        .top_k(200)
        .deduplication(false)
        .auto_save(false)
        .build()?;

    let mut indexed_chunks = 0usize;
    for file in files {
        let mut metadata = aither_rag::Metadata::new();
        metadata.insert("path".to_string(), file.file_path.clone());
        metadata.insert("root_path".to_string(), file.root_path.clone());
        metadata.insert("file_path".to_string(), file.file_path.clone());
        metadata.insert("relative_path".to_string(), file.relative_path.clone());
        metadata.insert("modified_at".to_string(), file.modified_at.clone());
        metadata.insert("file_name".to_string(), file.file_name.clone());

        indexed_chunks += rag
            .insert(Document::with_metadata(
                file.file_path.clone(),
                file.content.clone(),
                metadata,
            ))
            .await?;
    }

    rag.save()?;
    Ok(indexed_chunks > 0)
}

async fn query_index(
    db_path: PathBuf,
    meta_path: PathBuf,
    semantic_index_path: PathBuf,
    embedder: Option<CloudEmbedder>,
    args: SearchArgs,
) -> anyhow::Result<ToolOutput> {
    let meta = load_meta(&meta_path).await?;
    if meta.roots.is_empty() {
        return Err(anyhow::anyhow!(
            "search index is empty; run `search index <path>` first"
        ));
    }

    let conn = connect(db_path.to_string_lossy().as_ref())
        .execute()
        .await?;
    let table =
        conn.open_table(TABLE_NAME).execute().await.map_err(|_| {
            anyhow::anyhow!("search index is missing; run `search index <path>` again")
        })?;

    let query = args.query.clone().expect("validated");
    let query_enabled = query.trim() != "*" && !query.trim().is_empty();
    if args.search_mode == SearchMode::Semantic {
        let embedder = embedder.ok_or_else(|| {
            anyhow::anyhow!(
                "semantic search requires an embedding-capable provider; configure OpenAI-compatible or Google embeddings"
            )
        })?;
        return run_semantic_query(semantic_index_path, embedder, args, meta).await;
    }

    let mut candidates = match args.search_mode {
        SearchMode::Fulltext => run_full_text_query(&table, &args, false).await?,
        SearchMode::Fuzzy => run_full_text_query(&table, &args, true).await?,
        SearchMode::Regex => run_plain_query(&table, &args).await?,
        SearchMode::Text => run_plain_query(&table, &args).await?,
        SearchMode::Hybrid => {
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
        SearchMode::Semantic => unreachable!("semantic search returns earlier"),
    };

    let query_norm = if args.case_sensitive {
        query.clone()
    } else {
        query.to_lowercase()
    };

    let regex_enabled =
        args.search_mode == SearchMode::Regex && (query_enabled || args.regex.is_some());
    let regex = if regex_enabled {
        let pattern = args.regex.as_deref().unwrap_or(query.as_str()).to_string();
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
            push_unique(&mut row.matched_by, args.search_mode);
        }

        let haystack = [row.relative_path.as_str(), row.content.as_str()].join("\n");
        let haystack_norm = if args.case_sensitive {
            haystack.clone()
        } else {
            haystack.to_lowercase()
        };
        if query_enabled
            && args.search_mode != SearchMode::Fulltext
            && args.search_mode != SearchMode::Fuzzy
            && haystack_norm.contains(&query_norm)
        {
            row.score += 10.0;
            push_unique(&mut row.matched_by, SearchMode::Text);
        }
        if let Some(re) = &regex
            && re.is_match(&haystack)
        {
            row.score += 15.0;
            push_unique(&mut row.matched_by, SearchMode::Regex);
        }
    }

    candidates.retain(|row| {
        let haystack = [row.relative_path.as_str(), row.content.as_str()].join("\n");
        if query_enabled && args.search_mode == SearchMode::Text {
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
            return re.is_match(&haystack);
        }
        true
    });

    let mut deduped: HashMap<String, FileMatchRow> = HashMap::new();
    for row in candidates {
        let key = format!("{}:{}", row.file_path, row.chunk_id);
        match deduped.get_mut(&key) {
            Some(existing) => {
                if row.score > existing.score {
                    *existing = row;
                }
            }
            None => {
                deduped.insert(key, row);
            }
        }
    }

    let mut rows: Vec<FileMatchRow> = deduped.into_values().collect();
    rows.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_path.cmp(&b.file_path))
    });
    let total_matches = rows.len();
    finalize_rows(rows, total_matches, &args, &meta)
}

fn require_semantic_metadata<'a>(
    metadata: &'a aither_rag::Metadata,
    key: &str,
) -> anyhow::Result<&'a str> {
    metadata
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| anyhow::anyhow!("semantic index entry is missing '{key}' metadata"))
}

fn matches_path_prefix(path_filter: Option<&str>, file_path: &str) -> bool {
    path_filter.is_none_or(|prefix| file_path.starts_with(prefix))
}

async fn run_semantic_query(
    semantic_index_path: PathBuf,
    embedder: CloudEmbedder,
    args: SearchArgs,
    meta: SearchIndexMeta,
) -> anyhow::Result<ToolOutput> {
    let query = args.query.as_deref().expect("validated");
    if query == "*" {
        return Err(anyhow::anyhow!(
            "semantic search does not support wildcard queries"
        ));
    }

    match async_fs::metadata(&semantic_index_path).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow::anyhow!(
                "semantic index is missing; run `search index <path>` again"
            ));
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "failed to inspect semantic index {}: {error}",
                semantic_index_path.display()
            ));
        }
    }

    let rag = Rag::builder(embedder)
        .index_path(semantic_index_path)
        .code_chunking(CHUNK_TARGET_CHARS)
        .top_k(args.scan_limit)
        .deduplication(false)
        .auto_save(false)
        .build()?;
    rag.load()?;

    let path_filter = args
        .path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let rows = rag
        .search_with_k(query, args.scan_limit)
        .await?
        .into_iter()
        .map(|result| {
            let file_path = require_semantic_metadata(&result.chunk.metadata, "file_path")?;
            if !matches_path_prefix(path_filter, file_path) {
                return Ok(None);
            }

            let relative_path = require_semantic_metadata(&result.chunk.metadata, "relative_path")?;
            let modified_at = require_semantic_metadata(&result.chunk.metadata, "modified_at")?;

            Ok(Some(FileMatchRow {
                file_path: file_path.to_string(),
                relative_path: relative_path.to_string(),
                modified_at: modified_at.to_string(),
                chunk_id: result.chunk.index as i64,
                content: result.chunk.text,
                score: f64::from(result.score),
                matched_by: vec![SearchMode::Semantic],
            }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let total_matches = rows.len();
    finalize_rows(rows, total_matches, &args, &meta)
}

fn build_sql_filter(args: &SearchArgs) -> Option<String> {
    let mut filters = Vec::new();
    if let Some(path) = args
        .path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        filters.push(SqlExpr::Like {
            negated: false,
            any: false,
            expr: Box::new(identifier_sql_expr("file_path")),
            pattern: Box::new(string_sql_expr(prefix_match_pattern(path))),
            escape_char: None,
        });
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

fn prefix_match_pattern(value: &str) -> String {
    let mut pattern = String::with_capacity(value.len() + 1);
    pattern.push_str(value);
    pattern.push('%');
    pattern
}

async fn run_plain_query(table: &Table, args: &SearchArgs) -> anyhow::Result<Vec<FileMatchRow>> {
    let mut query = table
        .query()
        .select(Select::columns(&[
            "file_path",
            "relative_path",
            "modified_at",
            "chunk_id",
            "content",
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
    args: &SearchArgs,
    fuzzy: bool,
) -> anyhow::Result<Vec<FileMatchRow>> {
    let query_text = args.query.clone().expect("validated");
    if query_text.trim() == "*" || query_text.trim().is_empty() {
        return run_plain_query(table, args).await;
    }

    let fts = if fuzzy {
        FullTextSearchQuery::new_fuzzy(query_text, Some(args.fuzzy_distance))
    } else {
        FullTextSearchQuery::new(query_text)
    }
    .with_columns(&[
        "content".to_string(),
        "relative_path".to_string(),
        "file_name".to_string(),
    ])?;

    let mut query = table
        .query()
        .full_text_search(fts)
        .select(Select::columns(&[
            "file_path",
            "relative_path",
            "modified_at",
            "chunk_id",
            "content",
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
                SearchMode::Fuzzy
            } else {
                SearchMode::Fulltext
            },
        );
    }
    Ok(rows)
}

async fn read_rows_from_stream(
    stream: &mut lancedb::arrow::SendableRecordBatchStream,
) -> anyhow::Result<Vec<FileMatchRow>> {
    let mut rows = Vec::new();
    while let Some(batch_res) = stream.next().await {
        let batch = batch_res?;
        let file_path = as_string(batch.column_by_name("file_path"), "file_path")?;
        let relative_path = as_string(batch.column_by_name("relative_path"), "relative_path")?;
        let modified_at = as_string(batch.column_by_name("modified_at"), "modified_at")?;
        let chunk_id = as_i64(batch.column_by_name("chunk_id"), "chunk_id")?;
        let content = as_string(batch.column_by_name("content"), "content")?;

        let score_col = batch.column_by_name("_score");
        let scores = score_col
            .and_then(|column| column.as_any().downcast_ref::<Float32Array>())
            .map(|array| {
                (0..array.len())
                    .map(|idx| {
                        if array.is_null(idx) {
                            0.0
                        } else {
                            f64::from(array.value(idx))
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![0.0; batch.num_rows()]);

        for idx in 0..batch.num_rows() {
            rows.push(FileMatchRow {
                file_path: file_path.value(idx).to_string(),
                relative_path: relative_path.value(idx).to_string(),
                modified_at: modified_at.value(idx).to_string(),
                chunk_id: chunk_id.value(idx),
                content: content.value(idx).to_string(),
                score: scores[idx],
                matched_by: Vec::new(),
            });
        }
    }
    Ok(rows)
}

fn as_string<'a>(column: Option<&'a ArrayRef>, name: &str) -> anyhow::Result<&'a StringArray> {
    column
        .and_then(|value| value.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid '{name}' column"))
}

fn as_i64<'a>(column: Option<&'a ArrayRef>, name: &str) -> anyhow::Result<&'a Int64Array> {
    column
        .and_then(|value| value.as_any().downcast_ref::<Int64Array>())
        .ok_or_else(|| anyhow::anyhow!("missing or invalid '{name}' column"))
}

async fn collect_searchable_files(roots: &[String]) -> anyhow::Result<Vec<SearchableFile>> {
    let mut files_out = Vec::new();
    for root in roots {
        let root_path = PathBuf::from(root);
        if async_fs::metadata(&root_path).await?.is_file() {
            if let Some(file) =
                read_searchable_file(root_path.as_path(), root_path.as_path()).await?
            {
                files_out.push(file);
            }
            continue;
        }

        let root_path_clone = root_path.clone();
        let files = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<PathBuf>> {
            let mut files = Vec::new();
            let mut walk = WalkBuilder::new(&root_path_clone);
            walk.standard_filters(true);
            walk.hidden(false);
            for entry in walk.build() {
                let entry = entry?;
                if entry.file_type().is_some_and(|kind| kind.is_file()) {
                    files.push(entry.into_path());
                }
            }
            Ok(files)
        })
        .await??;

        for file in files {
            if let Some(searchable) =
                read_searchable_file(root_path.as_path(), file.as_path()).await?
            {
                files_out.push(searchable);
            }
        }
    }
    Ok(files_out)
}

fn build_rows(files: &[SearchableFile]) -> Vec<FileChunkRow> {
    files
        .iter()
        .flat_map(|file| {
            chunk_text(file.content.as_str())
                .into_iter()
                .enumerate()
                .map(|(idx, chunk)| FileChunkRow {
                    root_path: file.root_path.clone(),
                    file_path: file.file_path.clone(),
                    relative_path: file.relative_path.clone(),
                    file_name: file.file_name.clone(),
                    modified_at: file.modified_at.clone(),
                    chunk_id: idx as i64,
                    content: chunk,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

async fn read_searchable_file(
    root_path: &Path,
    file_path: &Path,
) -> anyhow::Result<Option<SearchableFile>> {
    let bytes = async_fs::read(file_path).await?;
    if bytes.len() > MAX_FILE_BYTES || is_probably_binary(&bytes) {
        return Ok(None);
    }

    let content = String::from_utf8_lossy(&bytes).to_string();
    if content.trim().is_empty() {
        return Ok(None);
    }

    let metadata = async_fs::metadata(file_path).await?;
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_default();
    let relative_path = file_path
        .strip_prefix(root_path)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(file_path)
        .display()
        .to_string();
    let file_name = file_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(relative_path.as_str())
        .to_string();

    Ok(Some(SearchableFile {
        root_path: root_path.display().to_string(),
        file_path: file_path.display().to_string(),
        relative_path,
        file_name,
        modified_at,
        content,
    }))
}

pub fn chunk_text(content: &str) -> Vec<String> {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= CHUNK_TARGET_CHARS {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + CHUNK_TARGET_CHARS).min(chars.len());
        chunks.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(CHUNK_OVERLAP_CHARS);
    }
    chunks
}

fn is_probably_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(4096).any(|byte| *byte == 0)
}

fn rows_to_record_reader(
    rows: &[FileChunkRow],
) -> anyhow::Result<Box<dyn RecordBatchReader + Send>> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("root_path", DataType::Utf8, false),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("relative_path", DataType::Utf8, false),
        Field::new("file_name", DataType::Utf8, false),
        Field::new("modified_at", DataType::Utf8, false),
        Field::new("chunk_id", DataType::Int64, false),
        Field::new("content", DataType::Utf8, false),
    ]));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.root_path.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.file_path.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.relative_path.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.file_name.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.modified_at.as_str()),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.chunk_id),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.content.as_str()),
            )),
        ],
    )?;

    Ok(Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema)))
}

fn finalize_rows(
    rows: Vec<FileMatchRow>,
    total_matches: usize,
    args: &SearchArgs,
    meta: &SearchIndexMeta,
) -> anyhow::Result<ToolOutput> {
    let start = args.offset.min(rows.len());
    let end = (start + args.limit).min(rows.len());
    let rows = rows[start..end].to_vec();

    let query = args.query.clone().expect("validated");
    let value = if args.output_mode == SearchOutputMode::Chunks {
        let items = rows
            .into_iter()
            .map(|row| ChunkResult {
                file_path: row.file_path,
                relative_path: row.relative_path,
                modified_at: row.modified_at,
                chunk_id: row.chunk_id,
                score: (row.score * 100.0).round() / 100.0,
                matched_by: row.matched_by,
                snippet: truncate_text(row.content.as_str(), 240),
                content: args.include_content.then_some(row.content),
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "action": "query",
            "query": query,
            "search_mode": args.search_mode,
            "output_mode": args.output_mode,
            "indexed_roots": meta.roots,
            "total_matches": total_matches,
            "returned": items.len(),
            "results": items,
        })
    } else {
        let mut grouped = HashMap::<String, FileResult>::new();
        for row in rows {
            let entry = grouped
                .entry(row.file_path.clone())
                .or_insert_with(|| FileResult {
                    file_path: row.file_path.clone(),
                    relative_path: row.relative_path.clone(),
                    modified_at: row.modified_at.clone(),
                    match_count: 0,
                    top_score: row.score,
                    highlights: Vec::new(),
                });
            entry.match_count += 1;
            if row.score > entry.top_score {
                entry.top_score = row.score;
            }
            if entry.highlights.len() < 3 {
                entry.highlights.push(ChunkResult {
                    file_path: row.file_path,
                    relative_path: row.relative_path,
                    modified_at: row.modified_at,
                    chunk_id: row.chunk_id,
                    score: (row.score * 100.0).round() / 100.0,
                    matched_by: row.matched_by,
                    snippet: truncate_text(row.content.as_str(), 240),
                    content: args.include_content.then_some(row.content),
                });
            }
        }
        let mut items = grouped.into_values().collect::<Vec<_>>();
        items.sort_by(|left, right| {
            right
                .top_score
                .partial_cmp(&left.top_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.file_path.cmp(&right.file_path))
        });
        serde_json::json!({
            "action": "query",
            "query": query,
            "search_mode": args.search_mode,
            "output_mode": args.output_mode,
            "indexed_roots": meta.roots,
            "total_matches": total_matches,
            "returned": items.len(),
            "results": items,
        })
    };

    ToolOutput::json(&value)
}

fn truncate_text(input: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            output.push_str("...");
            break;
        }
        output.push(ch);
    }
    output
}
