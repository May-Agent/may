use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use crate::profile::query_profile_sessions;

use super::config::{build_ui_config_response, list_skills_for_profile};
use super::profiles::{ContainerPrepareError, ensure_profile_environment_ready};
use super::sessions::list_api_sessions;
use super::state::UiServerState;
use super::types::{BootstrapProfileSessionsError, UiBootstrapResponse};

pub(super) async fn get_bootstrap(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<UiBootstrapResponse>> {
    let config = state.config_snapshot();
    let ui_config = build_ui_config_response(&config);

    let sessions_fut = list_api_sessions(state.as_ref());
    let profiles_fut = async {
        let db = state.db();
        crate::profile::list_profiles(&db).await
    };

    let (sessions, profiles) = tokio::join!(sessions_fut, profiles_fut);
    let profiles = profiles.map_err(super::error::internal_error)?;
    let default_profile_id = profiles.first().map_or_else(
        || state.default_profile_id.clone(),
        |profile| profile.id.clone(),
    );
    let (default_profile_sessions, default_profile_sessions_error, default_profile_skills) =
        if let Some(default_profile) = profiles.first() {
            let default_profile_id = default_profile.id.clone();
            let default_profile_sessions_fut =
                load_default_profile_sessions(state.clone(), default_profile_id.clone());
            let default_profile_skills_fut =
                list_skills_for_profile(state.as_ref(), default_profile_id.as_str());
            let (
                (default_profile_sessions, default_profile_sessions_error),
                default_profile_skills,
            ) = tokio::join!(default_profile_sessions_fut, default_profile_skills_fut);
            (
                default_profile_sessions,
                default_profile_sessions_error,
                default_profile_skills.map_err(super::error::internal_error)?,
            )
        } else {
            (None, None, Vec::new())
        };

    Ok(Json(UiBootstrapResponse {
        sessions: sessions.map_err(super::error::internal_error)?,
        profiles,
        default_profile_id,
        config: ui_config,
        default_profile_sessions,
        default_profile_sessions_error,
        default_profile_skills,
    }))
}

async fn load_default_profile_sessions(
    state: Arc<UiServerState>,
    profile_id: String,
) -> (
    Option<crate::profile::ProfileSessions>,
    Option<BootstrapProfileSessionsError>,
) {
    let db = state.db();

    match ensure_profile_environment_ready(&state, &db, &profile_id, false).await {
        Ok(()) => match query_profile_sessions(&db, &profile_id).await {
            Ok(sessions) => (Some(sessions), None),
            Err(error) => unavailable_profile_sessions(profile_id, None, error.to_string()),
        },
        Err(ContainerPrepareError::Domain { code, message }) => {
            unavailable_profile_sessions(profile_id, Some(code), message)
        }
        Err(ContainerPrepareError::Sky(error)) => {
            unavailable_profile_sessions(profile_id, None, error.to_string())
        }
    }
}

fn unavailable_profile_sessions(
    profile_id: String,
    code: Option<super::types::ApiErrorCode>,
    message: String,
) -> (
    Option<crate::profile::ProfileSessions>,
    Option<BootstrapProfileSessionsError>,
) {
    tracing::warn!(
        profile_id = %profile_id,
        error_code = ?code,
        error = %message,
        "Bootstrap could not preload default profile sessions"
    );
    (None, Some(BootstrapProfileSessionsError { code, message }))
}
