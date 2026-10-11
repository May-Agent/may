//! MCP (Model Context Protocol) configuration and connection management.
//!
//! Manages MCP server configurations and connections for the May agent.
//! Configuration is stored in `~/.may/mcp.json` using the same format
//! as Claude Desktop for compatibility.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aither_mcp::{McpConnection, McpError, McpServerConfig};
use async_fs;
use serde::{Deserialize, Serialize};

/// MCP configuration error.
#[derive(Debug, thiserror::Error)]
pub enum McpConfigError {
    #[error("Home directory not found")]
    NoHomeDir,
    #[error("Failed to read MCP config: {0}")]
    Read(#[from] std::io::Error),
    #[error("Failed to parse MCP config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("MCP connection error: {0}")]
    Connection(#[from] McpError),
}

/// MCP connection loading error for terminal-command registration.
#[derive(Debug, thiserror::Error)]
pub enum McpConnectionLoadError {
    #[error(transparent)]
    Config(#[from] McpConfigError),
    #[error("failed to connect MCP server '{server}': {source}")]
    Connect {
        server: String,
        #[source]
        source: McpError,
    },
}

/// MCP server configuration (serializable version).
///
/// This mirrors `McpServerConfig` from aither-mcp but adds Serialize support.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MayMcpServerConfig {
    /// The command to run (for process-based servers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// Arguments to pass to the command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    /// URL for HTTP-based servers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Optional environment variables for the process.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

impl From<MayMcpServerConfig> for McpServerConfig {
    fn from(config: MayMcpServerConfig) -> Self {
        Self {
            command: config.command,
            args: config.args,
            url: config.url,
            env: config.env,
        }
    }
}

impl From<&MayMcpServerConfig> for McpServerConfig {
    fn from(config: &MayMcpServerConfig) -> Self {
        Self {
            command: config.command.clone(),
            args: config.args.clone(),
            url: config.url.clone(),
            env: config.env.clone(),
        }
    }
}

impl MayMcpServerConfig {
    fn validate(&self) -> Result<(), McpError> {
        let has_command = self
            .command
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        let has_url = self
            .url
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());

        match (has_command, has_url) {
            (true, false) => {}
            (false, true) => {
                let url = self
                    .url
                    .as_deref()
                    .map(str::trim)
                    .expect("url presence must match validation branch");
                url::Url::parse(url).map_err(|error| McpError::InvalidConfig(error.to_string()))?;
            }
            (false, false) => {
                return Err(McpError::InvalidConfig(
                    "MCP server config requires either a process command or a URL".to_string(),
                ));
            }
            (true, true) => {
                return Err(McpError::InvalidConfig(
                    "MCP server config must choose either process or URL, not both".to_string(),
                ));
            }
        }

        if self.args.iter().any(|arg| arg.trim().is_empty()) {
            return Err(McpError::InvalidConfig(
                "MCP server args must not contain empty entries".to_string(),
            ));
        }

        if self.env.keys().any(|key| key.trim().is_empty()) {
            return Err(McpError::InvalidConfig(
                "MCP server env keys must not be empty".to_string(),
            ));
        }

        Ok(())
    }
}

/// MCP server configurations map.
pub type MayMcpServersConfig = HashMap<String, MayMcpServerConfig>;

/// MCP configuration file format.
///
/// Compatible with Claude Desktop's `claude_desktop_config.json` format.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    /// MCP server configurations, keyed by server name.
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: MayMcpServersConfig,
}

impl McpConfig {
    /// Get the MCP config file path (~/.may/mcp.json).
    pub fn config_path() -> Result<PathBuf, McpConfigError> {
        dirs::home_dir()
            .map(|h| h.join(".may").join("mcp.json"))
            .ok_or(McpConfigError::NoHomeDir)
    }

    /// Load MCP configuration from `~/.may/mcp.json` or use defaults.
    pub async fn load() -> Result<Self, McpConfigError> {
        let path = Self::config_path()?;
        Self::load_from(&path).await
    }

