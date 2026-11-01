//! Model picker state machine: tier model picker (config flow) and model
//! select popup (chat-mode quick switch).

use async_channel::Receiver;
use crossterm::event::KeyCode;
use executor_core::spawn;

use super::super::state::AppMode;
use super::{App, SendAction};
use crate::config::{ModelTier, get_model_tier};
use crate::models::FetchedModel;

impl App {
    /// Handle key in tier model picker.
    pub(super) fn handle_tier_model_picker_key(&mut self, code: KeyCode) -> Option<SendAction> {
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
        None
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

    /// Kick off a live fetch of every configured provider's model catalog.
    ///
    /// Returns a receiver to poll for the result; `None` is never produced —
    /// the fetch task always sends, an empty list on total failure.
    pub fn fetch_provider_models(&self) -> Option<Receiver<Vec<FetchedModel>>> {
        let (tx, rx) = async_channel::bounded(1);
        let config = self.config.clone();
        spawn(async move {
            let models = crate::models::fetch_all_models(&config).await;
            let _ = tx.send(models).await;
        })
        .detach();
        Some(rx)
    }

    /// Merge live-fetched provider catalogs into the model-select list:
    /// configured tier selectors keep their lead positions, and every model
    /// a provider actually serves is appended as `provider::model`. The
    /// catalog is remembered so the next `/model` open seeds a full list.
    pub fn merge_model_select_models(&mut self, models: Vec<FetchedModel>) {
        // Families whose catalog offers 2+ effort-suffixed variants collapse
        // to the family base: the rung is chosen on the effort ladder, not
        // by picking a suffixed model entry.
        let mut families: std::collections::HashMap<(String, String), usize> =
            std::collections::HashMap::new();
        for model in &models {
            let base = crate::models::effort_family_base(&model.id);
            *families
                .entry((model.provider_id.clone(), base.clone()))
                .or_default() += 1;
            if let Some(rung) = crate::models::effort_uid_token(&model.id) {
                let rungs = self
                    .family_effort_rungs
                    .entry((model.provider_id.clone(), base))
                    .or_default();
                if !rungs.contains(&rung) {
                    rungs.push(rung);
                }
            }
        }
        for rungs in self.family_effort_rungs.values_mut() {
            rungs.sort_by_key(|rung| crate::models::effort_rung_rank(rung));
        }
        let family_base_of = |provider_id: &str, model_id: &str| {
            let base = crate::models::effort_family_base(model_id);
            families
                .get(&(provider_id.to_string(), base.clone()))
                .is_some_and(|count| *count >= 2)
                .then_some(base)
        };
        // Configured selectors pointing at a suffixed variant of a collapsed
        // family resolve to the family base too.
        for entry in &mut self.model_select_list {
            let Ok(selector) = entry.parse::<crate::ModelSelector>() else {
                continue;
            };
            if let Some(base) = family_base_of(selector.provider_id(), selector.model_id()) {
                *entry = crate::ModelSelector::new(selector.provider_id(), &base).to_string();
            }
        }
        let mut seen = std::collections::HashSet::new();
        self.model_select_list
            .retain(|entry| seen.insert(entry.clone()));
        for model in &models {
            let id =
                family_base_of(&model.provider_id, &model.id).unwrap_or_else(|| model.id.clone());
            let selector = crate::ModelSelector::new(&model.provider_id, &id).to_string();
            if !self.model_select_list.contains(&selector) {
                self.model_select_list.push(selector);
            }
        }
        // A failed fetch returns an empty list: keep the previous catalog
        // then, or the next open falls back to the configured-only seed.
        if !models.is_empty() {
            self.model_catalog = models;
        }
        if self.model_select_index >= self.filtered_model_select().len() {
            self.model_select_index = 0;
        }
    }

    /// The select list narrowed by `model_select_query` as
    /// `(list_index, selector)` pairs, mirroring `filtered_sessions`.
    pub fn filtered_model_select(&self) -> Vec<(usize, &String)> {
        filter_model_list(&self.model_select_list, &self.model_select_query)
    }

    /// Position of `current` in the select list, matching the renderer's
    /// `(current)` marker: an effort-suffixed selector highlights its
    /// collapsed family entry on the same provider.
    pub(super) fn model_select_position(&self, current: &str) -> Option<usize> {
        self.model_select_list
            .iter()
            .position(|entry| crate::models::same_selectable_model(entry, current))
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

    /// Open the model select popup, optionally pre-narrowed by `query`.
    ///
    /// `/model` (an empty `query`) opens the picker with the highlight on
    /// the current model. `/model <query>` first resolves the query
    /// against the selector list — a hit switches at once like pressing
    /// Enter in the picker — while an ambiguous or unmatched query opens
    /// the picker with `query` already in the search line, narrowing
    /// further as the catalog fetch lands.
    pub fn open_model_select(&mut self, query: &str) -> Option<SendAction> {
        self.model_select_list = self.collect_all_models();
        // Seed with the catalog remembered from the last fetch so a
        // warm open paints the full list at once; the in-flight
        // refresh below re-merges whatever changed meanwhile.
        let catalog = std::mem::take(&mut self.model_catalog);
        self.merge_model_select_models(catalog);
        self.model_select_rx = self.fetch_provider_models();

        if let Some(selector) = resolve_model_query(&self.model_select_list, query) {
            self.model_select_query.clear();
            return Some(SendAction::SwitchModel(selector));
        }

        self.model_select_query = query.to_string();
        self.model_select_index = 0;
        if query.is_empty() {
            let fallback_model = self.config.last_model.as_ref().map(ToString::to_string);
            let current = self.current_model.as_deref().or(fallback_model.as_deref());
            if let Some(current) = current
                && let Some(idx) = self.model_select_position(current)
            {
                self.model_select_index = idx;
            }
        }
        self.mode = AppMode::ModelSelect;
        None
    }

    /// Handle key in model select popup. Typing narrows the list; `j`/`k`
    /// navigate only while the query is empty, like the session picker.
    pub(super) fn handle_model_select_key(&mut self, code: KeyCode) -> Option<SendAction> {
        let filtered_len = self.filtered_model_select().len();
        match code {
            KeyCode::Esc => {
                if self.model_select_query.is_empty() {
                    self.mode = AppMode::Chat;
                } else {
                    self.model_select_query.clear();
                    self.model_select_index = 0;
                }
            }
            KeyCode::Up => {
                self.model_select_index = self.model_select_index.saturating_sub(1);
            }
            KeyCode::Down => {
                if self.model_select_index + 1 < filtered_len {
                    self.model_select_index += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(&(idx, _)) = self.filtered_model_select().get(self.model_select_index)
                    && let Some(model) = self.model_select_list.get(idx).cloned()
                {
                    self.mode = AppMode::Chat;
                    return Some(SendAction::SwitchModel(model));
                }
            }
            KeyCode::Backspace => {
                self.model_select_query.pop();
                self.model_select_index = 0;
            }
            KeyCode::Char(c) => {
                if self.model_select_query.is_empty() {
                    match c {
                        'k' => {
                            self.model_select_index = self.model_select_index.saturating_sub(1);
                            return None;
                        }
                        'j' => {
                            if self.model_select_index + 1 < filtered_len {
                                self.model_select_index += 1;
                            }
                            return None;
                        }
                        _ => {}
                    }
                }
                self.model_select_query.push(c);
                self.model_select_index = 0;
            }
            _ => {}
        }
        None
    }

    /// Handle key in the effort select popup: ←/→ steps the ladder and
    /// applies at once, Enter/Esc closes.
    pub(super) fn handle_effort_select_key(&mut self, code: KeyCode) -> Option<SendAction> {
        match code {
            KeyCode::Left | KeyCode::Char('h') => {
                self.effort_index = self.effort_index.saturating_sub(1);
                return self.selected_effort();
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if self.effort_index + 1 < self.effort_ladder.len() {
                    self.effort_index += 1;
                }
                return self.selected_effort();
            }
            KeyCode::Enter | KeyCode::Esc => {
                self.mode = AppMode::Chat;
            }
            _ => {}
        }
        None
    }

    /// The effort token at the highlighted rung.
    fn selected_effort(&self) -> Option<SendAction> {
        self.effort_ladder
            .get(self.effort_index)
            .map(|token| SendAction::SetEffort(Some((*token).to_string())))
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

/// `list` narrowed by `query` — a case-insensitive substring on the
/// `provider::model` selector, so a query can hit either side — returned as
/// `(list_index, selector)` pairs. An empty query keeps every entry.
fn filter_model_list<'a>(list: &'a [String], query: &str) -> Vec<(usize, &'a String)> {
    if query.is_empty() {
        return list.iter().enumerate().collect();
    }
    let query = query.to_lowercase();
    list.iter()
        .enumerate()
        .filter(|(_, model)| model.to_lowercase().contains(&query))
        .collect()
}

/// The selector a `/model <query>` resolves to without the picker: an
/// exact list entry first, then the entry that is the same selectable
/// model as `query` (its effort-family twin), else the lone substring
/// hit. `None` means the query pins down no single entry — empty,
/// ambiguous or unmatched — and belongs in the picker's search line.
fn resolve_model_query(list: &[String], query: &str) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    if let Some(entry) = list.iter().find(|entry| *entry == query).or_else(|| {
        list.iter()
            .find(|entry| crate::models::same_selectable_model(entry, query))
    }) {
        return Some(entry.clone());
    }
    let filtered = filter_model_list(list, query);
    (filtered.len() == 1).then(|| filtered[0].1.clone())
}

/// The reasoning-effort ladder a provider's wire protocol expresses. Every
/// rung is concrete — the session's effort is the one it last selected, and
/// "default" before any selection means medium, not a provider-decided
/// level. Families outside a ladder still take it: vendors map what they can
/// on their own API-compat layer and reject what a model does not have.
pub(super) const fn effort_ladder_for(
    provider_type: Option<crate::config::ProviderType>,
) -> &'static [&'static str] {
    use crate::config::ProviderType;
    const OPENAI: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    const CLAUDE: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];
    const GEMINI: &[&str] = &["minimal", "low", "medium", "high"];
    match provider_type {
        Some(ProviderType::Anthropic) => CLAUDE,
        Some(ProviderType::Google) => GEMINI,
        _ => OPENAI,
    }
}

#[cfg(test)]
mod tests {
    use super::{filter_model_list, resolve_model_query};

