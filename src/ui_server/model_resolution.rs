//! Model resolution and persistence helpers.

use crate::config::MayConfig;

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
    let snapshot = state.config_snapshot();
    let strict = snapshot.preferences.strict_model_matching;
    let normalized_existing = snapshot.last_model.as_deref().map(|value| {
        if strict {
            value.to_string()
        } else {
            crate::config::normalize_model_id_for_relaxed_match(value)
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

    let updated = state.update_config(|config| {
        config.last_model = Some(model.to_string());
    });

    updated
        .save()
        .await
        .map_err(anyhow::Error::from)
        .inspect(|_| state.notify_ui_events())
}
