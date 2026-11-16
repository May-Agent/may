//! The codespace each cloud session lives on, as the daemon fronting it
//! records it: the session's home environment is that machine, and this
//! daemon relays the session's API to the machine's daemon.

use super::{ChatDb, ChatDbError, query};

/// One `cloud_sessions` row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct CloudSessionRow {
    /// The session.
    pub session_id: String,
    /// The GitHub account the codespace belongs to.
    pub account: String,
    /// The codespace's name.
    pub machine_id: String,
    /// The machine type it was created on.
    pub machine_type: String,
    /// The bearer token the machine's daemon admits this daemon by.
    pub token: String,
    /// The repository cloned into the codespace, when it was created from
    /// the environment repository for a project without an SSH server.
    pub clone_repository: Option<String>,
    /// The directory the session works in on the machine.
    pub workspace: String,
}

impl ChatDb {
    /// Records that `row.session_id` lives on `row.machine_id`.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn insert_cloud_session(&self, row: &CloudSessionRow) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO cloud_sessions (session_id, account, machine_id, machine_type, token, clone_repository, workspace, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            row.session_id.as_str(),
            row.account.as_str(),
            row.machine_id.as_str(),
            row.machine_type.as_str(),
            row.token.as_str(),
            row.clone_repository.as_deref(),
            row.workspace.as_str(),
            now.as_str()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// The machine `session_id` lives on, or `None` for a session whose
    /// home environment is this machine.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn cloud_session(
        &self,
        session_id: &str,
    ) -> Result<Option<CloudSessionRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT session_id, account, machine_id, machine_type, token, clone_repository, workspace FROM cloud_sessions WHERE session_id = ?",
            session_id
        )
        .fetch_optional::<CloudSessionRow>()
        .await?)
    }

    /// Every cloud session.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn list_cloud_sessions(&self) -> Result<Vec<CloudSessionRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT session_id, account, machine_id, machine_type, token, clone_repository, workspace FROM cloud_sessions ORDER BY created_at"
        )
        .fetch_all::<CloudSessionRow>()
        .await?)
    }

    /// Forgets `session_id`'s machine.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn delete_cloud_session(&self, session_id: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "DELETE FROM cloud_sessions WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_cloud_session_is_recorded_read_and_forgotten() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("database");
        let row = CloudSessionRow {
            session_id: "quiet-otter".into(),
            account: "octocat".into(),
            machine_id: "monalisa-octocat-hello-world-g4wpq6h95q".into(),
            machine_type: "standardLinux".into(),
            token: "token".into(),
            clone_repository: Some("octocat/Hello-World".into()),
            workspace: "/workspaces/Hello-World".into(),
        };

        db.insert_cloud_session(&row).await.expect("recorded");

        assert_eq!(
            db.cloud_session("quiet-otter").await.expect("read"),
            Some(row.clone())
        );
        assert_eq!(db.cloud_session("local-one").await.expect("read"), None);
        assert_eq!(db.list_cloud_sessions().await.expect("listed"), [row]);
        db.delete_cloud_session("quiet-otter")
            .await
            .expect("forgotten");
        assert_eq!(db.cloud_session("quiet-otter").await.expect("read"), None);
    }
}