    /// Load MCP configuration from a specific path.
    pub async fn load_from(path: &Path) -> Result<Self, McpConfigError> {
        match async_fs::metadata(path).await {
            Ok(_) => {
                let content = async_fs::read_to_string(path).await?;
                Ok(serde_json::from_str(&content)?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(McpConfigError::Read(error)),
        }
    }

    /// Save MCP configuration to `~/.may/mcp.json`.
    pub async fn save(&self) -> Result<(), McpConfigError> {
        let path = Self::config_path()?;
        self.save_to(&path).await
    }

    /// Save MCP configuration to a specific path.
    pub async fn save_to(&self, path: &Path) -> Result<(), McpConfigError> {
        if let Some(parent) = path.parent() {
            async_fs::create_dir_all(parent).await?;
        }
        let content = serde_json::to_string_pretty(self)?;
        async_fs::write(path, content).await?;
        Ok(())
    }

    /// Check if any MCP servers are configured.
    #[must_use]
    pub fn has_servers(&self) -> bool {
        !self.mcp_servers.is_empty()
    }

    /// Add or update an MCP server configuration.
    pub fn upsert_server(&mut self, name: impl Into<String>, config: MayMcpServerConfig) {
        self.mcp_servers.insert(name.into(), config);
    }

    /// Remove an MCP server configuration.
    pub fn remove_server(&mut self, name: &str) -> Option<MayMcpServerConfig> {
        self.mcp_servers.remove(name)
    }

    /// Get a server configuration by name.
    #[must_use]
    pub fn get_server(&self, name: &str) -> Option<&MayMcpServerConfig> {
        self.mcp_servers.get(name)
    }

    /// Iterate over all server configurations.
    pub fn servers(&self) -> impl Iterator<Item = (&str, &MayMcpServerConfig)> {
        self.mcp_servers.iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// Connection state for an MCP server.
#[derive(Debug)]
pub enum McpConnectionState {
    /// Server is configured but not connected.
    Disconnected,
    /// Connection is in progress.
    Connecting,
    /// Server is connected and ready.
    Connected(McpConnection),
    /// Connection failed with an error.
    Error(String),
}

impl McpConnectionState {
    /// Check if the server is connected.
    pub const fn is_connected(&self) -> bool {
        matches!(self, Self::Connected(_))
    }

    /// Get the connection if connected.
    pub const fn connection(&self) -> Option<&McpConnection> {
        if let Self::Connected(conn) = self {
            Some(conn)
        } else {
            None
        }
    }

    /// Get a mutable reference to the connection if connected.
    pub const fn connection_mut(&mut self) -> Option<&mut McpConnection> {
        if let Self::Connected(conn) = self {
            Some(conn)
        } else {
            None
        }
    }
}

/// Server information for display in UI.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpServerSource {
    Url { url: String },
    Process { command: String, args: Vec<String> },
    Unknown,
}

impl std::fmt::Display for McpServerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Url { url } => f.write_str(url),
            Self::Process { command, args } => {
                f.write_str(command)?;
                if !args.is_empty() {
                    f.write_str(" ")?;
                    f.write_str(args.join(" ").as_str())?;
                }
                Ok(())
            }
            Self::Unknown => f.write_str("Unknown"),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum McpServerStatus {
    Disconnected,
    Connecting,
    Connected { tool_count: usize },
    Error { message: String },
}

impl McpServerStatus {
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

impl std::fmt::Display for McpServerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => f.write_str("Disconnected"),
            Self::Connecting => f.write_str("Connecting..."),
            Self::Connected { tool_count } => {
                write!(f, "Connected ({tool_count} tools)")
            }
            Self::Error { message } => {
                f.write_str("Error: ")?;
                f.write_str(message)
            }
        }
    }
}

/// Server information for display in UI.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
pub struct McpServerInfo {
    /// Server name (configuration key).
    pub name: String,
    /// Structured source definition.
    pub source: McpServerSource,
    /// Structured connection status.
    pub status: McpServerStatus,
}

/// MCP connection manager.
///
/// Manages connections to MCP servers based on configuration.
#[derive(Debug)]
pub struct McpManager {
    /// Server configurations.
    config: McpConfig,
    /// Active connections, keyed by server name.
    connections: HashMap<String, McpConnectionState>,
}

impl McpManager {
    /// Create a new MCP manager with the given configuration.
    #[must_use]
    pub fn new(config: McpConfig) -> Self {
        // Initialize all servers as disconnected
        let connections = config
            .mcp_servers
            .keys()
            .map(|name| (name.clone(), McpConnectionState::Disconnected))
            .collect();

        Self {
            config,
            connections,
        }
    }

