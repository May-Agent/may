//! Profile and folder CRUD endpoints, session pin/archive/move operations.

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::Arc;

use skyzen::Result as SkyResult;
use skyzen::extract::Query;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use crate::db::{ChatDbError, CreateProfileParams, ProfileEnvironmentKind, UpdateProfileParams};
use crate::profile::{ProfileInfo, ProfilePreferences, query_profile_sessions};
use crate::ui_event_bus::UiEventTopics;

use super::error::{bad_request_error, internal_error, json_ok_response, typed_error_response};
use super::state::{ContainerPrepareStage, ContainerPrepareStatus, UiServerState};
use super::types::ApiErrorCode;

// ── Request / Response types ────────────────────────────────

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateProfileRequest {
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default = "default_env")]
    pub environment: ProfileEnvironmentKind,
    #[serde(default)]
    pub container_image: Option<String>,
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub preferences: Option<ProfilePreferences>,
    #[serde(default)]
    pub location_aware: bool,
}

const fn default_env() -> ProfileEnvironmentKind {
    ProfileEnvironmentKind::Local
}

fn map_profile_error(err: ChatDbError) -> skyzen::Error {
    match err {
        ChatDbError::PinnedLimitExceeded { .. } | ChatDbError::InvalidProfileEnvironment(_) => {
            bad_request_error(err)
        }
        _ => internal_error(err),
    }
}

/// Domain-specific error from container environment preparation.
///
/// Separates domain errors (which carry a machine-readable [`ApiErrorCode`]) from
/// infrastructure errors (which remain as [`skyzen::Error`]).
pub(super) enum ContainerPrepareError {
    /// Container-specific error with a machine-readable code for the frontend.
    Domain { code: ApiErrorCode, message: String },
    /// Infrastructure error (DB failures, serialization, etc.).
    Sky(skyzen::Error),
}

impl From<skyzen::Error> for ContainerPrepareError {
    fn from(e: skyzen::Error) -> Self {
        Self::Sky(e)
    }
}

