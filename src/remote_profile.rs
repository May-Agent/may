//! Typed parsing for remote profile preferences.

use crate::{config::SshServerConfig, db::entity::profiles};

#[derive(Debug, Clone, serde::Deserialize)]
struct ProfilePreferences {
    #[serde(default)]
    remote: Option<RemoteProfileConfig>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteProfileConfig {
    pub ssh_server_id: String,
}

impl RemoteProfileConfig {
    pub fn normalized_ssh_server_id(&self) -> Result<&str, String> {
        let ssh_server_id = self.ssh_server_id.trim();
        if ssh_server_id.is_empty() {
            return Err("remote profile requires preferences.remote.ssh_server_id".to_string());
        }
        Ok(ssh_server_id)
    }
}

pub fn parse_remote_profile_config(
    profile: &profiles::Model,
) -> Result<Option<RemoteProfileConfig>, String> {
    let Some(raw) = profile.preferences_json.as_deref() else {
        return Ok(None);
    };
    let preferences: ProfilePreferences =
        serde_json::from_str(raw).map_err(|error| {
            format!(
                "invalid profile.preferences_json for remote profile {}: {error}",
                profile.id
            )
    })?;
    Ok(preferences.remote)
}

pub fn resolve_remote_profile_server(
    profile: &profiles::Model,
    configured_servers: &[SshServerConfig],
) -> Result<SshServerConfig, String> {
    let config = parse_remote_profile_config(profile)?.ok_or_else(|| {
        format!(
            "remote profile {} requires preferences.remote.ssh_server_id",
            profile.id
        )
    })?;
    let ssh_server_id = config.normalized_ssh_server_id()?;
    configured_servers
        .iter()
        .find(|server| server.name == ssh_server_id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "remote profile {} references unknown ssh server {}",
                profile.id, ssh_server_id
            )
        })
}
