//! Config-mode key handling for App.

use crossterm::event::{KeyCode, KeyModifiers};

use super::super::state::{AppMode, ConfigEditMode, ConfigSection, detect_api_key_from_env};
use super::{App, SendAction, char_to_byte_index};
use crate::config::{ModelTier, ProviderType, get_model_tier};
use crate::model_selector::ModelSelector;

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
                        return Some(SendAction::StartCopilotAuth);
                    }
                    if id == "chatgpt" {
                        return Some(SendAction::StartChatgptAuth);
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
                            cursor: id.chars().count(),
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
                            return Some(SendAction::StartChatgptAuth);
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
                    *cursor = new_input.chars().count();
                }
                KeyCode::BackTab | KeyCode::Up => {
                    // Move to previous field
                    *focused_field = focused_field.saturating_sub(1);
                    let new_input = focused_input!(*focused_field, name, base_url, api_key);
                    *cursor = new_input.chars().count();
                }
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        *cursor -= 1;
                        let input = focused_input!(*focused_field, name, base_url, api_key);
                        let byte_idx = char_to_byte_index(input, *cursor);
                        input.remove(byte_idx);
                    }
                }
                KeyCode::Delete => {
                    let input = focused_input!(*focused_field, name, base_url, api_key);
                    let char_count = input.chars().count();
                    if *cursor < char_count {
                        let byte_idx = char_to_byte_index(input, *cursor);
                        input.remove(byte_idx);
                    }
                }
                KeyCode::Left => {
                    *cursor = cursor.saturating_sub(1);
                }
                KeyCode::Right => {
                    let input = focused_input!(*focused_field, name, base_url, api_key);
                    let char_count = input.chars().count();
                    *cursor = (*cursor + 1).min(char_count);
                }
                KeyCode::Home => {
                    *cursor = 0;
                }
                KeyCode::End => {
                    let input = focused_input!(*focused_field, name, base_url, api_key);
                    *cursor = input.chars().count();
                }
                KeyCode::Char(c) => {
                    let input = focused_input!(*focused_field, name, base_url, api_key);
                    let byte_idx = char_to_byte_index(input, *cursor);
                    input.insert(byte_idx, c);
                    *cursor += 1;
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
                KeyCode::Backspace => {
                    if *cursor > 0 {
                        *cursor -= 1;
                        let byte_idx = char_to_byte_index(input, *cursor);
                        input.remove(byte_idx);
                    }
                }
                KeyCode::Delete => {
                    let char_count = input.chars().count();
                    if *cursor < char_count {
                        let byte_idx = char_to_byte_index(input, *cursor);
                        input.remove(byte_idx);
                    }
                }
                KeyCode::Left => {
                    *cursor = cursor.saturating_sub(1);
                }
                KeyCode::Right => {
                    let char_count = input.chars().count();
                    *cursor = (*cursor + 1).min(char_count);
                }
                KeyCode::Home => {
                    *cursor = 0;
                }
                KeyCode::End => {
                    *cursor = input.chars().count();
                }
                KeyCode::Char(c) => {
                    let byte_idx = char_to_byte_index(input, *cursor);
                    input.insert(byte_idx, c);
                    *cursor += 1;
                }
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
                let models: Vec<ModelSelector> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        s.parse::<ModelSelector>()
                            .expect("tui config edit must receive explicit model selectors")
                    })
                    .collect();
                match selected {
                    0 => self.config.models.advanced = models,
                    1 => self.config.models.balanced = models,
                    2 => self.config.models.fast = models,
                    3 => self.config.models.image_generation = models,
                    _ => {}
                }
            }
            ConfigSection::Preferences => {
                if selected == 0 {
                    // Parse max_iterations
                    if let Ok(val) = value.parse::<usize>() {
                        self.config.max_iterations = val;
                    }
                }
            }
        }

        self.config_state.cancel_edit();
        self.persist_config_background();

        None
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