    /// Load MCP manager from default configuration path.
    pub async fn load() -> Result<Self, McpConfigError> {
        let config = McpConfig::load().await?;
        Ok(Self::new(config))
    }

    /// Get the configuration.
    #[must_use]
    pub const fn config(&self) -> &McpConfig {
        &self.config
    }

    /// Get a mutable reference to the configuration.
    pub const fn config_mut(&mut self) -> &mut McpConfig {
        &mut self.config
    }

    /// Save the current configuration.
    pub async fn save_config(&self) -> Result<(), McpConfigError> {
        self.config.save().await
    }

    /// Connect to all configured MCP servers.
    pub async fn connect_all(&mut self) -> Vec<(String, Result<(), McpError>)> {
        // Collect names first to avoid borrow conflict
        let names: Vec<String> = self.config.mcp_servers.keys().cloned().collect();
        let mut results = Vec::new();

        for name in names {
            if let Some(server_config) = self.config.mcp_servers.get(&name).cloned() {
                let result = self.connect_server_internal(&name, &server_config).await;
                results.push((name, result));
            }
        }

        results
    }

    /// Connect to a specific MCP server (internal implementation).
    async fn connect_server_internal(
        &mut self,
        name: &str,
        config: &MayMcpServerConfig,
    ) -> Result<(), McpError> {
        if let Err(error) = config.validate() {
            self.connections.insert(
                name.to_string(),
                McpConnectionState::Error(error.to_string()),
            );
            return Err(error);
        }

        // Mark as connecting
        self.connections
            .insert(name.to_string(), McpConnectionState::Connecting);

        // Convert to aither-mcp config
        let mcp_config: McpServerConfig = config.into();

        // Attempt connection
        match McpConnection::from_config(&mcp_config).await {
            Ok(conn) => {
                tracing::info!("Connected to MCP server: {name}");
                self.connections
                    .insert(name.to_string(), McpConnectionState::Connected(conn));
                Ok(())
            }
            Err(e) => {
                tracing::warn!("Failed to connect to MCP server {name}: {e}");
                self.connections
                    .insert(name.to_string(), McpConnectionState::Error(e.to_string()));
                Err(e)
            }
        }
    }

    /// Reconnect to a specific server by name.
    pub async fn reconnect(&mut self, name: &str) -> Result<(), McpError> {
        if let Some(config) = self.config.mcp_servers.get(name).cloned() {
            self.connect_server_internal(name, &config).await
        } else {
            Err(McpError::InvalidConfig(format!(
                "Server '{name}' not found in configuration"
            )))
        }
    }

    /// Add a new server and connect to it.
    pub async fn add_server(
        &mut self,
        name: impl Into<String>,
        config: MayMcpServerConfig,
    ) -> Result<(), McpError> {
        let name = name.into();
        config.validate()?;

        let mut next_config = self.config.clone();
        next_config.upsert_server(name.clone(), config.clone());
        next_config
            .save()
            .await
            .map_err(|e| McpError::InvalidConfig(format!("Failed to save config: {e}")))?;
        self.config = next_config;
        self.connections
            .entry(name.clone())
            .or_insert(McpConnectionState::Disconnected);
        self.connect_server_internal(&name, &config).await?;
        Ok(())
    }

