pub const CHAT_SCHEMA: &str = include_str!("chat_schema.sql");
pub const SESSION_CONTEXT_SNAPSHOT_COLUMNS: &str =
    include_str!("session_context_snapshot_columns.sql");
pub const SESSION_CONTEXT_SNAPSHOT_DROP: &str = include_str!("session_context_snapshot_drop.sql");
pub const SESSION_CONTEXT_SNAPSHOT_SCHEMA: &str =
    include_str!("session_context_snapshot_schema.sql");

pub fn statements(contents: &str) -> impl Iterator<Item = &str> {
    contents
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
}
