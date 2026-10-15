use std::sync::Arc;

use aither_sandbox::{SshAuthorizationError, SshSessionAuthorizer};
use askama::Template;

use crate::ask_user::{AnswerValue, AskUserArgs, AskUserBroker, AskUserRequestPayload};

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

pub struct MaySshAuthorizer {
    broker: AskUserBroker,
    session_id: String,
}

impl MaySshAuthorizer {
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
        let args = AskUserArgs {
            question: Some(question),
            option: vec![allow_label.to_string(), "Deny".to_string()],
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
        Ok(response.iter().any(|a| {
            matches!(&a.answer, AnswerValue::Selected(opts) if opts.iter().any(|o| o == allow_label))
        }))
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
