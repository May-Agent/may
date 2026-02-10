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
}

impl SshSessionAuthorizer for MaySshAuthorizer {
    fn authorize_connect(
        &self,
        target: &str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>> {
        let target = target.to_string();
        Box::pin(async move {
            let args = AskUserArgs {
                question: Some(format!("Allow SSH connection to {}?", target)),
                option: vec!["Allow".to_string(), "Deny".to_string()],
                multi_select: false,
                questions: Vec::new(),
            };
            let response = self.broker.request(args).await.map_err(|e| e.to_string())?;
            Ok(response.iter().any(|a| matches!(&a.answer, AnswerValue::Selected(opts) if opts.iter().any(|o| o == "Allow"))))
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
            let args = AskUserArgs {
                question: Some(format!("{} Install leash on {}?", details, target)),
                option: vec!["Install".to_string(), "Skip".to_string()],
                multi_select: false,
                questions: Vec::new(),
            };
            let response = self.broker.request(args).await.map_err(|e| e.to_string())?;
            Ok(response.iter().any(|a| matches!(&a.answer, AnswerValue::Selected(opts) if opts.iter().any(|o| o == "Install"))))
        })
    }

    fn authorize_unsafe_fallback(
        &self,
        target: &str,
        reason: &str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>> {
        let target = target.to_string();
        let reason = reason.to_string();
        Box::pin(async move {
            let args = AskUserArgs {
                question: Some(format!(
                    "{} Allow unsafe (no sandbox) execution on {}?",
                    reason, target
                )),
                option: vec!["Allow unsafe".to_string(), "Deny".to_string()],
                multi_select: false,
                questions: Vec::new(),
            };
            let response = self.broker.request(args).await.map_err(|e| e.to_string())?;
            Ok(response.iter().any(|a| matches!(&a.answer, AnswerValue::Selected(opts) if opts.iter().any(|o| o == "Allow unsafe"))))
        })
    }
}
