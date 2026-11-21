//! Config-mode key handling for App.

use crossterm::event::{KeyCode, KeyModifiers};

use super::super::state::{
    AppMode, ConfigEditMode, ConfigSection, ConfigTextTarget, detect_api_key_from_env,
};
use super::{App, SendAction};
use crate::config::{ModelTier, NativeToolsConfig, ProviderInstance, ProviderType, get_model_tier};
use crate::daemon::types::{SecretUpdate, UiFullConfig, UiModelsConfig, UiProviderConfig};
use crate::model_selector::ModelSelector;
use crate::tui::grapheme;

/// Edits `input` at `cursor`, a byte offset on a grapheme boundary, for the
/// cursor keys and text keys of a one-line field; other keys do nothing.
fn apply_edit_key(code: KeyCode, input: &mut String, cursor: &mut usize) {
    match code {
        KeyCode::Backspace => *cursor = grapheme::remove_before(input, *cursor),
        KeyCode::Delete => grapheme::remove_after(input, *cursor),
        KeyCode::Left => *cursor = grapheme::prev_boundary(input, *cursor),
        KeyCode::Right => *cursor = grapheme::next_boundary(input, *cursor),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = input.len(),
        KeyCode::Char(c) => *cursor = grapheme::insert_char(input, *cursor, c),
        _ => {}
    }
}

impl App {
    /// Handle key in unified config mode.
    pub(super) fn handle_config_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<SendAction> {
        let state = &mut self.config_state;

        // If in edit mode, handle text input
        if state.is_editing() {
            return self.handle_config_edit_key(code);
        }

        // Tab/Shift+Tab to switch sections
        if code == KeyCode::Tab {
            if modifiers.contains(KeyModifiers::SHIFT) {
                state.section = state.section.prev();
            } else {
                state.section = state.section.next();
            }
            state.selected = 0;
            return None;
        }

        // Navigation within section
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                state.selected = state.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let row_count = state.row_count(&self.config);
                if state.selected + 1 < row_count {
                    state.selected += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                return self.handle_config_enter();
            }
            KeyCode::Char('c') => {
                // Edit the selected provider's cache lifetime (Providers section only)
                if state.section == ConfigSection::Providers
                    && let Some(id) = state.provider_ids.get(state.selected)
                    && let Some(instance) = self.config.provider.get(id)
                {
                    let current = instance.cache_lifetime.to_string();
                    state.start_text_edit(&current);
                    state.text_target = ConfigTextTarget::CacheLifetime;
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                // Delete provider (only in Providers section)
                if state.section == ConfigSection::Providers
                    && let Some(id) = state.provider_ids.get(state.selected).cloned()
                {
                    self.config.provider.remove(&id);
                    state.load_providers(&self.config);
                    if state.selected >= state.provider_ids.len() && state.selected > 0 {
                        state.selected -= 1;
                    }
                    self.persist_config_background();
                }
            }
            KeyCode::Esc => {
                if !self.needs_setup {
                    self.mode = AppMode::Chat;
                } else if self.config.is_configured() {
                    // Setup is done - at least one provider configured
                    self.needs_setup = false;
                    self.mode = AppMode::Chat;
                    return Some(SendAction::NewSession);
                }
            }
            _ => {}
        }

        None
    }

