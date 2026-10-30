use std::sync::Arc;

use aither_sandbox::{SshAuthorizationError, SshSessionAuthorizer};
use askama::Template;

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

#[derive(Template)]
#[template(path = "ssh_install_permission.txt", escape = "none")]
struct SshInstallPermission<'a> {
    target: &'a str,
    details: &'a str,
}

/// Authorizes `terminal` ssh-mode targets by asking the user through the
/// ask-user broker.
#[derive(Debug)]
pub struct MaySshAuthorizer {
    broker: AskUserBroker,
    session_id: String,
}

impl MaySshAuthorizer {
    /// Create an authorizer for `session_id` sharing `broker`.
    pub fn new(session_id: impl Into<String>, broker: AskUserBroker) -> Arc<Self> {
        Arc::new(Self {
            broker,
            session_id: session_id.into(),
        })
    }

    /// Ask a yes/no permission question via the `AskUser` UI.
    async fn ask_permission<T: Template + Send>(
        &self,
        question: T,
        allow_label: &str,
    ) -> Result<bool, SshAuthorizationError> {
        let question = question
            .render()
            .map_err(|error| SshAuthorizationError::Prompt(error.to_string()))?;
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
            .map_err(|error| SshAuthorizationError::Prompt(error.to_string()))?;
        Ok(choice.allows(&response))
    }
}

impl SshSessionAuthorizer for MaySshAuthorizer {
    async fn authorize_connect(&self, target: &str) -> Result<bool, SshAuthorizationError> {
        self.ask_permission(SshConnectPermission { target }, "Allow")
            .await
    }

    async fn authorize_runtime_install(
        &self,
        target: &str,
        details: &str,
    ) -> Result<bool, SshAuthorizationError> {
        self.ask_permission(SshInstallPermission { target, details }, "Install")
            .await
    }
}
