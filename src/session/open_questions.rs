//! Delivery of an answer to an `ask_user` question the agent is no longer
//! waiting on.
//!
//! The answer reaches the agent as a steer message through the session
//! queue: at the next turn boundary when a run is active, otherwise as the
//! start of a new run. It carries the request id, each question and the full
//! answer, so the agent can clear the matching `BLOCKING` marks.

use askama::Template;

use crate::ask_user::{AnswerValue, QuestionAnswer};
use crate::xml::XmlBlock;

use super::{PendingUserMessage, PendingUserMessageMode, PersistedAskUserRequest};

/// The answers do not answer the questions they claim to.
#[derive(Debug, thiserror::Error)]
pub enum AnswerMismatch {
    /// The counts differ.
    #[error("{answers} answers were given for {questions} questions")]
    Count {
        /// Number of questions asked.
        questions: usize,
        /// Number of answers given.
        answers: usize,
    },
    /// An answer chose nothing. A question is answered or stays open; there
    /// is no third state.
    #[error("the answer to question {ordinal} is empty")]
    Empty {
        /// Zero-based position of the empty answer.
        ordinal: usize,
    },
    /// An answer is the skipped marker, which May never produces.
    #[error("the answer to question {ordinal} is a skip, which is not an answer")]
    Skipped {
        /// Zero-based position of the skipped answer.
        ordinal: usize,
    },
}

struct DeliveredAnswer<'a> {
    question: &'a str,
    answer: String,
}

#[derive(Template)]
#[template(path = "reminder_ask_user_answer.txt", escape = "none")]
struct AnswerDelivery<'a> {
    request_id: &'a str,
    entries: Vec<DeliveredAnswer<'a>>,
}

/// Check that `answers` answer each question of `request`, in order, each
/// with a choice or a text.
///
/// # Errors
/// Returns [`AnswerMismatch`] when the counts differ or an answer is empty.
pub fn check_answers(
    request: &PersistedAskUserRequest,
    answers: &[QuestionAnswer],
) -> Result<(), AnswerMismatch> {
    if request.questions.len() != answers.len() {
        return Err(AnswerMismatch::Count {
            questions: request.questions.len(),
            answers: answers.len(),
        });
    }
    for (ordinal, answer) in answers.iter().enumerate() {
        match &answer.answer {
            AnswerValue::Selected(values)
                if values.is_empty() || values.iter().any(|value| value.trim().is_empty()) =>
            {
                return Err(AnswerMismatch::Empty { ordinal });
            }
            AnswerValue::Custom(text) if text.trim().is_empty() => {
                return Err(AnswerMismatch::Empty { ordinal });
            }
            AnswerValue::Skipped => return Err(AnswerMismatch::Skipped { ordinal }),
            AnswerValue::Selected(_) | AnswerValue::Custom(_) => {}
        }
    }
    Ok(())
}

/// The steer message that tells the agent `answers` to `request`.
///
/// # Panics
/// Panics if the answers were not checked with [`check_answers`], or the
/// delivery template does not render.
#[must_use]
pub fn answer_delivery(
    request: &PersistedAskUserRequest,
    answers: &[QuestionAnswer],
) -> PendingUserMessage {
    assert_eq!(
        request.questions.len(),
        answers.len(),
        "answers must be checked against the question before delivery"
    );
    let text = AnswerDelivery {
        request_id: request.id.as_str(),
        entries: request
            .questions
            .iter()
            .zip(answers)
            .map(|(question, answer)| DeliveredAnswer {
                question: question.question.as_str(),
                answer: answer.answer.to_string(),
            })
            .collect(),
    }
    .render()
    .expect("answer delivery template must render");
    PendingUserMessage {
        content: String::new(),
        attachments: Vec::new(),
        hidden_context_xml: Some(
            XmlBlock::parse(text).expect("answer delivery renders a non-empty block"),
        ),
        created_at: jiff::Timestamp::now().to_string(),
        delivery_mode: PendingUserMessageMode::Steer,
    }
}