    #[test]
    fn filter_keeps_every_entry_on_an_empty_query() {
        let list = vec!["relay::o4-mini".to_string(), "google::aqa".to_string()];
        assert_eq!(filter_model_list(&list, "").len(), 2);
    }

    #[test]
    fn filter_matches_provider_or_model_case_insensitively() {
        let list = vec![
            "relay::o4-mini".to_string(),
            "google::aqa".to_string(),
            "relay::claude-opus-4-6".to_string(),
        ];
        // Pairs carry the original list index so Enter resolves the real entry.
        let by_provider = filter_model_list(&list, "GOOGLE");
        assert_eq!(by_provider.len(), 1);
        assert_eq!(by_provider[0].0, 1);
        assert_eq!(by_provider[0].1, "google::aqa");
        let by_model = filter_model_list(&list, "opus");
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0].0, 2);
    }

    #[test]
    fn query_resolves_an_exact_selector_before_its_family_twin() {
        let list = vec![
            "relay::o4-mini-high".to_string(),
            "relay::o4-mini".to_string(),
            "google::aqa".to_string(),
        ];
        assert_eq!(
            resolve_model_query(&list, "relay::o4-mini").as_deref(),
            Some("relay::o4-mini")
        );
    }

    #[test]
    fn query_resolves_an_effort_suffixed_uid_to_its_family_entry() {
        let list = vec!["relay::o4-mini".to_string(), "google::aqa".to_string()];
        assert_eq!(
            resolve_model_query(&list, "relay::o4-mini-max").as_deref(),
            Some("relay::o4-mini")
        );
    }

    #[test]
    fn query_resolves_a_lone_substring_hit() {
        let list = vec![
            "relay::o4-mini".to_string(),
            "google::aqa".to_string(),
            "anthropic::claude-sonnet-4-6".to_string(),
        ];
        assert_eq!(
            resolve_model_query(&list, "aqa").as_deref(),
            Some("google::aqa")
        );
    }

    #[test]
    fn ambiguous_unmatched_and_empty_queries_stay_in_the_picker() {
        let list = vec![
            "relay::o4-mini".to_string(),
            "google::aqa".to_string(),
            "anthropic::claude-sonnet-4-6".to_string(),
        ];
        assert_eq!(resolve_model_query(&list, "a"), None);
        assert_eq!(resolve_model_query(&list, "gpt-5"), None);
        assert_eq!(resolve_model_query(&list, ""), None);
    }
}
