//! `POST /api/rules/import`: `/import-claude` and `/import-codex`.
//!
//! Copies the agent's skills into the profile's skills, then opens the main
//! session that rewrites the agent's global rules into `~/.may/AGENTS.md` and
//! starts its first turn with the import brief.

use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use crate::rules_import::{RulesImport, RulesImportRequest, RulesImportResponse, copy_skills};
use crate::session::MayHomePaths;
use crate::session::import::ForeignAgent;
use crate::skill::SkillManager;
use crate::ui_event_bus::UiEventTopics;

use super::error::{bad_request_error, internal_error};
use super::model_resolution::resolve_default_model;
use super::state::DaemonState;
use super::types::ChatRequest;

/// `POST /api/rules/import`.
pub(super) async fn import_rules(
    Json(req): Json<RulesImportRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<RulesImportResponse>> {
    let source = ForeignAgent::from_label(&req.source).ok_or_else(|| {
        bad_request_error(format!(
            "unknown rules source `{}`: expected `claude` or `codex`",
            req.source
        ))
    })?;
    let profile_id = req
        .profile_id
        .unwrap_or_else(|| state.default_profile_id.clone());
    let home = crate::config::MayConfig::home_dir().map_err(internal_error)?;

    let skills = SkillManager::from_profile_home(
        &MayHomePaths::new(state.may_dir.clone()).profile_home(&profile_id),
    );
    let report = copy_skills(&source.skills_dir(&home), &skills)
        .await
        .map_err(internal_error)?;

    let import = RulesImport::new(source, &home, &state.may_dir);
    let rules_file = import.rules_file().display().to_string();
    let has_rules = async_fs::metadata(import.rules_file())
        .await
        .is_ok_and(|metadata| metadata.is_file());
    let session_id = if has_rules {
        Some(open_import_session(&state, &profile_id, &import).await?)
    } else {
        None
    };
    Ok(Json(RulesImportResponse {
        session_id,
        rules_file,
        imported_skills: report.imported,
        skipped_skills: report.skipped,
    }))
}

/// Creates the import session in `profile_id` and starts its first turn.
async fn open_import_session(
    state: &Arc<DaemonState>,
    profile_id: &str,
    import: &RulesImport,
) -> SkyResult<String> {
    let model = resolve_default_model(&state.config_snapshot(), None)
        .ok_or_else(|| bad_request_error("No model configured"))?;
    let session_id = super::sessions::create_session_for_profile(
        state,
        super::sessions::SessionName::Generate,
        Some(profile_id),
        &model,
    )
    .await?;
    state
        .db()
        .set_session_rules_import(&session_id, import.source())
        .await
        .map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS | UiEventTopics::SESSION_RUNTIMES);
    super::chat::start_prompt_turn(
        state,
        ChatRequest {
            session_id: session_id.clone(),
            content: import.brief(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            delivery_mode: crate::session::PendingUserMessageMode::default(),
            pending_entry_id: None,
        },
    )
    .await?;
    Ok(session_id)
}