    /// Remove a server and disconnect if connected.
    pub async fn remove_server(&mut self, name: &str) -> Result<(), McpError> {
        let mut next_config = self.config.clone();
        if next_config.remove_server(name).is_none() {
            return Err(McpError::InvalidConfig(format!(
                "Server '{name}' not found in configuration"
            )));
        }
        next_config
            .save()
            .await
            .map_err(|e| McpError::InvalidConfig(format!("Failed to save config: {e}")))?;

        // Close connection if exists
        if let Some(mut state) = self.connections.remove(name)
            && let Some(conn) = state.connection_mut()
        {
            let _ = conn.close().await;
        }

        self.config = next_config;
        Ok(())
    }

    /// Get connection state for a server.
    #[must_use]
    pub fn connection_state(&self, name: &str) -> Option<&McpConnectionState> {
        self.connections.get(name)
    }

    /// Get all active MCP connections.
    pub fn active_connections(&self) -> impl Iterator<Item = (&str, &McpConnection)> {
        self.connections.iter().filter_map(|(name, state)| {
            if let McpConnectionState::Connected(conn) = state {
                Some((name.as_str(), conn))
            } else {
                None
            }
        })
    }

    /// Take all active connections (for passing to agent builder).
    ///
    /// This removes the connections from the manager, so they cannot be
    /// reused. The manager will mark them as disconnected.
    pub fn take_connections(&mut self) -> Vec<McpConnection> {
        let names: Vec<String> = self
            .connections
            .iter()
            .filter_map(|(name, state)| {
                if state.is_connected() {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect();

        let mut connections = Vec::new();
        for name in names {
            if let Some(McpConnectionState::Connected(conn)) = self
                .connections
                .insert(name, McpConnectionState::Disconnected)
            {
                connections.push(conn);
            }
        }
        connections
    }

    /// Get server information for UI display.
    #[must_use]
    pub fn server_info(&self) -> Vec<McpServerInfo> {
        self.config
            .mcp_servers
            .iter()
            .map(|(name, config)| {
                let source = if let Some(ref url) = config.url {
                    McpServerSource::Url { url: url.clone() }
                } else if let Some(ref cmd) = config.command {
                    McpServerSource::Process {
                        command: cmd.clone(),
                        args: config.args.clone(),
                    }
                } else {
                    McpServerSource::Unknown
                };

                let status = match self.connections.get(name) {
                    Some(McpConnectionState::Connected(conn)) => {
                        let count = conn.definitions().len();
                        McpServerStatus::Connected { tool_count: count }
                    }
                    Some(McpConnectionState::Connecting) => McpServerStatus::Connecting,
                    Some(McpConnectionState::Error(e)) => {
                        McpServerStatus::Error { message: e.clone() }
                    }
                    Some(McpConnectionState::Disconnected) | None => McpServerStatus::Disconnected,
                };

                McpServerInfo {
                    name: name.clone(),
                    source,
                    status,
                }
            })
            .collect()
    }

    /// Check if any servers are configured.
    #[must_use]
    pub fn has_servers(&self) -> bool {
        !self.config.mcp_servers.is_empty()
    }

    /// Get the number of connected servers.
    #[must_use]
    pub fn connected_count(&self) -> usize {
        self.connections
            .values()
            .filter(|s| s.is_connected())
            .count()
    }
}

/// Load all configured MCP servers from the given May home and return live connections.
///
/// This is the single runtime path for May's automatic MCP -> terminal-command conversion.
/// Every connected MCP tool is expected to be exposed through the schema-driven IPC command
/// registry rather than a bespoke native tool path.
pub async fn load_active_mcp_connections(
    may_dir: &Path,
) -> Result<Vec<McpConnection>, McpConnectionLoadError> {
    let config_path = may_dir.join("mcp.json");
    let config = McpConfig::load_from(&config_path).await?;
    if !config.has_servers() {
        return Ok(Vec::new());
    }

    let mut manager = McpManager::new(config);
    for (server, result) in manager.connect_all().await {
        if let Err(source) = result {
            return Err(McpConnectionLoadError::Connect { server, source });
        }
    }

    Ok(manager.take_connections())
}
