//! May's foundational types: the ids, wire shapes and leaf utilities every
//! crate above it shares, with no dependency on `may-agent` itself.
//!
//! What lives here is what more than one layer reads: the model-facing ids
//! and word pools, the XML block and command-reading shapes clients render,
//! the persisted ask-user requests a refresh recovers, and the small
//! standalone utilities (panic boundary, log ring, quota decoding, file
//! tracking) that need nothing but the platform.

/// The `ask_user` wire types and their persisted shapes.
pub mod ask_user;
/// The `/btw` mailbox and side-request responses.
pub mod btw;
/// Provider prompt-cache lifetime.
pub mod cache_lifetime;
/// Provider prompt-cache miss accounting.
pub mod cache_miss;
/// The per-tool-call store of images an IPC command produced.
pub mod call_images;
/// The cloud-machine wire types profiles and sessions carry.
pub mod cloud;
/// Streamed command output for the model.
pub mod command_output;
/// How a terminal call's script is read before it runs.
pub mod command_reading;
/// The verbatim assembled-context dump of `/context dump`.
pub mod context_dump;
/// The assembled-context report of `/context`.
pub mod context_report;
/// The prompt-section tags a context layout orders, shared by the report.
pub mod context_tag;
/// Constants of the daemon's client-facing API.
pub mod daemon_api;
/// Live decode-speed measurement for streamed model output.
pub mod decode_meter;
/// Disk-exhaustion classification shared by the database, shells and daemon.
pub mod disk_exhaustion;
/// Per-session file-change tracking backed by detached git trees.
pub mod file_tracker;
/// Model-facing ids: session names and each session's pool of words.
pub mod id;
/// The issue-tracking wire types: states, summaries, comments and targets.
pub mod issue;
/// The issues a session's task rows name, as the wire carries them.
pub mod issue_brief;
/// Layout constants shared across the workspace.
pub mod layout_consts;
/// An assistant response as it streams: the live row and the settled one.
pub mod live_response;
/// Location lookup for location-aware profiles.
pub mod location;
/// A ring buffer of the tracing events the daemon writes, dumped on demand.
pub mod log_ring;
/// Model-facing ids: session names and each session's pool of words.
pub mod model_ids;
/// Model selection and filtering helpers.
pub mod model_selector;
/// A model and the `Parameters` of the requests sent to it, paired.
pub mod model_slot;
/// The web-push notification wire types.
pub mod notification;
/// Catches a panic at the boundary of the operation that raised it.
pub mod panic_boundary;
/// The permission-request modes persisted with a session.
pub mod permission;
/// The turn-progress snapshot the events stream carries.
/// The PR check-summary wire types.
pub mod pr_checks;
/// The profile wire types: preferences, environments and pinned items.
pub mod profile;
/// The turn-progress snapshot the events stream carries.
pub mod progress;
/// Provider quota and rate-limit decoding.
pub mod quota;
/// The recap a turn writes for a later one to open with.
pub mod recap;
/// The `/import-claude` / `/import-codex` request and response.
pub mod rules_import;
/// The secret types the wire shares; the store is `may-config`'s.
pub mod secrets;
/// The per-session change notices the events stream diffs by.
pub mod session_change;
/// The foreign agents a May session can be seeded from.
pub mod session_import;
/// The home and session paths of a profile's layout.
pub mod session_layout;
/// The chat-message wire types persisted in the history database.
pub mod session_messages;
/// The session's plan mode.
pub mod session_plan;
/// The session's persisted runtime state and server-owned pending messages.
pub mod session_runtime;
/// The session's persisted id and its list-row summary.
pub mod session_summary;
/// The per-model usage report a session's `/usage` assembles.
pub mod session_usage;
/// The sleep/shutdown status the daemon reports.
pub mod sleep;
/// The subagent-run snapshots the tracker reports.
pub mod subagent_snapshot;
/// The rows of a session's `tasks.md` as clients see them.
pub mod task_rows;
/// The lifecycle states of a scheduled task's run.
pub mod task_run;
/// The tool-result wire shape transcripts persist.
pub mod tool_result_wire;
/// The persisted trust level of one repository.
pub mod trust;
/// Wall-clock accounting of a turn's phases.
pub mod wall_clock;
/// XML block serialization for prompt/context payloads.
pub mod xml;

/// Test support shared by the workspace's `#[cfg(test)]` code.
#[cfg(feature = "testing")]
pub mod test_support;
