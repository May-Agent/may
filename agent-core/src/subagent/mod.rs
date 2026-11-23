//! The `subagent` command and what it runs: in-process agents on aither's
//! subagent primitives, and connected external agents behind a protocol.

mod approvals;
mod args;
mod child;
mod command;
pub mod diff;
mod external;
mod finished;
mod message;
mod mounts;
mod report;
mod tier;

pub use approvals::{ChildApproval, ChildApprovals};
pub use args::SubagentArgs;
pub use child::{ChildHooks, ChildSession, KeepAlive};
pub use command::{
    FORK_DESCRIPTION, SubagentCommand, SubagentModelResolver, SubagentService, SubagentSettings,
};
pub use diff::{Baseline, WorktreeDiff};
pub use external::{ExternalAgent, ExternalAgents, ExternalStart};
pub use message::{SendMessageArgs, SendMessageTool};
pub use mounts::{PromptFiles, SubagentFileMount};
pub use tier::{Tier, TierModels};