    /// Handle Enter in config screen - edit/toggle the selected item.
    fn handle_config_enter(&mut self) -> Option<SendAction> {
        let state = &mut self.config_state;

        match state.section {
            ConfigSection::Providers => {
                // Last row is "Add provider..."
                if state.selected >= state.provider_ids.len() {
                    // Open provider type selector
                    let items: Vec<String> = ProviderType::ALL
                        .iter()
                        .map(|p| p.display_name().to_string())
                        .collect();
                    state.edit_mode = ConfigEditMode::List { items, selected: 0 };
                } else if let Some(id) = state.provider_ids.get(state.selected).cloned() {
                    // OAuth providers restart their login flow on Enter.
                    if id == "copilot" {
                        self.auth_return = crate::tui::state::AuthReturn::Config;
                        return Some(SendAction::StartCopilotAuth);
                    }
                    if id == "chatgpt" {
                        self.auth_return = crate::tui::state::AuthReturn::Config;
                        return Some(SendAction::StartChatgptAuth);
                    }
                    if id == "grok" {
                        self.auth_return = crate::tui::state::AuthReturn::Config;
                        return Some(SendAction::StartGrokAuth);
                    }
                    // Check if this is a custom provider (has base_url)
                    let instance = self.config.provider.get(&id);
                    if let Some(inst) = instance
                        && inst.base_url.is_some()
                    {
                        // Custom provider - open full edit form
                        state.edit_mode = ConfigEditMode::CustomProvider {
                            focused_field: 0,
                            name: id.clone(),
                            base_url: inst.base_url.clone().unwrap_or_default(),
                            api_key: inst.api_key.clone().unwrap_or_default(),
                            cursor: id.len(),
                            editing_existing: true,
                            original_name: Some(id),
                        };
                        return None;
                    }
                    // Regular provider - just edit API key
                    let current_key = self
                        .config
                        .provider
                        .get(&id)
                        .and_then(|p| p.api_key.clone())
                        .unwrap_or_default();
                    state.start_text_edit(&current_key);
                }
            }
            ConfigSection::Models => match state.selected {
                0 => {
                    self.model_fetch_rx = self.open_tier_model_picker(ModelTier::Advanced);
                }
                1 => {
                    self.model_fetch_rx = self.open_tier_model_picker(ModelTier::Balanced);
                }
                2 => {
                    self.model_fetch_rx = self.open_tier_model_picker(ModelTier::Fast);
                }
                3 => {
                    self.model_fetch_rx = self.open_tier_model_picker(ModelTier::ImageGeneration);
                }
                4 => {
                    // Edit the compaction model selector
                    let current = self
                        .config
                        .models
                        .compaction_model
                        .as_ref()
                        .map_or_default(ToString::to_string);
                    state.start_text_edit(&current);
                }
                _ => {}
            },
            ConfigSection::Preferences => {
                match state.selected {
                    0 => {
                        // Edit max_iterations
                        state.start_text_edit(&self.config.max_iterations.to_string());
                    }
                    1 => {
                        // Toggle warn_on_model_change
                        self.config.preferences.warn_on_model_change =
                            !self.config.preferences.warn_on_model_change;
                        self.persist_config_background();
                    }
                    2 => {
                        // Toggle show_decode_speed
                        self.config.preferences.show_decode_speed =
                            !self.config.preferences.show_decode_speed;
                        self.persist_config_background();
                    }
                    3 => {
                        // Edit the jobserver pool size; empty is automatic
                        let current = self
                            .config
                            .sandbox
                            .job_pool_size
                            .map_or_default(|size| size.to_string());
                        state.start_text_edit(&current);
                    }
                    _ => {}
                }
            }
        }

        None
    }

