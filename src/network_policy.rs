use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::text::join_text;

/// Global network restriction mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicyMode {
    #[default]
    Disabled,
    Restricted,
    AllowList,
    DenyList,
}

impl NetworkPolicyMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Restricted => "restricted",
            Self::AllowList => "allow_list",
            Self::DenyList => "deny_list",
        }
    }
}

/// Persisted network policy settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyConfig {
    #[serde(default)]
    pub mode: NetworkPolicyMode,
    #[serde(default)]
    pub hosts: Vec<String>,
}

impl NetworkPolicyConfig {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        !matches!(self.mode, NetworkPolicyMode::Disabled)
    }

    pub fn compile(&self) -> Result<Option<CompiledNetworkPolicy>, String> {
        if !self.is_active() {
            return Ok(None);
        }

        let hosts = self
            .hosts
            .iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();

        let matcher = if matches!(
            self.mode,
            NetworkPolicyMode::AllowList | NetworkPolicyMode::DenyList
        ) {
            Some(build_matcher(&hosts)?)
        } else {
            None
        };

        Ok(Some(CompiledNetworkPolicy {
            mode: self.mode,
            hosts,
            matcher,
        }))
    }
}

#[derive(Debug, Clone)]
pub struct CompiledNetworkPolicy {
    mode: NetworkPolicyMode,
    hosts: Vec<String>,
    matcher: Option<GlobSet>,
}

impl CompiledNetworkPolicy {
    #[must_use]
    pub const fn mode(&self) -> NetworkPolicyMode {
        self.mode
    }

    #[must_use]
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    #[must_use]
    pub fn allows_domain(&self, domain: &str) -> bool {
        let normalized = domain.trim().to_ascii_lowercase();
        match self.mode {
            NetworkPolicyMode::Disabled => true,
            NetworkPolicyMode::Restricted => false,
            NetworkPolicyMode::AllowList => self
                .matcher
                .as_ref()
                .is_some_and(|matcher| matcher.is_match(normalized.as_str())),
            NetworkPolicyMode::DenyList => !self
                .matcher
                .as_ref()
                .is_some_and(|matcher| matcher.is_match(normalized.as_str())),
        }
    }
}

fn build_matcher(hosts: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for host in hosts {
        let glob = Glob::new(host).map_err(|error| {
            let error_text = error.to_string();
            join_text(&[
                "invalid network host pattern '",
                host.as_str(),
                "': ",
                error_text.as_str(),
            ])
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_policy_blocks_everything() {
        let policy = NetworkPolicyConfig {
            mode: NetworkPolicyMode::Restricted,
            hosts: Vec::new(),
        }
        .compile()
        .expect("restricted policy should compile")
        .expect("restricted policy should be active");
        assert!(!policy.allows_domain("example.com"));
    }

    #[test]
    fn allow_list_policy_matches_globs() {
        let policy = NetworkPolicyConfig {
            mode: NetworkPolicyMode::AllowList,
            hosts: vec!["*.example.com".to_string()],
        }
        .compile()
        .expect("allow list should compile")
        .expect("allow list should be active");
        assert!(policy.allows_domain("api.example.com"));
        assert!(!policy.allows_domain("example.org"));
    }
}
