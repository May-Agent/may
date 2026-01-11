//! MCP (Model Context Protocol) configuration and connection management.
//!
//! Manages MCP server configurations and connections for the May agent.
//! Configuration is stored in `~/.may/mcp.json` using the same format
//! as Claude Desktop for compatibility.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aither_mcp::{McpConnection, McpError, McpServerConfig};
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
    pub fn load() -> Result<Self, McpConfigError> {
        let path = Self::config_path()?;
        Self::load_from(&path)
    }

    /// Load MCP configuration from a specific path.
    pub fn load_from(path: &Path) -> Result<Self, McpConfigError> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            Ok(serde_json::from_str(&content)?)
        } else {
            Ok(Self::default())
        }
    }

    /// Save MCP configuration to `~/.may/mcp.json`.
    pub fn save(&self) -> Result<(), McpConfigError> {
        let path = Self::config_path()?;
        self.save_to(&path)
    }

    /// Save MCP configuration to a specific path.
    pub fn save_to(&self, path: &Path) -> Result<(), McpConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Check if any MCP servers are configured.
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
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected(_))
    }

    /// Get the connection if connected.
    pub fn connection(&self) -> Option<&McpConnection> {
        if let Self::Connected(conn) = self {
            Some(conn)
        } else {
            None
        }
    }

    /// Get a mutable reference to the connection if connected.
    pub fn connection_mut(&mut self) -> Option<&mut McpConnection> {
        if let Self::Connected(conn) = self {
            Some(conn)
        } else {
            None
        }
    }
}

/// Server information for display in UI.
#[derive(Debug, Clone)]
pub struct McpServerInfo {
    /// Server name (configuration key).
    pub name: String,
    /// Command or URL used to connect.
    pub source: String,
    /// Connection status description.
    pub status: String,
    /// Whether the server is connected.
    pub is_connected: bool,
    /// Number of tools provided by this server.
    pub tool_count: usize,
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
    pub fn load() -> Result<Self, McpConfigError> {
        let config = McpConfig::load()?;
        Ok(Self::new(config))
    }

    /// Get the configuration.
    pub fn config(&self) -> &McpConfig {
        &self.config
    }

    /// Get a mutable reference to the configuration.
    pub fn config_mut(&mut self) -> &mut McpConfig {
        &mut self.config
    }

    /// Save the current configuration.
    pub fn save_config(&self) -> Result<(), McpConfigError> {
        self.config.save()
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
        self.config.mcp_servers.insert(name.clone(), config.clone());
        self.connect_server_internal(&name, &config).await?;
        self.config.save().map_err(|e| {
            McpError::InvalidConfig(format!("Failed to save config: {e}"))
        })?;
        Ok(())
    }

    /// Remove a server and disconnect if connected.
    pub async fn remove_server(&mut self, name: &str) -> Result<(), McpError> {
        // Close connection if exists
        if let Some(mut state) = self.connections.remove(name) {
            if let Some(conn) = state.connection_mut() {
                let _ = conn.close().await;
            }
        }

        // Remove from config
        self.config.mcp_servers.remove(name);
        self.config.save().map_err(|e| {
            McpError::InvalidConfig(format!("Failed to save config: {e}"))
        })?;

        Ok(())
    }

    /// Get connection state for a server.
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
            if let Some(McpConnectionState::Connected(conn)) =
                self.connections.insert(name, McpConnectionState::Disconnected)
            {
                connections.push(conn);
            }
        }
        connections
    }

    /// Get server information for UI display.
    pub fn server_info(&self) -> Vec<McpServerInfo> {
        self.config
            .mcp_servers
            .iter()
            .map(|(name, config)| {
                let source = if let Some(ref url) = config.url {
                    url.clone()
                } else if let Some(ref cmd) = config.command {
                    format!("{} {}", cmd, config.args.join(" "))
                } else {
                    "Unknown".to_string()
                };

                let (status, is_connected, tool_count) =
                    match self.connections.get(name) {
                        Some(McpConnectionState::Connected(conn)) => {
                            let count = conn.definitions().len();
                            (format!("Connected ({count} tools)"), true, count)
                        }
                        Some(McpConnectionState::Connecting) => {
                            ("Connecting...".to_string(), false, 0)
                        }
                        Some(McpConnectionState::Error(e)) => {
                            (format!("Error: {e}"), false, 0)
                        }
                        Some(McpConnectionState::Disconnected) | None => {
                            ("Disconnected".to_string(), false, 0)
                        }
                    };

                McpServerInfo {
                    name: name.clone(),
                    source,
                    status,
                    is_connected,
                    tool_count,
                }
            })
            .collect()
    }

    /// Check if any servers are configured.
    pub fn has_servers(&self) -> bool {
        !self.config.mcp_servers.is_empty()
    }

    /// Get the number of connected servers.
    pub fn connected_count(&self) -> usize {
        self.connections
            .values()
            .filter(|s| s.is_connected())
            .count()
    }
}