    /// Handle key input when editing in config screen.
    fn handle_config_edit_key(&mut self, code: KeyCode) -> Option<SendAction> {
        let state = &mut self.config_state;

        // Handle list selection mode
        if let ConfigEditMode::List { items, selected } = &mut state.edit_mode {
            match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if *selected + 1 < items.len() {
                        *selected += 1;
                    }
                }
                KeyCode::Enter => {
                    // Add provider of selected type
                    let idx = *selected;
                    if let Some(provider_type) = ProviderType::ALL.get(idx) {
                        // For Copilot, start OAuth
                        if *provider_type == ProviderType::Copilot {
                            state.edit_mode = ConfigEditMode::None;
                            return Some(SendAction::StartCopilotAuth);
                        }
                        // For ChatGPT, start subscription OAuth
                        if *provider_type == ProviderType::Chatgpt {
                            state.edit_mode = ConfigEditMode::None;
                            self.auth_return = crate::tui::state::AuthReturn::Config;
                            return Some(SendAction::StartChatgptAuth);
                        }
                        // For Grok, start subscription OAuth
                        if *provider_type == ProviderType::Grok {
                            state.edit_mode = ConfigEditMode::None;
                            self.auth_return = crate::tui::state::AuthReturn::Config;
                            return Some(SendAction::StartGrokAuth);
                        }
                        // For Custom provider, need name + base_url + api_key
                        if *provider_type == ProviderType::Custom {
                            state.edit_mode = ConfigEditMode::CustomProvider {
                                focused_field: 0,
                                name: String::new(),
                                base_url: String::new(),
                                api_key: String::new(),
                                cursor: 0,
                                editing_existing: false,
                                original_name: None,
                            };
                            return None;
                        }
                        state.edit_mode = ConfigEditMode::None;
                        // For others, try auto-detect API key
                        let provider_id = provider_type.short_name().to_string();
                        if let Some((key, var_name)) = detect_api_key_from_env(*provider_type) {
                            // Auto-detected - create provider
                            let mut instance = crate::config::ProviderInstance::new(*provider_type);
                            instance.api_key = Some(key);
                            self.config.upsert_provider(&provider_id, instance);
                            state.load_providers(&self.config);
                            state.key_from_env = true;
                            state.key_env_var_name = Some(var_name.to_string());
                            self.auto_populate_models_if_empty();
                            self.persist_config_background();
                        } else {
                            // Need manual API key entry
                            let instance = crate::config::ProviderInstance::new(*provider_type);
                            self.config.upsert_provider(&provider_id, instance);
                            state.load_providers(&self.config);
                            // Find the new provider in the list and select it
                            if let Some(pos) =
                                state.provider_ids.iter().position(|id| id == &provider_id)
                            {
                                state.selected = pos;
                            }
                            // Start editing API key
                            state.start_text_edit("");
                        }
                    }
                }
                KeyCode::Esc => {
                    state.edit_mode = ConfigEditMode::None;
                }
                _ => {}
            }
            return None;
        }

        // Handle custom provider edit mode
        if let ConfigEditMode::CustomProvider {
            focused_field,
            name,
            base_url,
            api_key,
            cursor,
            editing_existing,
            original_name,
        } = &mut state.edit_mode
        {
            // Helper macro to get focused input
            macro_rules! focused_input {
                ($field:expr, $name:expr, $base_url:expr, $api_key:expr) => {
                    match $field {
                        0 => $name,
                        1 => $base_url,
                        _ => $api_key,
                    }
                };
            }
            match code {
                KeyCode::Tab | KeyCode::Down => {
                    // Move to next field
                    *focused_field = (*focused_field + 1).min(2);
                    let new_input = focused_input!(*focused_field, name, base_url, api_key);
                    *cursor = new_input.len();
                }
                KeyCode::BackTab | KeyCode::Up => {
                    // Move to previous field
                    *focused_field = focused_field.saturating_sub(1);
                    let new_input = focused_input!(*focused_field, name, base_url, api_key);
                    *cursor = new_input.len();
                }
                KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Char(_) => {
                    let input = focused_input!(*focused_field, name, base_url, api_key);
                    apply_edit_key(code, input, cursor);
                }
                KeyCode::Enter => {
                    // Validate and save
                    if name.is_empty() {
                        // Name is required, flash error or just don't save
                        return None;
                    }
                    if base_url.is_empty() {
                        // Base URL is required for custom provider
                        return None;
                    }
                    // Clone values before moving out of the borrowed state
                    let name_val = name.clone();
                    let base_url_val = base_url.clone();
                    let api_key_val = if api_key.is_empty() {
                        None
                    } else {
                        Some(api_key.clone())
                    };
                    let was_editing = *editing_existing;
                    let orig_name = original_name.clone();

                    // Reset edit mode
                    self.config_state.edit_mode = ConfigEditMode::None;

                    // Editing an existing provider keeps the fields this form
                    // does not expose (`api`, `native_tools`, `display_name`,
                    // `enabled`); a new provider starts from defaults.
                    let mut instance = if was_editing
                        && let Some(ref old_name) = orig_name
                        && let Some(existing) = self.config.provider.get(old_name)
                    {
                        existing.clone()
                    } else {
                        crate::config::ProviderInstance::new(ProviderType::Custom)
                    };

                    // If editing existing and name changed, remove old entry
                    if was_editing
                        && let Some(ref old_name) = orig_name
                        && old_name != &name_val
                    {
                        self.config.provider.remove(old_name);
                    }

                    instance.base_url = Some(base_url_val);
                    instance.api_key = api_key_val;
                    self.config.upsert_provider(&name_val, instance);
                    self.config_state.load_providers(&self.config);
                    self.auto_populate_models_if_empty();
                    self.persist_config_background();
                    return None;
                }
                KeyCode::Esc => {
                    self.config_state.edit_mode = ConfigEditMode::None;
                }
                _ => {}
            }
            return None;
        }

        // Handle text edit mode
        if let Some((input, cursor)) = state.edit_text_mut() {
            match code {
                KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Char(_) => apply_edit_key(code, input, cursor),
                KeyCode::Enter => {
                    // Save the edited value
                    return self.save_config_edit();
                }
                KeyCode::Esc => {
                    state.cancel_edit();
                }
                _ => {}
            }
        }

        None
    }

    /// Save the current edit in config screen.
    fn save_config_edit(&mut self) -> Option<SendAction> {
        // Extract needed values from state first to avoid borrow issues
        let (value, section, selected) = {
            let state = &self.config_state;
            let value = match &state.edit_mode {
                ConfigEditMode::Text { input, .. } => input.clone(),
                _ => return None,
            };
            (value, state.section, state.selected)
        };

        if self.config_state.text_target == ConfigTextTarget::CacheLifetime {
            return self.save_cache_lifetime_edit(&value, selected);
        }

        match section {
            ConfigSection::Providers => {
                // Save API key for selected provider
                let id = self.config_state.provider_ids.get(selected).cloned();
                if let Some(id) = id {
                    if let Some(instance) = self.config.provider.get_mut(&id) {
                        instance.api_key = if value.is_empty() { None } else { Some(value) };
                    }
                    self.auto_populate_models_if_empty();
                }
            }
            ConfigSection::Models => {
                // Parse comma-separated model list
                let mut models = Vec::new();
                for entry in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    match entry.parse::<ModelSelector>() {
                        Ok(selector) => models.push(selector),
                        Err(error) => {
                            // The edit stays open so the user can fix it;
                            // nothing is persisted from a partial parse.
                            self.set_status(format!("Invalid model selector: {error}"));
                            return None;
                        }
                    }
                }
                match selected {
                    0 => self.config.models.advanced = models,
                    1 => self.config.models.balanced = models,
                    2 => self.config.models.fast = models,
                    3 => self.config.models.image_generation = models,
                    4 => self.config.models.compaction_model = models.into_iter().next(),
                    _ => {}
                }
            }
            ConfigSection::Preferences => match selected {
                0 => {
                    // Parse max_iterations
                    if let Ok(val) = value.parse::<usize>() {
                        self.config.max_iterations = val;
                    }
                }
                3 => return self.save_job_pool_edit(value.trim()),
                _ => {}
            },
        }

        self.config_state.cancel_edit();
        self.persist_config_background();

        None
    }

    /// Saves the text edit of the jobserver pool size: empty is the host's
    /// available parallelism. The daemon applies it to the running pool and
    /// saves it; the config file is written here too when no daemon is
    /// connected. Text that is not a positive number leaves the edit open
    /// with the reason in the status line.
    fn save_job_pool_edit(&mut self, value: &str) -> Option<SendAction> {
        let size = if value.is_empty() {
            None
        } else if let Ok(size) = value.parse::<std::num::NonZeroUsize>() {
            Some(size)
        } else {
            self.set_status("The jobserver pool size is a positive number, or empty");
            return None;
        };
        self.config.sandbox.job_pool_size = size;
        self.config_state.cancel_edit();
        if let Some(client) = self.btw_daemon.clone() {
            let body = crate::daemon::types::JobPoolRequest {
                size: size.map(std::num::NonZeroUsize::get),
            };
            let tx = self.event_tx.clone();
            executor_core::spawn(async move {
                let result = client
                    .put::<_, serde_json::Value>("/api/config/job-pool", &body)
                    .await
                    .map(|_| ())
                    .map_err(crate::tui::error::TuiError::from);
                let _ = tx
                    .send(crate::tui::event::AppEvent::ConfigSaved(result))
                    .await;
            })
            .detach();
        } else {
            self.persist_config_background();
        }
        None
    }

    /// Saves the text edit of the selected provider's cache lifetime.
    ///
    /// Text that is neither a duration nor `permanent` leaves the edit open
    /// with the reason in the status line.
    fn save_cache_lifetime_edit(&mut self, value: &str, selected: usize) -> Option<SendAction> {
        if let Some(id) = self.config_state.provider_ids.get(selected).cloned()
            && let Err(error) = self.config.set_provider_cache_lifetime(&id, value)
        {
            self.set_status(error.to_string());
            return None;
        }
        self.config_state.cancel_edit();
        self.persist_config_background();
        None
    }

    /// Apply a `config` topic push: the daemon is authoritative, so the
    /// local snapshot takes every field the UI surface carries. API keys
    /// are the exception — a push never contains key material, so a
    /// provider the daemon still has a key for keeps the locally stored
    /// material; one the daemon dropped loses its local copy too.
    pub(in crate::tui) fn apply_pushed_config(&mut self, pushed: crate::tui::event::PushedConfig) {
        let full = pushed.full;
        self.config.ssh_servers = pushed.ssh_servers;
        self.config.max_iterations = pushed.max_iterations;
        self.config.appearance.font = full.appearance.font;
        self.config.appearance.theme = full.appearance.theme;
        if let Ok(timezone) = pushed.timezone.parse() {
            self.config.time.timezone = Some(timezone);
        }
        self.config.last_model = full.last_model.and_then(|model| model.parse().ok());
        self.config.preferences.warn_on_model_change = full.preferences.warn_on_model_change;
        self.config.preferences.show_token_usage = full.preferences.show_token_usage;
        self.config.preferences.show_decode_speed = full.preferences.show_decode_speed;
        self.config.preferences.network_policy.mode = full.preferences.network_policy_mode;
        self.config.preferences.network_policy.hosts = full.preferences.network_policy_hosts;
        self.config.sandbox.job_pool_size = full
            .sandbox
            .job_pool_size
            .and_then(std::num::NonZeroUsize::new);
        self.config.models.advanced = parse_selectors(&full.models.advanced);
        self.config.models.balanced = parse_selectors(&full.models.balanced);
        self.config.models.fast = parse_selectors(&full.models.fast);
        self.config.models.image_generation = parse_selectors(&full.models.image_generation);
        self.config.models.compaction_model = full
            .models
            .compaction_model
            .and_then(|model| model.parse().ok());
        let mut providers = std::collections::HashMap::with_capacity(full.providers.len());
        for pushed_provider in full.providers {
            let api_key = if pushed_provider.has_api_key {
                self.config
                    .provider
                    .get(&pushed_provider.id)
                    .and_then(|old| old.api_key.clone())
            } else {
                None
            };
            providers.insert(
                pushed_provider.id.clone(),
                ProviderInstance {
                    provider_type: Some(pushed_provider.provider_type),
                    api_key,
                    native_tools: NativeToolsConfig::from(&pushed_provider.native_tools),
                    api: pushed_provider.api,
                    base_url: pushed_provider.base_url,
                    display_name: pushed_provider.display_name,
                    enabled: pushed_provider.enabled,
                    cache_lifetime: pushed_provider.cache_lifetime,
                },
            );
        }
        self.config.provider = providers;
        self.config_state.load_providers(&self.config);
    }

    /// Auto-populate models from the first configured provider if models are empty.
    fn auto_populate_models_if_empty(&mut self) {
        if self.config.models.has_any_models() {
            return;
        }

        // Find first valid provider
        for (id, instance) in &self.config.provider {
            if !instance.is_valid(id) {
                continue;
            }
            let provider_type = instance.resolve_type(id);
            let provider_str = provider_type.aither_provider();

            if provider_str == "unknown" {
                // Custom/OpenRouter - use preset models
                self.config.models.advanced = vec![ModelSelector::new(id, "claude-opus-4")];
                self.config.models.balanced = vec![ModelSelector::new(id, "claude-sonnet-4")];
                self.config.models.fast = vec![ModelSelector::new(id, "claude-haiku-4")];
                return;
            }

            // Get models from aither-models
            for info in aither_models::all_models() {
                if info.is_deprecated() {
                    continue;
                }
                let model_id = info.id().to_string();
                if !provider_type.can_serve_model(&model_id) {
                    continue;
                }
                let tier = get_model_tier(&model_id);
                if tier == ModelTier::Advanced && self.config.models.advanced.is_empty() {
                    self.config
                        .models
                        .advanced
                        .push(ModelSelector::new(id, &model_id));
                }
                if tier == ModelTier::Balanced && self.config.models.balanced.is_empty() {
                    self.config
                        .models
                        .balanced
                        .push(ModelSelector::new(id, &model_id));
                }
                if tier == ModelTier::Fast && self.config.models.fast.is_empty() {
                    self.config
                        .models
                        .fast
                        .push(ModelSelector::new(id, model_id));
                }
            }
            return;
        }
    }
}

