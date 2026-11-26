//! Resident May Bot runtime and memory subsystem.
//!
//! Implements always-on non-bash-first personal bridge agents with git-backed
//! hierarchical Markdown memory, deduplicated hybrid RAG, delegated-worker
//! authority, and event-driven inbox triage.

pub mod approvals;
pub mod delegates;
pub mod memory;
pub mod runner;
pub mod tools;

#[cfg(test)]
mod integration_tests;

pub use approvals::{
    ApprovalOutcome, BotApprovalError, BotApprovalRouter, BotEventPublishFn, BotWakeFn,
    DelegatedWorkerApprover,
};
pub use delegates::{
    BashDelegateRunner, BotDelegateManager, DelegateError, DelegateRunResult, DelegateRunner,
    DelegateSpawnSpec, drive_delegate_agent, make_delegate_hooks,
};
pub use memory::{
    BOT_MEMORY_INDEX_CONTENT, BOT_MEMORY_INDEX_SOURCE, BotMemoryCuratorQueue, BotMemoryError,
    BotMemoryRag, ConfinedShellCuratorSession, CuratorFactory, CuratorSession, RecalledMemoryHit,
    ResolvedFactSource, blake3_hex, bot_memory_collection, commit_memory_staging_sync,
    ensure_bot_memory_initialized, list_memory_commits_sync, normalize_leaf_path,
    publish_staging_atomically, read_memory_tree, recall_leaf, record_leaf_seen,
    render_hierarchical_memory_index, reset_seen_window, resolve_store_fact_source,
    run_one_store_fact, verify_memory_tree,
};
pub use runner::{
    BotModelFactory, BotService, BotServiceDeps, BotServiceError, NoopDelegateRunner,
    NoopSessionBridge, bot_native_tool_names,
};
pub use tools::{BotToolContext, BotToolError, SessionBridge};
