//! Settings an agent is built with that a saved config change must reach.

use std::collections::BTreeMap;

use crate::cache_lifetime::CacheLifetime;
use crate::config::{ContextConfig, MayConfig};
use crate::model_selector::ModelSelector;

/// The config values a running agent was built from: each provider's cache
/// lifetime and the compaction model.
///
/// aither fixes both when the agent is built, so a worker whose settings
/// differ from the saved config is replaced by one built from it.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentLiveSettings {
    cache_lifetimes: BTreeMap<String, CacheLifetime>,
    compaction_model: Option<ModelSelector>,
    context: ContextConfig,
}

impl AgentLiveSettings {
    /// Reads the settings from `config`.
    #[must_use]
    pub fn of(config: &MayConfig) -> Self {
        Self {
            cache_lifetimes: config
                .provider
                .iter()
                .map(|(id, instance)| (id.clone(), instance.cache_lifetime))
                .collect(),
            compaction_model: config.models.compaction_model.clone(),
            context: config.context,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn config() -> MayConfig {
        let mut config = MayConfig::default();
        config.provider.insert(
            "default".to_string(),
            crate::config::ProviderInstance::default(),
        );
        config
    }

    #[test]
    fn a_changed_cache_lifetime_or_compaction_model_differs() {
        let mut config = config();
        let built = AgentLiveSettings::of(&config);
        assert_eq!(built, AgentLiveSettings::of(&config));

        config
            .set_provider_cache_lifetime("default", "30m")
            .expect("a duration");
        let lifetime_changed = AgentLiveSettings::of(&config);
        assert_ne!(built, lifetime_changed);
        assert_eq!(
            config.provider["default"].cache_lifetime,
            CacheLifetime::Limited(Duration::from_mins(30))
        );

        config.models.compaction_model = Some("default::small".parse().expect("selector"));
        assert_ne!(lifetime_changed, AgentLiveSettings::of(&config));
    }

    #[test]
    fn a_changed_warn_distance_differs_and_reaches_the_policy() {
        let mut config = config();
        assert_eq!(
            config.context.policy().warn_distance,
            aither_agent::ContextPolicy::default().warn_distance,
            "the default is aither's"
        );
        let built = AgentLiveSettings::of(&config);
        config.context.warn_distance = 12_345;
        assert_ne!(built, AgentLiveSettings::of(&config));
        assert_eq!(config.context.policy().warn_distance, 12_345);
    }
}
