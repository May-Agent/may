//! May's agent harness: everything the session's agent loop and its IPC
//! command bridge share — the command registry, working documents, host-load
//! and language guards, the OAuth provider helpers, scheduled-task triggers
//! and the self-update machinery.

/// What May's agents do around the loop.
pub mod harness;
/// May's IPC command bridge.
pub mod ipc;
/// `tasks.md`: its format and the harness that keeps it current.
pub mod tasks_md;
/// The rows of `tasks.md` as clients see them.
pub use may_protocol::task_rows;
/// `ChatGPT` subscription OAuth flow.
pub mod chatgpt;
/// GitHub Copilot OAuth flow.
pub mod copilot;
/// Crash-time signal handling.
#[cfg(unix)]
pub mod crash_signal;
/// xAI Grok subscription OAuth flow.
pub mod grok;
/// MCP server configuration, connections, and management.
pub mod mcp;
/// The preflight checks a repository clone runs against.
pub mod repo_preflight;
/// Scheduled-task trigger definitions.
pub mod task_trigger;
/// Self-update from CI artifacts (dev builds) or GitHub releases.
pub mod update;

/// The layers below this crate, under the paths the modules were written
/// against.
pub use may_config::auth_store;
pub use may_config::commit_signing;
pub use may_config::config;
pub use may_config::forge;
pub use may_config::host_git;
pub use may_config::secrets;
pub use may_protocol::call_images;
pub use may_protocol::id;
pub use may_protocol::log_ring;
pub use may_protocol::model_slot;

/// The `ask_user` broker the preflight questions go through.
pub use harness::ask_user;
/// The workspace-request broker.
pub use harness::workspace_request;
