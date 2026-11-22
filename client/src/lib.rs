//! May's client side: the daemon API client, the session store and the
//! shared worker protocol the TUI and daemon exchange.

pub mod agent_environment;
pub mod bundle;
pub mod client;
pub mod code_search;
pub mod command_cards;
pub mod container;
pub mod daemon;
pub mod issue;
pub mod repo_clone;
pub use may_repo_search as repo_search;
pub mod session;
pub mod skill;
pub mod subagent_tracker;
pub mod tool_environment_cache;
pub mod trust;
/// The in-process event bus wiring the client parts.
pub mod ui_event_bus;
pub mod worker;
pub mod workspace;
