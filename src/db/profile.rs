//! Profile + folder CRUD, profile preferences, session profile
//! operations (pin/unpin/archive/move to folder), pinned counters, and
//! default profile bootstrapping.

use crate::profile::{
    ContainerMountPreference, ContainerProfilePreferences, ProfileInfo, ProfilePreferences,
};
use crate::session::SessionRuntimeState;

use super::common::{
    CreateProfileParams, DefaultProfile, I32ValueRow, I64ValueRow, ProfileEnvironmentKind,
    ProfileMountRow, SessionIdRow, UpdateProfileParams, build_profile_preferences, db_custom_error,
    normalize_profile_preferences, overflow_error, parse_profile_environment, row_not_found,
    storage_error,
};
use super::entity::{folders, profiles, sessions};
use super::{ChatDb, ChatDbError, query};

impl ChatDb {
    pub async fn load_profile_preferences(
        &self,
        profile: &profiles::Model,
    ) -> Result<Option<ProfilePreferences>, ChatDbError> {
        let mounts = query!(
            self.db,
            "SELECT * FROM profile_container_mounts WHERE profile_id = ? ORDER BY ordinal ASC",
            profile.id.as_str()
        )
        .fetch_all::<ProfileMountRow>()
        .await?
        .into_iter()
        .map(|row| {
            let access = row.access.parse().map_err(|error| {
                storage_error(db_custom_error("invalid profile mount access: ", &error))
            })?;
            Ok(ContainerMountPreference {
                host_path: row.host_path,
                container_path: row.container_path,
                access,
            })
        })
        .collect::<Result<Vec<_>, ChatDbError>>()?;
        Ok(build_profile_preferences(profile, mounts))
    }

    pub async fn profile_info_from_model(
        &self,
        profile: profiles::Model,
    ) -> Result<crate::profile::ProfileInfo, ChatDbError> {
        let preferences = self.load_profile_preferences(&profile).await?;
        crate::profile::ProfileInfo::from_db_parts(profile, preferences)
    }

    pub async fn load_container_profile_preferences(
        &self,
        profile: &profiles::Model,
    ) -> Result<Option<ContainerProfilePreferences>, ChatDbError> {
        Ok(self
            .load_profile_preferences(profile)
            .await?
            .and_then(|preferences| preferences.container))
    }

    // ── Profile CRUD ────────────────────────────────────────────

