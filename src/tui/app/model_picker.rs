//! Model picker state machine: tier model picker (config flow) and model
//! select popup (chat-mode quick switch).

use async_channel::Receiver;
use crossterm::event::KeyCode;
use executor_core::spawn;

use super::super::error::AppResult;
use super::super::state::AppMode;
use super::{App, SendAction};
use crate::config::{ModelTier, get_model_tier};
use crate::models::FetchedModel;

impl App {
    /// Handle key in tier model picker.
    pub(super) fn handle_tier_model_picker_key(
        &mut self,
        code: KeyCode,
    ) -> AppResult<Option<SendAction>> {
        match code {
            KeyCode::Esc => {
                // Cancel and go back to config
                self.mode = AppMode::Config;
                self.tier_picker_query.clear();
            }
            KeyCode::Enter => {
                // Select model and add to tier
                if let Some(&idx) = self.tier_picker_filtered.get(self.tier_picker_selected)
                    && let Some(model) = self.tier_picker_models.get(idx)
                    && let Some(tier) = self.tier_picker_tier
                {
                    self.config
                        .models
                        .add_model(tier, &model.id, &model.provider_id);
                    self.persist_config_background();
                }
                self.mode = AppMode::Config;
                self.tier_picker_query.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.tier_picker_selected = self.tier_picker_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.tier_picker_filtered.is_empty() {
                    self.tier_picker_selected =
                        (self.tier_picker_selected + 1).min(self.tier_picker_filtered.len() - 1);
                }
            }
            KeyCode::Backspace => {
                self.tier_picker_query.pop();
                self.update_tier_picker_filter();
            }
            KeyCode::Char(c) => {
                self.tier_picker_query.push(c);
                self.update_tier_picker_filter();
            }
            _ => {}
        }
        Ok(None)
    }

    /// Update tier picker filtered list based on query.
    pub(super) fn update_tier_picker_filter(&mut self) {
        let query_lower = self.tier_picker_query.to_lowercase();
        self.tier_picker_filtered = self
            .tier_picker_models
            .iter()
            .enumerate()
            .filter(|(_, model)| {
                query_lower.is_empty() || model.id.to_lowercase().contains(&query_lower)
            })
            .map(|(i, _)| i)
            .collect();
        // Reset selection if out of bounds
        if self.tier_picker_selected >= self.tier_picker_filtered.len() {
            self.tier_picker_selected = 0;
        }
    }

    /// Open tier model picker for a specific tier.
    /// Returns a receiver to poll for fetched models if async fetch was started.
    pub fn open_tier_model_picker(
        &mut self,
        tier: ModelTier,
    ) -> Option<Receiver<Vec<FetchedModel>>> {
        self.tier_picker_tier = Some(tier);
        self.tier_picker_query.clear();
        self.tier_picker_selected = 0;
        self.tier_picker_models.clear();
        self.tier_picker_filtered.clear();
        self.tier_picker_loading = true;
        self.mode = AppMode::TierModelPicker;

        // Create channel and spawn fetch task
        let (tx, rx) = async_channel::bounded(1);
        let config = self.config.clone();
        spawn(async move {
            let models = crate::models::fetch_all_models(&config).await;
            let _ = tx.send(models).await;
        })
        .detach();

        Some(rx)
    }

    /// Handle fetched models from async task.
    pub fn handle_fetched_models(&mut self, mut models: Vec<FetchedModel>) {
        // Sort by tier relevance:
        // 1. Models matching the current tier (from aither_models database)
        // 2. Models in the database but not matching this tier
        // 3. Unknown models (not in database)
        let current_tier = self.tier_picker_tier;

        models.sort_by(|a, b| {
            let a_info = aither_models::lookup(&a.id);
            let b_info = aither_models::lookup(&b.id);

            // Check tier match
            let a_tier_match = current_tier
                .and_then(|tier| a_info.map(|info| get_model_tier(info.id()) == tier))
                .unwrap_or(false);
            let b_tier_match = current_tier
                .and_then(|tier| b_info.map(|info| get_model_tier(info.id()) == tier))
                .unwrap_or(false);

            // Priority: tier match > in database > unknown
            match (a_tier_match, b_tier_match) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => match (a_info.is_some(), b_info.is_some()) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => a.id.cmp(&b.id),
                },
            }
        });

        // Dedupe by model ID (keep first occurrence which has correct provider)
        let mut seen = std::collections::HashSet::new();
        models.retain(|m| seen.insert(m.id.clone()));

        self.tier_picker_models = models;
        self.update_tier_picker_filter();
        self.tier_picker_loading = false;
    }

    /// Handle key in model select popup.
    pub(super) fn handle_model_select_key(
        &mut self,
        code: KeyCode,
    ) -> AppResult<Option<SendAction>> {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.model_select_index = self.model_select_index.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.model_select_index + 1 < self.model_select_list.len() {
                    self.model_select_index += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(model) = self.model_select_list.get(self.model_select_index).cloned() {
                    self.mode = AppMode::Chat;
                    return Ok(Some(SendAction::SwitchModel(model)));
                }
            }
            KeyCode::Esc => {
                self.mode = AppMode::Chat;
            }
            _ => {}
        }
        Ok(None)
    }

    /// Collect all unique models from config for model select.
    pub(super) fn collect_all_models(&self) -> Vec<String> {
        let mut models = Vec::new();
        for m in &self.config.models.advanced {
            let selector = m.to_string();
            if !models.contains(&selector) {
                models.push(selector);
            }
        }
        for m in &self.config.models.balanced {
            let selector = m.to_string();
            if !models.contains(&selector) {
                models.push(selector);
            }
        }
        for m in &self.config.models.fast {
            let selector = m.to_string();
            if !models.contains(&selector) {
                models.push(selector);
            }
        }
        models
    }
}
