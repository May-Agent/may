//! The connected external agents a session can run as subagents.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::acp_subagent::AcpSubagent;

/// What an external session starts from.
#[derive(Debug)]
pub struct ExternalStart<'a> {
    /// Where the call was made from: the session's working directory.
    pub working_dir: &'a Path,
    /// The directories granted for writing.
    pub worktrees: &'a [PathBuf],
    /// The directories granted for committing in.
    pub commit_worktrees: &'a [PathBuf],
    /// The working copies the run's space was prepared with.
    pub working_copies: &'a [PathBuf],
}

/// One connected agent.
#[derive(Debug)]
pub struct ExternalAgent {
    /// What it is for, as the delegating model reads it.
    pub description: String,
    /// The backend that opens its sessions.
    pub backend: AcpSubagent,
}

/// The connected agents of a session, by name.
#[derive(Debug, Clone, Default)]
pub struct ExternalAgents {
    agents: Arc<BTreeMap<String, ExternalAgent>>,
}

impl ExternalAgents {
    /// The agents `agents` holds.
    #[must_use]
    pub fn new(agents: BTreeMap<String, ExternalAgent>) -> Self {
        Self {
            agents: Arc::new(agents),
        }
    }

    /// The agent connected as `name`, or why none is.
    ///
    /// # Errors
    /// Returns what the model reads: which agents are connected.
    pub fn get(&self, name: &str) -> anyhow::Result<&ExternalAgent> {
        self.agents.get(name).ok_or_else(|| {
            let connected: Vec<&str> = self.agents.keys().map(String::as_str).collect();
            if connected.is_empty() {
                anyhow::anyhow!("no external agent is connected, so '{name}' cannot run")
            } else {
                anyhow::anyhow!(
                    "no external agent '{name}' is connected; connected: {}",
                    connected.join(", ")
                )
            }
        })
    }

    /// The connected agents with their descriptions, by name.
    pub fn described(&self) -> impl Iterator<Item = (&str, &str)> {
        self.agents
            .iter()
            .map(|(name, agent)| (name.as_str(), agent.description.as_str()))
    }
}