    /// Create a new profile.
    pub async fn create_profile(
        &self,
        params: CreateProfileParams<'_>,
    ) -> Result<profiles::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let sort_order = self.max_profile_sort_order().await? + 1;
        let preferences = normalize_profile_preferences(params.environment, params.preferences)?;
        let container_image = match params.environment {
            ProfileEnvironmentKind::Local | ProfileEnvironmentKind::Remote => None,
            ProfileEnvironmentKind::Container => params.container_image,
        };
        let remote_ssh_server_id = preferences
            .as_ref()
            .and_then(|value| value.remote.as_ref())
            .map(|value| value.ssh_server_id.as_str());
        let container_dockerfile = preferences
            .as_ref()
            .and_then(|value| value.container.as_ref())
            .and_then(|value| value.dockerfile.as_deref());
        let container_mounts = preferences
            .as_ref()
            .and_then(|value| value.container.as_ref())
            .map(|value| value.mounts.as_slice())
            .unwrap_or(&[]);
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "INSERT INTO profiles (id, slug, name, icon, environment, container_image, container_id, persona, remote_ssh_server_id, container_dockerfile, location_aware, pinned_limit, sort_order, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params.id,
            params.slug,
            params.name,
            params.icon,
            params.environment.as_str(),
            container_image,
            Option::<String>::None,
            params.persona,
            remote_ssh_server_id,
            container_dockerfile,
            params.location_aware,
            10_i32,
            sort_order,
            now.as_str(),
            now.as_str()
        )
        .execute()
        .await?;
        for (ordinal, mount) in container_mounts.iter().enumerate() {
            query!(
                tx,
                "INSERT INTO profile_container_mounts (profile_id, ordinal, host_path, container_path, access) VALUES (?, ?, ?, ?, ?)",
                params.id,
                i64::try_from(ordinal).map_err(|error| overflow_error("profile mount ordinal", &error))?,
                mount.host_path.as_str(),
                mount.container_path.as_str(),
                mount.access.as_str()
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        self.get_profile(params.id)
            .await?
            .ok_or_else(|| row_not_found(format!("Profile not found after insert: {}", params.id)))
    }

    /// List all profiles ordered by `sort_order`.
    pub async fn list_profiles(&self) -> Result<Vec<profiles::Model>, ChatDbError> {
        Ok(
            query!(self.db, "SELECT * FROM profiles ORDER BY sort_order ASC")
                .fetch_all::<profiles::Model>()
                .await?,
        )
    }

    /// Get a profile by ID.
    pub async fn get_profile(&self, id: &str) -> Result<Option<profiles::Model>, ChatDbError> {
        Ok(query!(self.db, "SELECT * FROM profiles WHERE id = ?", id)
            .fetch_optional::<profiles::Model>()
            .await?)
    }

    /// Update profile fields.
    pub async fn update_profile(
        &self,
        id: &str,
        params: UpdateProfileParams<'_>,
    ) -> Result<(), ChatDbError> {
        let mut existing = self
            .get_profile(id)
            .await?
            .ok_or_else(|| row_not_found(format!("Profile not found: {id}")))?;
        if let Some(name) = params.name {
            existing.name = name.to_string();
        }
        if let Some(icon) = params.icon {
            existing.icon = icon.map(ToOwned::to_owned);
        }
        if let Some(environment) = params.environment {
            existing.environment = environment.as_str().to_string();
            if matches!(
                environment,
                ProfileEnvironmentKind::Local | ProfileEnvironmentKind::Remote
            ) {
                existing.container_image = None;
                existing.container_id = None;
            }
        }
        if let Some(container_image) = params.container_image {
            existing.container_image = container_image.map(ToOwned::to_owned);
        }
        if let Some(container_id) = params.container_id {
            existing.container_id = container_id.map(ToOwned::to_owned);
        }
        if let Some(sort_order) = params.sort_order {
            existing.sort_order = sort_order;
        }
        if let Some(persona) = params.persona {
            existing.persona = persona.map(ToOwned::to_owned);
        }
        if let Some(location_aware) = params.location_aware {
            existing.location_aware = location_aware;
        }
        let environment = parse_profile_environment(existing.environment.as_str())?;
        let current_preferences = self.load_profile_preferences(&existing).await?;
        let merged_preferences = match params.preferences {
            Some(preferences) => normalize_profile_preferences(environment, preferences)?,
            None => current_preferences,
        };
        existing.remote_ssh_server_id = merged_preferences
            .as_ref()
            .and_then(|value| value.remote.as_ref())
            .map(|value| value.ssh_server_id.clone());
        existing.container_dockerfile = merged_preferences
            .as_ref()
            .and_then(|value| value.container.as_ref())
            .and_then(|value| value.dockerfile.clone());
        let container_mounts = merged_preferences
            .as_ref()
            .and_then(|value| value.container.as_ref())
            .map(|value| value.mounts.as_slice())
            .unwrap_or(&[]);
        existing.updated_at = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "UPDATE profiles SET name = ?, icon = ?, environment = ?, container_image = ?, container_id = ?, persona = ?, remote_ssh_server_id = ?, container_dockerfile = ?, location_aware = ?, pinned_limit = ?, sort_order = ?, created_at = ?, updated_at = ? WHERE id = ?",
            existing.name.as_str(),
            existing.icon.as_deref(),
            existing.environment.as_str(),
            existing.container_image.as_deref(),
            existing.container_id.as_deref(),
            existing.persona.as_deref(),
            existing.remote_ssh_server_id.as_deref(),
            existing.container_dockerfile.as_deref(),
            existing.location_aware,
            existing.pinned_limit,
            existing.sort_order,
            existing.created_at.as_str(),
            existing.updated_at.as_str(),
            id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM profile_container_mounts WHERE profile_id = ?",
            id
        )
        .execute()
        .await?;
        for (ordinal, mount) in container_mounts.iter().enumerate() {
            query!(
                tx,
                "INSERT INTO profile_container_mounts (profile_id, ordinal, host_path, container_path, access) VALUES (?, ?, ?, ?, ?)",
                id,
                i64::try_from(ordinal).map_err(|error| overflow_error("profile mount ordinal", &error))?,
                mount.host_path.as_str(),
                mount.container_path.as_str(),
                mount.access.as_str()
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Delete a profile and reassign its sessions to the default profile.
    pub async fn delete_profile(&self, id: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE sessions SET profile_id = NULL WHERE profile_id = ?",
            id
        )
        .execute()
        .await?;
        query!(self.db, "DELETE FROM folders WHERE profile_id = ?", id)
            .execute()
            .await?;
        query!(self.db, "DELETE FROM profiles WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    /// Get a profile by its well-known slug ("daily", "work", "local").
    pub async fn get_profile_by_slug(
        &self,
        slug: &str,
    ) -> Result<Option<profiles::Model>, ChatDbError> {
        Ok(
            query!(self.db, "SELECT * FROM profiles WHERE slug = ?", slug)
                .fetch_optional::<profiles::Model>()
                .await?,
        )
    }

    /// Ensure the three default profiles (daily, work, local) exist.
    ///
    /// Returns the ID of the first profile (daily).
    pub async fn ensure_default_profiles(&self) -> Result<String, ChatDbError> {
        let local_hostname = hostname::get().map_or_else(
            |_| "local".to_string(),
            |h| h.to_string_lossy().into_owned(),
        );

        let defaults = [
            DefaultProfile {
                slug: "daily",
                name: "Daily",
                icon: "\u{1F305}", // 🌅
                environment: "container",
                location_aware: true,
            },
            DefaultProfile {
                slug: "work",
                name: "Work",
                icon: "\u{1F4BC}", // 💼
                environment: "container",
                location_aware: false,
            },
            DefaultProfile {
                slug: "local",
                name: &local_hostname,
                icon: "\u{1F5A5}\u{FE0F}", // 🖥️
                environment: "local",
                location_aware: false,
            },
        ];

        let mut first_id = None;
        for def in &defaults {
            let ensured = if let Some(existing) = self.get_profile_by_slug(def.slug).await? {
                existing
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                self.create_profile(CreateProfileParams {
                    id: &id,
                    name: def.name,
                    environment: parse_profile_environment(def.environment)?,
                    container_image: None,
                    slug: Some(def.slug),
                    icon: Some(def.icon),
                    persona: None,
                    preferences: None,
                    location_aware: def.location_aware,
                })
                .await?
            };

            if first_id.is_none() {
                first_id = Some(ensured.id.clone());
            }
        }

        // first_id is always set because we iterate at least one default profile
        Ok(first_id.expect("at least one default profile must be created"))
    }

    async fn max_profile_sort_order(&self) -> Result<i32, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT COALESCE(MAX(sort_order), -1) AS value FROM profiles"
        )
        .fetch_one::<I32ValueRow>()
        .await?
        .value)
    }

    // ── Folder CRUD ─────────────────────────────────────────────

    /// Create a folder in a profile.
    pub async fn create_folder(
        &self,
        id: &str,
        profile_id: &str,
        name: &str,
    ) -> Result<folders::Model, ChatDbError> {
        self.ensure_pinned_capacity(profile_id).await?;
        let now = jiff::Timestamp::now().to_string();
        let max_order = self.max_folder_sort_order(profile_id).await?;
        let sort_order = max_order + 1;
        query!(
            self.db,
            "INSERT INTO folders (id, profile_id, name, sort_order, created_at) VALUES (?, ?, ?, ?, ?)",
            id,
            profile_id,
            name,
            sort_order,
            now.clone()
        )
        .execute()
        .await?;
        Ok(folders::Model {
            id: id.to_string(),
            profile_id: profile_id.to_string(),
            name: name.to_string(),
            sort_order,
            created_at: now,
        })
    }

    /// List folders for a profile.
    pub async fn list_folders(&self, profile_id: &str) -> Result<Vec<folders::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM folders WHERE profile_id = ? ORDER BY sort_order ASC",
            profile_id
        )
        .fetch_all::<folders::Model>()
        .await?)
    }

    /// Update a folder's name.
    pub async fn update_folder(&self, id: &str, name: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE folders SET name = ? WHERE id = ?",
            name,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Update a folder's sort order.
    pub async fn update_folder_sort_order(
        &self,
        id: &str,
        sort_order: i32,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE folders SET sort_order = ? WHERE id = ?",
            sort_order,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Delete a folder and unpin its sessions.
    pub async fn delete_folder(&self, id: &str) -> Result<(), ChatDbError> {
        query!(self.db, "UPDATE sessions SET folder_id = NULL, pinned = FALSE, pin_sort_order = NULL WHERE folder_id = ?", id)
            .execute()
            .await?;
        query!(self.db, "DELETE FROM folders WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    async fn max_folder_sort_order(&self, profile_id: &str) -> Result<i32, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT COALESCE(MAX(sort_order), -1) AS value FROM folders WHERE profile_id = ?",
            profile_id
        )
        .fetch_one::<I32ValueRow>()
        .await?
        .value)
    }

    // ── Session Profile Operations ──────────────────────────────

    /// Pin a session in its profile.
    pub async fn pin_session(&self, id: &str) -> Result<(), ChatDbError> {
        let session = self
            .get_session(id)
            .await?
            .ok_or_else(|| row_not_found(format!("Session not found: {id}")))?;

        let profile_id = session
            .profile_id
            .as_deref()
            .ok_or_else(|| row_not_found(format!("Session {id} is not attached to a profile")))?;

        // Adding a new standalone pinned session consumes one pinned slot.
        // Moving a folder-contained pinned session to top-level also consumes one slot.
        if !session.pinned || session.folder_id.is_some() {
            self.ensure_pinned_capacity(profile_id).await?;
        }

        let max_order = self.max_pin_sort_order(profile_id).await?;
        query!(
            self.db,
            "UPDATE sessions SET pinned = TRUE, archived = FALSE, pin_sort_order = ? WHERE id = ?",
            max_order + 1,
            id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Unpin a session.
    pub async fn unpin_session(&self, id: &str) -> Result<(), ChatDbError> {
        query!(self.db, "UPDATE sessions SET pinned = FALSE, folder_id = NULL, pin_sort_order = NULL WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    /// Archive a session.
    pub async fn archive_session(&self, id: &str) -> Result<(), ChatDbError> {
        let runtime = self.load_session_runtime_summary(id).await?;
        if runtime.is_active() {
            let reason = if runtime.state.is_active() {
                format!("runtime state is {}", runtime.state.as_str())
            } else {
                "pending follow-up work exists".to_string()
            };
            return Err(storage_error(format!(
                "cannot archive active session {id} while {reason}"
            )));
        }
        query!(self.db, "UPDATE sessions SET archived = TRUE, pinned = FALSE, folder_id = NULL, pin_sort_order = NULL WHERE id = ?", id)
            .execute()
            .await?;
        Ok(())
    }

    /// Unarchive (reopen) a session.
    pub async fn unarchive_session(&self, id: &str) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(self.db, "UPDATE sessions SET archived = FALSE, pinned = FALSE, folder_id = NULL, pin_sort_order = NULL, updated_at = ? WHERE id = ?", now, id)
            .execute()
            .await?;
        Ok(())
    }

    /// Move a session to a folder (or out of a folder if `folder_id` is None).
    pub async fn move_session_to_folder(
        &self,
        session_id: &str,
        folder_id: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let session = self
            .get_session(session_id)
            .await?
            .ok_or_else(|| row_not_found(format!("Session not found: {session_id}")))?;

        if session.archived {
            return Err(storage_error(format!(
                "cannot move archived session {session_id} into pinned folders"
            )));
        }

        if folder_id.is_some() && !session.pinned {
            return Err(storage_error(format!(
                "cannot move unpinned session {session_id} into a pinned folder"
            )));
        }

        // Moving a session out of a folder (while pinned) creates a new standalone
        // pinned item and therefore consumes profile pin capacity.
        if folder_id.is_none() && session.pinned && session.folder_id.is_some() {
            let profile_id = session.profile_id.as_deref().ok_or_else(|| {
                row_not_found(format!("Session {session_id} is not attached to a profile"))
            })?;
            self.ensure_pinned_capacity(profile_id).await?;
        }

        if let Some(target_folder_id) = folder_id {
            let folder = query!(
                self.db,
                "SELECT * FROM folders WHERE id = ?",
                target_folder_id
            )
            .fetch_optional::<folders::Model>()
            .await?
            .ok_or_else(|| row_not_found(format!("Folder not found: {target_folder_id}")))?;

            let session_profile = session.profile_id.as_deref().ok_or_else(|| {
                row_not_found(format!("Session {session_id} is not attached to a profile"))
            })?;
            if folder.profile_id != session_profile {
                return Err(row_not_found(format!(
                    "Folder {target_folder_id} does not belong to session profile {session_profile}"
                )));
            }
        }

        query!(
            self.db,
            "UPDATE sessions SET folder_id = ? WHERE id = ?",
            folder_id,
            session_id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Set a session's `profile_id`.
    pub async fn set_session_profile(
        &self,
        session_id: &str,
        profile_id: &str,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "UPDATE sessions SET profile_id = ? WHERE id = ?",
            profile_id,
            session_id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// List sessions for a specific profile, ordered by `updated_at` descending.
    pub async fn list_sessions_for_profile(
        &self,
        profile_id: &str,
    ) -> Result<Vec<sessions::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM sessions WHERE profile_id = ? ORDER BY updated_at DESC",
            profile_id
        )
        .fetch_all::<sessions::Model>()
        .await?)
    }

    /// Find non-pinned, non-archived sessions with `updated_at` before the cutoff.
    pub async fn sessions_needing_archive(
        &self,
        cutoff: &str,
    ) -> Result<Vec<sessions::Model>, ChatDbError> {
        let mut active_session_ids = query!(
            self.db,
            "SELECT session_id FROM session_runtimes WHERE state != ?",
            SessionRuntimeState::Idle.as_str()
        )
        .fetch_all::<SessionIdRow>()
        .await?
        .into_iter()
        .map(|row| row.session_id)
        .collect::<std::collections::BTreeSet<_>>();
        active_session_ids.extend(
            query!(
                self.db,
                "SELECT DISTINCT session_id FROM session_pending_message_entries"
            )
            .fetch_all::<SessionIdRow>()
            .await?
            .into_iter()
            .map(|row| row.session_id),
        );
        active_session_ids.extend(
            query!(
                self.db,
                "SELECT session_id FROM session_pending_permissions"
            )
            .fetch_all::<SessionIdRow>()
            .await?
            .into_iter()
            .map(|row| row.session_id),
        );
        active_session_ids.extend(
            query!(
                self.db,
                "SELECT DISTINCT session_id FROM session_pending_ask_user_requests_v2"
            )
            .fetch_all::<SessionIdRow>()
            .await?
            .into_iter()
            .map(|row| row.session_id),
        );
        active_session_ids.extend(
            query!(self.db, "SELECT session_id FROM session_pending_workspace")
                .fetch_all::<SessionIdRow>()
                .await?
                .into_iter()
                .map(|row| row.session_id),
        );

        let sessions = query!(self.db, "SELECT * FROM sessions WHERE pinned = FALSE AND archived = FALSE AND updated_at < ? ORDER BY updated_at ASC", cutoff)
            .fetch_all::<sessions::Model>()
            .await?;
        Ok(sessions
            .into_iter()
            .filter(|session| !active_session_ids.contains(&session.id))
            .collect())
    }

    /// Count pinned items (sessions + folders) in a profile.
    pub async fn pinned_count(&self, profile_id: &str) -> Result<i64, ChatDbError> {
        let session_count = query!(self.db, "SELECT COUNT(*) AS value FROM sessions WHERE profile_id = ? AND pinned = TRUE AND folder_id IS NULL", profile_id)
            .fetch_one::<I64ValueRow>()
            .await?
            .value;
        let folder_count = query!(
            self.db,
            "SELECT COUNT(*) AS value FROM folders WHERE profile_id = ?",
            profile_id
        )
        .fetch_one::<I64ValueRow>()
        .await?
        .value;

        Ok(session_count + folder_count)
    }

    async fn ensure_pinned_capacity(&self, profile_id: &str) -> Result<(), ChatDbError> {
        let profile = self
            .get_profile(profile_id)
            .await?
            .ok_or_else(|| row_not_found(format!("Profile not found: {profile_id}")))?;
        let current = self.pinned_count(profile_id).await?;
        let limit = i64::from(profile.pinned_limit);
        if current >= limit {
            return Err(ChatDbError::PinnedLimitExceeded {
                profile_id: profile_id.to_string(),
                limit,
                current,
            });
        }
        Ok(())
    }

    async fn max_pin_sort_order(&self, profile_id: &str) -> Result<i32, ChatDbError> {
        Ok(query!(self.db, "SELECT COALESCE(MAX(pin_sort_order), -1) AS value FROM sessions WHERE profile_id = ? AND pinned = TRUE", profile_id)
            .fetch_one::<I32ValueRow>()
            .await?
            .value)
    }
}