pub(super) async fn ensure_profile_environment_ready(
    state: &Arc<UiServerState>,
    db: &crate::db::ChatDb,
    profile_id: &str,
    allow_managed_podman_download: bool,
) -> Result<(), ContainerPrepareError> {
    let profile = db
        .get_profile(profile_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("Profile not found"))?;
    let environment = ProfileEnvironmentKind::from_str(profile.environment.as_str())
        .map_err(map_profile_error)?;

    if environment == ProfileEnvironmentKind::Remote {
        let config = state.config_snapshot();
        crate::profile::remote::resolve_remote_profile_server(&profile, &config.ssh_servers)
            .map_err(bad_request_error)?;
        state.clear_container_prepare_status(profile_id);
        return Ok(());
    }

    if environment != ProfileEnvironmentKind::Container {
        state.clear_container_prepare_status(profile_id);
        return Ok(());
    }

    let manager = match state
        .resolve_container_manager(allow_managed_podman_download)
        .await
    {
        Ok(manager) => manager,
        Err(crate::container::ContainerError::ManagedPodmanDownloadApprovalRequired) => {
            let message = "Managed Podman is not installed. Approve download before preparing this container profile.".to_string();
            state.set_container_prepare_status(ContainerPrepareStatus {
                profile_id: profile_id.to_string(),
                stage: ContainerPrepareStage::Failed,
                summary: message.clone(),
                recent_logs: Vec::new(),
            });
            return Err(ContainerPrepareError::Domain {
                code: ApiErrorCode::ManagedPodmanApprovalRequired,
                message,
            });
        }
        Err(error) => {
            let message = format!(
                "Container runtime is unavailable: start Docker/Podman/OrbStack and retry ({error})"
            );
            state.set_container_prepare_status(ContainerPrepareStatus {
                profile_id: profile_id.to_string(),
                stage: ContainerPrepareStage::Failed,
                summary: message.clone(),
                recent_logs: Vec::new(),
            });
            return Err(ContainerPrepareError::Domain {
                code: ApiErrorCode::ContainerRuntimeUnavailable,
                message,
            });
        }
    };

    let container_preferences = db
        .load_container_profile_preferences(&profile)
        .await
        .map_err(internal_error)?;
    let default_image_tag = crate::container::profile::default_image_tag(
        &state.may_dir,
        &profile,
        container_preferences.clone(),
    )
    .await
    .map_err(internal_error)?;
    let image_spec = crate::container::profile::image_spec_for_profile(
        &state.may_dir,
        &profile,
        &default_image_tag,
        container_preferences,
    )
    .map_err(internal_error)?;

    if !state.try_begin_container_prepare(profile_id).await {
        let summary = state.get_container_prepare_status(profile_id).map_or_else(
            || "Container image is still preparing".to_string(),
            |status| status.summary,
        );
        return Err(ContainerPrepareError::Domain {
            code: ApiErrorCode::ContainerPreparing,
            message: summary,
        });
    }

    let prepare_result = async {
        let checking_status = ContainerPrepareStatus {
            profile_id: profile_id.to_string(),
            stage: ContainerPrepareStage::Checking,
            summary: format!("checking image {}", image_spec.tag),
            recent_logs: Vec::new(),
        };
        state.set_container_prepare_status(checking_status);

        let (progress_tx, mut progress_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::container::image::ImageBuildProgress>();
        let progress_state = state.clone();
        let progress_profile_id = profile_id.to_string();
        let progress_task = tokio::spawn(async move {
            let mut recent_logs = VecDeque::new();
            while let Some(progress) = progress_rx.recv().await {
                if progress.phase == crate::container::image::ImageBuildPhase::BuildOutput {
                    if recent_logs.len() >= 12 {
                        let _ = recent_logs.pop_front();
                    }
                    recent_logs.push_back(progress.message.clone());
                }

                let stage = match progress.phase {
                    crate::container::image::ImageBuildPhase::CheckingExisting => {
                        ContainerPrepareStage::Checking
                    }
                    crate::container::image::ImageBuildPhase::AlreadyExists
                    | crate::container::image::ImageBuildPhase::BuildCompleted => {
                        ContainerPrepareStage::Ready
                    }
                    crate::container::image::ImageBuildPhase::BuildStarting
                    | crate::container::image::ImageBuildPhase::BuildOutput => {
                        ContainerPrepareStage::Building
                    }
                };

                let status = ContainerPrepareStatus {
                    profile_id: progress_profile_id.clone(),
                    stage,
                    summary: progress.message,
                    recent_logs: recent_logs.iter().cloned().collect(),
                };
                progress_state.set_container_prepare_status(status);
            }
        });

        let ensure_result = crate::container::image::ensure_image_with_progress(
            manager.runtime(),
            &image_spec,
            move |progress| {
                let _ = progress_tx.send(progress);
            },
        )
        .await;

        let _ = progress_task.await;

        match ensure_result {
            Ok(()) => {
                state.clear_container_prepare_status(profile_id);
                Ok(())
            }
            Err(error) => {
                let recent_logs = state
                    .get_container_prepare_status(profile_id)
                    .map(|status| status.recent_logs)
                    .unwrap_or_default();

                state.set_container_prepare_status(ContainerPrepareStatus {
                    profile_id: profile_id.to_string(),
                    stage: ContainerPrepareStage::Failed,
                    summary: format!("container build failed: {error}"),
                    recent_logs,
                });

                Err(ContainerPrepareError::Domain {
                    code: ApiErrorCode::ContainerBuildFailed,
                    message: format!("Container image build failed: {error}"),
                })
            }
        }
    }
    .await;

    state.finish_container_prepare(profile_id);
    prepare_result
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpdateProfileRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub environment: Option<ProfileEnvironmentKind>,
    #[serde(default)]
    pub container_image: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub preferences: Option<ProfilePreferences>,
    #[serde(default)]
    pub location_aware: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateFolderRequest {
    pub name: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpdateFolderRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i32>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MoveToFolderRequest {
    pub folder_id: Option<String>,
}

// ── Profile endpoints ───────────────────────────────────────

pub(super) async fn list_profiles(
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<Vec<ProfileInfo>>> {
    let db = state.db();

    let profiles = crate::profile::list_profiles(&db)
        .await
        .map_err(internal_error)?;
    Ok(Json(profiles))
}

pub(super) async fn create_profile(
    Json(req): Json<CreateProfileRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ProfileInfo>> {
    let db = state.db();

    let id = uuid::Uuid::new_v4().to_string();
    let model = db
        .create_profile(CreateProfileParams {
            id: &id,
            name: &req.name,
            environment: req.environment,
            container_image: req.container_image.as_deref(),
            slug: None,
            icon: req.icon.as_deref(),
            persona: req.persona.as_deref(),
            preferences: req.preferences.as_ref(),
            location_aware: req.location_aware,
        })
        .await
        .map_err(map_profile_error)?;
    super::remote_access::sync_remote_access_from_state(&state)
        .await
        .map_err(internal_error)?;

    Ok(Json(
        db.profile_info_from_model(model)
            .await
            .map_err(map_profile_error)?,
    ))
}

pub(super) async fn update_profile(
    params: Params,
    Json(req): Json<UpdateProfileRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<ProfileInfo>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    db.update_profile(
        &id,
        UpdateProfileParams {
            name: req.name.as_deref(),
            icon: req.icon.as_deref().map(Some),
            environment: req.environment,
            container_image: req.container_image.as_deref().map(Some),
            container_id: None,
            sort_order: req.sort_order,
            persona: req.persona.as_deref().map(Some),
            preferences: req.preferences.as_ref().map(Some),
            location_aware: req.location_aware,
        },
    )
    .await
    .map_err(map_profile_error)?;

    let model = db
        .get_profile(&id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("Profile not found"))?;
    super::remote_access::sync_remote_access_from_state(&state)
        .await
        .map_err(internal_error)?;

    Ok(Json(
        db.profile_info_from_model(model)
            .await
            .map_err(map_profile_error)?,
    ))
}

pub(super) async fn delete_profile(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    db.delete_profile(&id).await.map_err(internal_error)?;
    super::remote_access::sync_remote_access_from_state(&state)
        .await
        .map_err(internal_error)?;
    Ok(Json(super::types::API_OK))
}

pub(super) async fn get_profile_sessions(
    params: Params,
    Query(query): Query<ProfileSessionsQuery>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<skyzen::Response> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    if let Err(err) =
        ensure_profile_environment_ready(&state, &db, &id, query.allow_managed_podman_download)
            .await
    {
        return match err {
            ContainerPrepareError::Domain { code, message } => Ok(typed_error_response(
                skyzen::StatusCode::BAD_REQUEST,
                code,
                message,
            )),
            ContainerPrepareError::Sky(e) => Err(e),
        };
    }

    let result = query_profile_sessions(&db, &id)
        .await
        .map_err(internal_error)?;
    json_ok_response(&result)
}

#[derive(Debug, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProfileSessionsQuery {
    #[serde(default)]
    allow_managed_podman_download: bool,
}

// ── Folder endpoints ────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FolderResponse {
    id: String,
    profile_id: String,
    name: String,
    sort_order: i32,
}

pub(super) async fn create_folder(
    params: Params,
    Json(req): Json<CreateFolderRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<FolderResponse>> {
    let profile_id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    let folder_id = uuid::Uuid::new_v4().to_string();
    let model = db
        .create_folder(&folder_id, &profile_id, &req.name)
        .await
        .map_err(map_profile_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);

    Ok(Json(FolderResponse {
        id: model.id,
        profile_id: model.profile_id,
        name: model.name,
        sort_order: model.sort_order,
    }))
}

pub(super) async fn update_folder(
    params: Params,
    Json(req): Json<UpdateFolderRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    if let Some(name) = req.name {
        db.update_folder(&id, &name).await.map_err(internal_error)?;
    }
    if let Some(sort_order) = req.sort_order {
        db.update_folder_sort_order(&id, sort_order)
            .await
            .map_err(internal_error)?;
    }
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);

    Ok(Json(super::types::API_OK))
}

pub(super) async fn delete_folder(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    db.delete_folder(&id).await.map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
    Ok(Json(super::types::API_OK))
}

// ── Session pin/archive/move endpoints ──────────────────────

pub(super) async fn pin_session(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    db.pin_session(&id).await.map_err(map_profile_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
    Ok(Json(super::types::API_OK))
}

pub(super) async fn unpin_session(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    db.unpin_session(&id).await.map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
    Ok(Json(super::types::API_OK))
}

pub(super) async fn archive_session(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    db.archive_session(&id).await.map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
    Ok(Json(super::types::API_OK))
}

pub(super) async fn unarchive_session(
    params: Params,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    db.unarchive_session(&id).await.map_err(internal_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
    Ok(Json(super::types::API_OK))
}

pub(super) async fn move_to_folder(
    params: Params,
    Json(req): Json<MoveToFolderRequest>,
    State(state): State<Arc<UiServerState>>,
) -> SkyResult<Json<super::types::ApiHealth>> {
    let id = params.get("id").map_err(bad_request_error)?.to_string();

    let db = state.db();

    db.move_session_to_folder(&id, req.folder_id.as_deref())
        .await
        .map_err(map_profile_error)?;
    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
    Ok(Json(super::types::API_OK))
}
