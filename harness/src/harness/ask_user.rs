//! The questions an agent puts to the user and the channel they travel on.

use serde::{Deserialize, Serialize};

use super::tool_request::{
    ToolRequest, ToolRequestBroker, ToolRequestQueue, channel as tool_request_channel,
};

pub use may_protocol::ask_user::{AnswerValue, AskUserArgs, Question, QuestionAnswer};

/// A request from the agent to ask the user questions.
pub type AskUserRequest = ToolRequest<AskUserRequestPayload, Vec<QuestionAnswer>>;

/// Internal request payload sent from a session-bound command to the host runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskUserRequestPayload {
    /// Session issuing the request.
    pub session_id: String,
    /// User-visible request body.
    pub request: AskUserArgs,
}

/// Broker for `ask_user` requests.
pub type AskUserBroker = ToolRequestBroker<AskUserRequestPayload, Vec<QuestionAnswer>>;

/// Queue for pending `ask_user` requests.
pub type AskUserQueue = ToolRequestQueue<AskUserRequestPayload, Vec<QuestionAnswer>>;

/// Create a new `ask_user` channel pair.
#[must_use]
pub fn channel() -> (AskUserBroker, AskUserQueue) {
    tool_request_channel()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deser_option_array() {
        let json = serde_json::json!({
            "question": "请选择你喜欢的水果？",
            "option": ["苹果", "香蕉", "橘子", "梨"]
        });
        let args: AskUserArgs = serde_json::from_value(json).unwrap();
        assert_eq!(args.option.len(), 4);
        assert_eq!(args.option, vec!["苹果", "香蕉", "橘子", "梨"]);
        let questions = args.into_questions();
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].option.len(), 4);
    }

    #[test]
    fn test_schema_option_is_array() {
        let schema = schemars::schema_for!(AskUserArgs);
        let val = serde_json::to_value(schema).unwrap();
        let props = val.get("properties").unwrap().as_object().unwrap();
        let opt = props.get("option").unwrap();
        let opt_type = opt.get("type").and_then(|t| t.as_str());
        assert_eq!(
            opt_type,
            Some("array"),
            "option must be array type in schema"
        );
    }

    #[test]
    fn test_cli_to_json_with_real_schema() {
        let schema = schemars::schema_for!(AskUserArgs);
        let schema_val = serde_json::to_value(schema).unwrap();

        let result = crate::ipc::cli_to_json(
            &schema_val,
            &[
                "--question".into(),
                "请选择你喜欢的水果？".into(),
                "--option".into(),
                "苹果".into(),
                "--option".into(),
                "香蕉".into(),
                "--option".into(),
                "橘子".into(),
                "--option".into(),
                "梨".into(),
            ],
        )
        .unwrap();

        let args: AskUserArgs = serde_json::from_value(result).unwrap();
        assert_eq!(
            args.option.len(),
            4,
            "expected 4 options, got: {:?}",
            args.option
        );
        assert_eq!(args.option, vec!["苹果", "香蕉", "橘子", "梨"]);
    }
}
