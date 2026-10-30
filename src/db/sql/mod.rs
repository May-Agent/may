pub const CHAT_SCHEMA: &str = include_str!("chat_schema.sql");
pub const SESSION_CONTEXT_SNAPSHOT_COLUMNS: &str =
    include_str!("session_context_snapshot_columns.sql");
pub const SESSION_CONTEXT_SNAPSHOT_DROP: &str = include_str!("session_context_snapshot_drop.sql");
pub const SESSION_CONTEXT_SNAPSHOT_SCHEMA: &str =
    include_str!("session_context_snapshot_schema.sql");

pub const SESSION_RUN_LEASES_COLUMNS: &str = include_str!("session_run_leases_columns.sql");
pub const SESSION_RUN_LEASES_DROP: &str = include_str!("session_run_leases_drop.sql");
pub const SESSION_RUN_LEASES_SCHEMA: &str = include_str!("session_run_leases_schema.sql");

pub const SEARCH_OUTBOX_SCHEMA: &str = include_str!("search_outbox.sql");
pub const SEARCH_OUTBOX_BACKFILL: &str = include_str!("search_outbox_backfill.sql");

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