/// The selectors of a UI model list, in order; a selector that does not
/// parse is dropped rather than killing the push.
fn parse_selectors(list: &[String]) -> Vec<ModelSelector> {
    list.iter().filter_map(|entry| entry.parse().ok()).collect()
}

/// `GET /api/config`'s body carries the UI-shaped config under `fullConfig`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigEnvelope {
    full_config: UiFullConfig,
}

/// Write `config` through the daemon: fetch its full config, overwrite the
/// sections the TUI manages from `config`, and `PUT` the result back so
/// sections the TUI never edits keep the daemon's own values.
pub(in crate::tui) async fn persist_config_via_daemon(
    client: &crate::client::DaemonClient,
    config: &crate::config::MayConfig,
) -> Result<(), crate::tui::error::TuiError> {
    let envelope: ConfigEnvelope = client.get("/api/config").await?;
    let mut full = envelope.full_config;
    overlay_tui_config(&mut full, config);
    client
        .put::<_, serde_json::Value>("/api/config", &full)
        .await
        .map(|_| ())?;
    Ok(())
}

/// Overlay `config`'s TUI-managed sections onto `full` — providers, model
/// lists, the preference flags, appearance, `last_model`, `max_iterations`
/// and the jobserver pool size — leaving everything else (browser, remote
/// access, stream, suggestions, recap, sessions, command priority) at the
/// daemon's own values.
fn overlay_tui_config(full: &mut UiFullConfig, config: &crate::config::MayConfig) {
    full.models = UiModelsConfig {
        advanced: config
            .models
            .advanced
            .iter()
            .map(ToString::to_string)
            .collect(),
        balanced: config
            .models
            .balanced
            .iter()
            .map(ToString::to_string)
            .collect(),
        fast: config.models.fast.iter().map(ToString::to_string).collect(),
        image_generation: config
            .models
            .image_generation
            .iter()
            .map(ToString::to_string)
            .collect(),
        compaction_model: config
            .models
            .compaction_model
            .as_ref()
            .map(ToString::to_string),
    };
    full.preferences.warn_on_model_change = config.preferences.warn_on_model_change;
    full.preferences.show_token_usage = config.preferences.show_token_usage;
    full.preferences.show_decode_speed = config.preferences.show_decode_speed;
    full.preferences.network_policy_mode = config.preferences.network_policy.mode;
    full.preferences
        .network_policy_hosts
        .clone_from(&config.preferences.network_policy.hosts);
    full.appearance = crate::daemon::types::UiAppearanceConfig {
        font: config.appearance.font.clone(),
        theme: config.appearance.theme.clone(),
    };
    full.last_model = config.last_model.as_ref().map(ToString::to_string);
    full.max_iterations = config.max_iterations;
    full.sandbox.job_pool_size = config
        .sandbox
        .job_pool_size
        .map(std::num::NonZeroUsize::get);

    // Which providers the daemon stores key material for, so a locally
    // keyless instance is `Keep` rather than `Remove`.
    let keyed: std::collections::HashMap<String, bool> = full
        .providers
        .iter()
        .map(|pushed| (pushed.id.clone(), pushed.has_api_key))
        .collect();
    full.providers = config
        .provider
        .iter()
        .map(|(id, instance)| {
            let daemon_keyed = keyed.get(id).copied().unwrap_or(false);
            let (has_api_key, api_key) = match &instance.api_key {
                Some(key) => (true, Some(SecretUpdate::Replace(key.clone()))),
                None if daemon_keyed => (true, Some(SecretUpdate::Keep)),
                None => (false, Some(SecretUpdate::Remove)),
            };
            UiProviderConfig {
                id: id.clone(),
                provider_type: instance
                    .provider_type
                    .unwrap_or_else(|| ProviderType::from_id(id)),
                has_api_key,
                api_key,
                base_url: instance.base_url.clone(),
                display_name: instance.display_name.clone(),
                enabled: instance.enabled,
                native_tools: crate::daemon::types::UiNativeToolsConfig::from(
                    &instance.native_tools,
                ),
                cache_lifetime: instance.cache_lifetime,
                api: instance.api,
            }
        })
        .collect();
}
