//! The `subagent` command and what it runs: in-process agents on aither's
//! subagent primitives, and connected external agents behind a protocol.

mod args;
mod child;
mod command;
mod external;
mod finished;
mod mounts;
mod report;
mod tier;

pub use args::SubagentArgs;
pub use child::{ChildHooks, ChildSession, KeepAlive};
pub use command::{FORK_DESCRIPTION, SubagentCommand, SubagentService, SubagentSettings};
pub use external::{ExternalAgent, ExternalAgents, ExternalStart};
pub use mounts::{PromptFiles, SubagentFileMount};
pub use tier::{Tier, TierModels};
