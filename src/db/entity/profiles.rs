//! Profile row model for the chat history database.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub id: String,
    /// Well-known identifier: "daily", "work", "local", or None for custom profiles.
    pub slug: Option<String>,
    pub name: String,
    /// Emoji or icon identifier for the profile.
    pub icon: Option<String>,
    /// Environment type: "local" or "container".
    pub environment: String,
    /// Docker image tag for container environments.
    pub container_image: Option<String>,
    /// Docker container ID when created.
    pub container_id: Option<String>,
    /// Persona overlay text injected into the agent system prompt.
    pub persona: Option<String>,
    /// Legacy SSH server binding migrated into `profile_remote_servers`.
    pub remote_ssh_server_id: Option<String>,
    /// Structured Dockerfile override for container profiles.
    pub container_dockerfile: Option<String>,
    /// Whether this profile has location awareness enabled.
    pub location_aware: bool,
    /// Maximum number of pinned items in this profile.
    pub pinned_limit: i32,
    /// Display order among profiles.
    pub sort_order: i32,
    pub created_at: String,
    pub updated_at: String,
}
