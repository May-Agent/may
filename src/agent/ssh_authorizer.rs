use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aither_sandbox::SshSessionAuthorizer;

use crate::ask_user::{AnswerValue, AskUserArgs, AskUserBroker};

pub struct MaySshAuthorizer {
    broker: AskUserBroker,
}

impl MaySshAuthorizer {
    pub fn new(broker: AskUserBroker) -> Arc<Self> {
        Arc::new(Self { broker })
    }

    /// Ask a yes/no permission question via the AskUser UI.
    async fn ask_permission(&self, question: String, allow_label: &str) -> Result<bool, String> {
        let args = AskUserArgs {
            question: Some(question),
            option: vec![allow_label.to_string(), "Deny".to_string()],
            multi_select: false,
            questions: Vec::new(),
        };
        let response = self.broker.request(args).await.map_err(|e| e.to_string())?;
        Ok(response.iter().any(|a| {
            matches!(&a.answer, AnswerValue::Selected(opts) if opts.iter().any(|o| o == allow_label))
        }))
    }
}

impl SshSessionAuthorizer for MaySshAuthorizer {
    fn authorize_connect(
        &self,
        target: &str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>> {
        let target = target.to_string();
        Box::pin(async move {
            self.ask_permission(format!("Allow SSH connection to {}?", target), "Allow")
                .await
        })
    }

    fn authorize_leash_install(
        &self,
        target: &str,
        details: &str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>> {
        let target = target.to_string();
        let details = details.to_string();
        Box::pin(async move {
            self.ask_permission(
                format!("{} Install leash on {}?", details, target),
                "Install",
            )
            .await
        })
    }
}
