//! Host-brokered Debug Adapter Protocol (DAP) service and `debug` IPC command.
//!
//! Native debug adapters (`lldb-dap`, `gdb --interpreter=dap`, `dlv dap`) execute
//! on the host inside [`DebugBroker`] outside `heel`, while target debuggees run
//! inside the session's `Shell` sandbox. Python (`debugpy`) targets run inside the
//! sandbox and speak DAP over loopback TCP. Active sessions are tracked by real
//! `pid: u32` in `JobRegistry`, deliver one-shot asynchronous stop/termination
//! wakeups via [`crate::task_wake::SessionPrompts`], and integrate with
//! post-compaction live-resource recall and session teardown.

pub mod adapter;
pub mod broker;
pub mod context;
pub mod process;
pub mod tool;

pub use adapter::{AdapterError, AdapterKind, ResolvedNativeAdapter};
pub use broker::{DebugBroker, DebugCallContext, DebugOwner};
pub use context::DebugLiveContext;
pub use tool::MayDebugTool;
