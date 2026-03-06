//! Model resolution and persistence helpers.

use crate::config::MayConfig;

use super::helpers::save_config_to_disk;
use super::state::UiServerState;

pub(super) fn preferred_advanced_model(config: &MayConfig) -> Option<String> {
    config.models.advanced.first().cloned()
}

/// Resolve the effective model: prefer `preferred`, then `last_model`, then first advanced model.
pub(super) fn resolve_default_model(
    config: &MayConfig,
    preferred: Option<String>,
) -> Option<String> {
    preferred
        .or_else(|| config.last_model.clone())
        .or_else(|| preferred_advanced_model(config))
}

pub(super) async fn persist_last_model(state: &UiServerState, model: &str) -> anyhow::Result<()> {
    let updated = {
        let mut config = state.config.write().await;
        let strict = config.preferences.strict_model_matching;

        let normalized_existing = config.last_model.as_deref().map(|m| {
            if strict {
                m.to_string()
            } else {
                crate::config::normalize_model_id_for_relaxed_match(m)
            }
        });
        let normalized_new = if strict {
            model.to_string()
        } else {
            crate::config::normalize_model_id_for_relaxed_match(model)
        };

        if normalized_existing
            .as_deref()
            .is_some_and(|existing| existing == normalized_new.as_str())
        {
            return Ok(());
        }

        config.last_model = Some(model.to_string());
        config.clone()
    };

    save_config_to_disk(updated)
        .await
        .inspect(|_| state.notify_ui_events())
}
