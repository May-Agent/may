//! Typed parsing for remote profile preferences.

use crate::profile::RemoteProfilePreferences as RemoteProfileConfig;
use crate::{config::SshServerConfig, db::entity::profiles};

pub fn parse_remote_profile_config(
    profile: &profiles::Model,
) -> Result<Option<RemoteProfileConfig>, String> {
    let Some(ssh_server_id) = profile.remote_ssh_server_id.as_deref() else {
        return Ok(None);
    };
    let config = RemoteProfileConfig {
        ssh_server_id: ssh_server_id.to_string(),
    };
    config.normalized_ssh_server_id()?;
    Ok(Some(config))
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
