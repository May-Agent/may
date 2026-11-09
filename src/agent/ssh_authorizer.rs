use std::sync::Arc;

use aither_sandbox::permission::PermissionError;
use askama::Template;

use crate::PermissionHistoryMode;
use crate::ask_user::{
    AnswerValue, AskUserArgs, AskUserBroker, AskUserRequestPayload, QuestionAnswer,
};

/// The choice a permission prompt offers: one allow option and a deny
/// option. The live gate and the routing of a late answer both decide
/// approval here, so they cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionChoice<'a> {
    allow: &'a str,
}

impl<'a> PermissionChoice<'a> {
    const DENY: &'static str = "Deny";

    /// The choice that allows with the option labelled `allow`.
    #[must_use]
    pub const fn new(allow: &'a str) -> Self {
        Self { allow }
    }

    /// Recover the choice from the options of a prompt built by
    /// [`Self::options`].
    #[must_use]
    pub fn from_options(options: &'a [String]) -> Option<Self> {
        options.first().map(|allow| Self::new(allow))
    }

    /// The options to show, allow first.
    #[must_use]
    pub fn options(&self) -> Vec<String> {
        vec![self.allow.to_string(), Self::DENY.to_string()]
    }

    /// Whether `answers` select the allow option. Anything else, a typed
    /// reply included, is not approval.
    #[must_use]
    pub fn allows(&self, answers: &[QuestionAnswer]) -> bool {
        answers.iter().any(|answer| {
            matches!(&answer.answer, AnswerValue::Selected(options) if options.iter().any(|option| option == self.allow))
        })
    }
}

#[derive(Template)]
#[template(path = "ssh_connect_permission.txt", escape = "none")]
struct SshConnectPermission<'a> {
    target: &'a str,
}

/// Authorizes the SSH servers `terminal` commands name by asking the user through the
/// ask-user broker.
#[derive(Debug)]
pub struct MaySshAuthorizer {
    broker: AskUserBroker,
    session_id: String,
    db: crate::db::ChatDb,
}

impl MaySshAuthorizer {
    /// Create an authorizer for `session_id` sharing `broker`.
    pub fn new(
        session_id: impl Into<String>,
        broker: AskUserBroker,
        db: crate::db::ChatDb,
    ) -> Arc<Self> {
        Arc::new(Self {
            broker,
            db,
            session_id: session_id.into(),
        })
    }

    /// Whether [`Self::authorize_connect`] will wait for the user: a yolo
    /// session answers at once.
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
    /// Returns [`PermissionError`] when the question cannot be put or the
    /// yolo grant cannot be recorded.
    pub async fn authorize_connect(&self, target: &str) -> Result<bool, PermissionError> {
        self.ask_permission(SshConnectPermission { target }, "Allow")
            .await
    }

    /// Ask a yes/no permission question via the `AskUser` UI.
    async fn ask_permission<T: Template + Send>(
        &self,
        question: T,
        allow_label: &str,
    ) -> Result<bool, PermissionError> {
        let question = question
            .render()
            .map_err(anyhow::Error::from)
            .map_err(PermissionError::Internal)?;
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
            return Ok(true);
        }
        let choice = PermissionChoice::new(allow_label);
        let args = AskUserArgs {
            question: Some(question),
            option: choice.options(),
            multi_select: false,
            questions: Vec::new(),
        };
        let response = self
            .broker
            .request(AskUserRequestPayload {
                session_id: self.session_id.clone(),
                request: args,
            })
            .await
            .map_err(PermissionError::Internal)?;
        Ok(choice.allows(&response))
    }
}
