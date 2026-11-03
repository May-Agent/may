//! The wire shapes of `/api/acp-agents`, shared by the daemon and its clients.

use serde::{Deserialize, Serialize};

/// A connected ACP agent. Its environment stays on the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectedAcpAgent {
    /// The subagent type name.
    pub name: String,
    /// Executable it launches.
    pub command: String,
    /// Arguments to the executable.
    pub args: Vec<String>,
    /// The model every session runs.
    pub model: Option<String>,
    /// The mode every session runs.
    pub mode: Option<String>,
    /// What the main agent reads about it.
    pub description: String,
}

/// An installed agent that answered ACP `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DetectedAcpAgent {
    /// Executable on `PATH`.
    pub command: String,
    /// Arguments that make it speak ACP.
    pub args: Vec<String>,
    /// The name it reports.
    pub name: String,
    /// The version it reports.
    pub version: String,
    /// The subagent type it is already connected as.
    pub connected_as: Option<String>,
}

/// `GET /api/acp-agents`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AcpAgentsResponse {
    /// The connected agents.
    pub connected: Vec<ConnectedAcpAgent>,
    /// The installed agents that could be connected.
    pub detected: Vec<DetectedAcpAgent>,
}

/// `POST /api/acp-agents`.
#[derive(Debug, Clone, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectAcpAgentRequest {
    /// The subagent type name to connect it as.
    pub name: String,
    /// Executable to launch.
    pub command: String,
    /// Arguments to the executable.
    #[serde(default)]
    pub args: Vec<String>,
    /// The model every session runs.
    pub model: Option<String>,
    /// The mode every session runs.
    pub mode: Option<String>,
}

/// What the client must choose before the connection can succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AcpChoice {
    /// The agent advertises models and none was given.
    ModelRequired,
    /// The agent advertises modes and none was given.
    ModeRequired,
}

/// Body of the `422` that asks the client to choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
pub struct AcpChoiceBody {
    /// What is missing.
    pub code: AcpChoice,
    /// The agent's own message.
    pub error: String,
    /// The ids the agent advertises.
    pub valid: Vec<String>,
}
