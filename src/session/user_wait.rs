//! What a session waits on the user for.
//!
//! Every popup that blocks on the user's answer announces itself as a
//! [`UserWait`] through [`SessionRuntimeService::announce_user_wait`]; the
//! daemon turns each announcement into a notification.
//!
//! [`SessionRuntimeService::announce_user_wait`]: super::SessionRuntimeService::announce_user_wait

use crate::agent::PermissionRequestMode;

use super::PersistedAskUserRequest;

/// One popup waiting on the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserWait {
    /// Session that waits.
    pub session_id: String,
    /// What the user is asked.
    pub ask: UserAsk,
}

/// What the user is asked in a popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAsk {
    /// Approval to run a command outside the sandbox, or over SSH.
    Command {
        /// Which approval the command needs.
        mode: PermissionRequestMode,
        /// The command.
        script: String,
    },
    /// Approval of an issue, pull request or comment the agent wants to
    /// publish.
    Publication {
        /// The first line of the publication: what it is and where.
        headline: String,
    },
    /// Access to a directory outside the workspace.
    Workspace {
        /// The requested directory.
        path: String,
    },
    /// A question: a prompt a run is blocked on or an open question.
    Question {
        /// The question text.
        text: String,
    },
}

impl UserAsk {
    /// The ask of a persisted question; it names the first question.
    #[must_use]
    pub fn question(request: &PersistedAskUserRequest) -> Self {
        Self::Question {
            text: request
                .questions
                .first()
                .map(|question| question.question.clone())
                .unwrap_or_default(),
        }
    }

    /// The ask of a persisted permission request.
    #[must_use]
    pub fn permission(mode: PermissionRequestMode, script: &str) -> Self {
        match mode {
            PermissionRequestMode::Publish => Self::Publication {
                headline: script.lines().next().unwrap_or_default().to_owned(),
            },
            PermissionRequestMode::Default
            | PermissionRequestMode::Unsafe
            | PermissionRequestMode::Ssh => Self::Command {
                mode,
                script: script.to_owned(),
            },
        }
    }
}
