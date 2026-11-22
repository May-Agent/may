//! May - A warm, boundaried AI assistant.
//!
//! `may-agent` is the assembly layer: the whole library surface lives in
//! [`may_agent_core`] and [`may_daemon`], re-exported here under the paths
//! the `may` binary, the TUI and the integration tests were written against.

pub use may_agent_core::*;
/// The resident daemon that runs every agent loop and serves the UIs.
pub use may_daemon as daemon;
/// Scheduled task execution and tool request dispatch.
pub use may_daemon::{scheduler, tool_dispatch};
/// Terminal user interface.
pub use may_tui as tui;
