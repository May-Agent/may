use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use askama::Template;

use super::permission::PermissionError;

use crate::PermissionHistoryMode;
use crate::ask_user::{
    AnswerValue, AskUserArgs, AskUserBroker, AskUserRequestPayload, QuestionAnswer,
};

/// The options a permission prompt offers.
///
/// Includes an allow-once option, an optional session-wide allow option, and a
/// deny option so the live gate and late-answer routing agree on approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionChoice<'a> {
    allow: &'a str,
    session_allow: bool,
}

impl<'a> PermissionChoice<'a> {
    const ALWAYS_ALLOW: &'static str = "Always allow for session";
    const DENY: &'static str = "Deny";

    /// The 3-way choice allowing once (`allow`), for the session, or denying.
    #[must_use]
    pub const fn with_session_allow(allow: &'a str) -> Self {
        Self {
            allow,
            session_allow: true,
        }
    }

    /// The choice that allows with the option labelled `allow`.
    #[must_use]
    pub const fn new(allow: &'a str) -> Self {
        Self {
            allow,
            session_allow: false,
        }
    }

    /// Recover the choice from the options of a prompt built by
    /// [`Self::options`].
    #[must_use]
    pub fn from_options(options: &'a [String]) -> Option<Self> {
        let allow = options.first()?;
        let session_allow = options.iter().any(|option| option == Self::ALWAYS_ALLOW);
        Some(Self {
            allow,
            session_allow,
        })
    }

    /// The options to show, allow first.
    #[must_use]
    pub fn options(&self) -> Vec<String> {
        if self.session_allow {
            vec![
                self.allow.to_string(),
                Self::ALWAYS_ALLOW.to_string(),
                Self::DENY.to_string(),
            ]
        } else {
            vec![self.allow.to_string(), Self::DENY.to_string()]
        }
    }

    /// Whether `answers` select an allow option (once or for session).
    /// Anything else, a typed reply included, is not approval.
    #[must_use]
    pub fn allows(&self, answers: &[QuestionAnswer]) -> bool {
        answers.iter().any(|answer| {
            matches!(
                &answer.answer,
                AnswerValue::Selected(options)
                    if options.iter().any(|option| option == self.allow
                        || (self.session_allow && option == Self::ALWAYS_ALLOW))
            )
        })
    }

    /// Whether `answers` select `"Always allow for session"`.
    #[must_use]
    pub fn allows_for_session(&self, answers: &[QuestionAnswer]) -> bool {
        self.session_allow
            && answers.iter().any(|answer| {
                matches!(
                    &answer.answer,
                    AnswerValue::Selected(options)
                        if options.iter().any(|option| option == Self::ALWAYS_ALLOW)
                )
            })
    }
}

#[derive(Template)]
#[template(path = "ssh_connect_permission.txt", escape = "none")]
struct SshConnectPermission<'a> {
    target: &'a str,
}

/// Authorizes the SSH servers `terminal` commands name by asking the user through the
/// ask-user broker and remembering session-wide grants per session.
#[derive(Debug)]
pub struct MaySshAuthorizer {
    broker: AskUserBroker,
    session_id: String,
    db: crate::db::ChatDb,
    session_grants: Mutex<HashSet<String>>,
    /// Held while a question waits on the user so a command's own timeout
    /// does not count the wait.
    approval_pause: crate::harness::approval_pause::ApprovalPause,
}

impl MaySshAuthorizer {
    /// Create an authorizer for `session_id` sharing `broker`.
    pub fn new(
        session_id: impl Into<String>,
        broker: AskUserBroker,
        db: crate::db::ChatDb,
    ) -> Arc<Self> {
        Self::new_with_pause(
            session_id,
            broker,
            db,
            crate::harness::approval_pause::ApprovalPause::new(),
        )
    }

    /// [`Self::new`] whose questions hold `pause` while the user decides.
    pub fn new_with_pause(
        session_id: impl Into<String>,
        broker: AskUserBroker,
        db: crate::db::ChatDb,
        approval_pause: crate::harness::approval_pause::ApprovalPause,
    ) -> Arc<Self> {
        Arc::new(Self {
            broker,
            db,
            session_id: session_id.into(),
            session_grants: Mutex::new(HashSet::new()),
            approval_pause,
        })
    }

    /// Whether [`Self::authorize_connect`] will wait for the user: a yolo
    /// session or a target already granted for this session answers at once.
    ///
    /// # Errors
    /// Returns [`PermissionError`] when the yolo switch cannot be read.
    pub async fn will_ask(&self) -> Result<bool, PermissionError> {
        Ok(!self
            .db
            .session_yolo(&self.session_id)
            .await
            .map_err(anyhow::Error::from)
            .map_err(PermissionError::Internal)?)
    }

    /// Ask whether commands may run on the server at `target`.
    ///
    /// # Errors
    /// Returns [`PermissionError`] when the question cannot be put, or when the
    /// session grant cannot be read or recorded.
    pub async fn authorize_connect(&self, target: &str) -> Result<bool, PermissionError> {
        self.ask_permission(SshConnectPermission { target }, "Approve", true)
            .await
    }

    fn has_memory_grant(&self, question: &str) -> bool {
        self.session_grants
            .lock()
            .expect("ssh session grants lock poisoned")
            .contains(question)
    }

    fn record_memory_grant(&self, question: String) {
        self.session_grants
            .lock()
            .expect("ssh session grants lock poisoned")
            .insert(question);
    }

    /// Ask a permission question via the `AskUser` UI, honoring prior
    /// session-wide grants when `session_allow` is enabled.
    async fn ask_permission<T: Template + Send>(
        &self,
        question: T,
        allow_label: &str,
        session_allow: bool,
    ) -> Result<bool, PermissionError> {
        let question = question
            .render()
            .map_err(anyhow::Error::from)
            .map_err(PermissionError::Internal)?;
        if session_allow {
            if self.has_memory_grant(&question) {
                return Ok(true);
            }
            if self
                .db
                .has_granted_permission(&self.session_id, PermissionHistoryMode::Ssh, &question)
                .await
                .map_err(anyhow::Error::from)
                .map_err(PermissionError::Internal)?
            {
                self.record_memory_grant(question);
                return Ok(true);
            }
        }
        // A yolo session grants at once and records it like a user approval.
        if !self.will_ask().await? {
            self.db
                .append_granted_permission(
                    &self.session_id,
                    PermissionHistoryMode::Ssh,
                    &question,
                    None,
                )
                .await
                .map_err(anyhow::Error::from)
                .map_err(PermissionError::Internal)?;
            if session_allow {
                self.record_memory_grant(question);
            }
            return Ok(true);
        }
        let choice = if session_allow {
            PermissionChoice::with_session_allow(allow_label)
        } else {
            PermissionChoice::new(allow_label)
        };
        let args = AskUserArgs {
            question: Some(question.clone()),
            option: choice.options(),
            multi_select: false,
            questions: Vec::new(),
        };
        // The question waits on the user session-wide: the call it came
        // through has no hold of its own to give.
        let _pause = self.approval_pause.hold_session();
        let response = self
            .broker
            .request(AskUserRequestPayload {
                session_id: self.session_id.clone(),
                request: args,
            })
            .await
            .map_err(PermissionError::Internal)?;
        if choice.allows_for_session(&response) {
            self.db
                .append_granted_permission(
                    &self.session_id,
                    PermissionHistoryMode::Ssh,
                    &question,
                    None,
                )
                .await
                .map_err(anyhow::Error::from)
                .map_err(PermissionError::Internal)?;
            self.record_memory_grant(question);
            return Ok(true);
        }
        Ok(choice.allows(&response))
    }
}
