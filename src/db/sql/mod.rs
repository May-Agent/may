pub const CHAT_SCHEMA: &str = include_str!("chat_schema.sql");
pub const SESSION_CONTEXT_SNAPSHOT_SCHEMA: &str =
    include_str!("session_context_snapshot_schema.sql");

pub const SESSION_DELETE: &str = include_str!("session_delete.sql");
pub const SEARCH_OUTBOX_SCHEMA: &str = include_str!("search_outbox.sql");
pub const SESSION_INTERVALS_SCHEMA: &str = include_str!("session_intervals.sql");

/// Statements separated by a blank line, for scripts whose statements contain
/// `;` themselves (trigger bodies).
pub fn blocks(contents: &str) -> impl Iterator<Item = &str> {
    contents
        .split("\n\n")
        .map(str::trim)
        .filter(|part| !part.is_empty())
}

pub fn statements(contents: &str) -> impl Iterator<Item = &str> {
    contents
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
}

pub const CACHE_MISS_INSERT: &str = include_str!("cache_miss_insert.sql");
pub const PROMPT_CACHE_ROUTE_UPSERT: &str = include_str!("prompt_cache_route_upsert.sql");
pub const MESSAGE_SCAN: &str = include_str!("message_scan.sql");
pub const REQUEST_LOG_INSERT: &str = include_str!("request_log_insert.sql");
pub const NETWORK_ACCESS_LOG_INSERT: &str = include_str!("network_access_log_insert.sql");
pub const REPO_USAGE_TOUCH: &str = include_str!("repo_usage_touch.sql");
pub const REPO_USAGE_SINCE: &str = include_str!("repo_usage_since.sql");
pub const INDEXED_MESSAGES: &str = include_str!("indexed_messages.sql");
