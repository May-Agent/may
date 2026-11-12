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
