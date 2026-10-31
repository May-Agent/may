//! Model resolution and persistence helpers.

use crate::config::MayConfig;
use crate::model_selector::ModelSelector;
use crate::ui_event_bus::UiEventTopics;

use super::state::DaemonState;

#[derive(Debug, thiserror::Error)]
pub(super) enum PersistLastModelError {
    #[error("config save failed: {0}")]
    Save(#[from] crate::config::ConfigError),
    #[error("invalid model selector: {0}")]
    InvalidSelector(#[from] crate::ModelSelectorParseError),
}

pub(super) fn preferred_advanced_model(config: &MayConfig) -> Option<String> {
    config.models.advanced.first().map(ToString::to_string)
}

/// Resolve the effective model: prefer `preferred`, then `last_model`, then first advanced model.
pub(super) fn resolve_default_model(
    config: &MayConfig,
    preferred: Option<String>,
) -> Option<String> {
    preferred
        .or_else(|| config.last_model.as_ref().map(ToString::to_string))
        .or_else(|| preferred_advanced_model(config))
}

pub(super) async fn persist_last_model(
    state: &DaemonState,
    model: &str,
) -> Result<(), PersistLastModelError> {
    let snapshot = state.config_snapshot();
    let normalized_existing = snapshot.last_model.as_ref().map(ToString::to_string);
    let normalized_new = model.to_string();

    if normalized_existing
        .as_deref()
        .is_some_and(|existing| existing == normalized_new.as_str())
    {
        return Ok(());
    }

    let selector = model.parse::<ModelSelector>()?;
    let updated = state.update_config(|config| {
        config.last_model = Some(selector);
    });

    updated.save().await?;
    state.notify_ui_events(UiEventTopics::CONFIG);
    Ok(())
}
