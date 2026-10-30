//! Open `ask_user` questions, and the atomic commit of a response.
//!
//! A question the agent asked without a run waiting on it is *open* until it
//! is answered or withdrawn. It lives in the same tables as a prompt a run
//! blocks on (`blocking = 1`), told apart by `blocking = 0`, so it outlives
//! the run that asked it and a restart.
//!
//! Every response is committed here, in one transaction, before anything is
//! woken: the pending request row goes, the response joins the history, and a
//! delivery to the agent (when there is no live waiter to wake) joins the
//! session queue. A failure rolls all of it back.

use crate::ask_user::{AnswerValue, Question, QuestionAnswer};
use crate::session::{PendingUserMessage, PersistedAskUserQuestion, PersistedAskUserRequest};

use super::common::{
    CommittedAnswerRow, CommittedDecisionRow, CommittedResponseLookup, CommittedSelectedOptionRow,
    PendingAskUserLookup, PendingAskUserQuestionOptionRow, PendingAskUserQuestionRow,
    PendingAskUserRequestRow, PendingPermissionLookup, PendingWorkspaceLookup, overflow_error,
};
use super::{ChatDb, ChatDbError, query};

/// A question still waiting for its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAskUserQuestion {
    /// Session the question belongs to.
    pub session_id: String,
    /// Whether a run is waiting on the prompt.
    pub blocking: bool,
    /// The question.
    pub request: PersistedAskUserRequest,
}

/// A response that was committed, and what it resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedAskUserAnswer {
    /// Session the question belonged to.
    pub session_id: String,
    /// Whether a run was waiting on the prompt.
    pub blocking: bool,
    /// The question that was answered.
    pub request: PersistedAskUserRequest,
    /// Whether the answer was queued for delivery to the agent.
    pub delivered: bool,
}

fn history_questions(questions: &[PersistedAskUserQuestion]) -> Vec<Question> {
    questions
        .iter()
        .map(|question| Question {
            section: question.section.clone(),
            question: question.question.clone(),
            option: question.option.clone(),
            multi_select: question.multi_select,
        })
        .collect()
}

impl ChatDb {
    /// Persist `request` as an open question of `session_id`, with its
    /// history entry, in one transaction.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn open_ask_user_question(
        &self,
        session_id: &str,
        request: &PersistedAskUserRequest,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "INSERT INTO session_pending_ask_user_requests_v2 (request_id, session_id, created_at, blocking) VALUES (?, ?, ?, 0)",
            request.id.as_str(),
            session_id,
            now.as_str()
        )
        .execute()
        .await?;
        for (question_ordinal, question) in request.questions.iter().enumerate() {
            let question_ordinal = i64::try_from(question_ordinal)
                .map_err(|error| overflow_error("open question ordinal", &error))?;
            query!(
                tx,
                "INSERT INTO session_pending_ask_user_questions_v2 (request_id, ordinal, section, question, multi_select) VALUES (?, ?, ?, ?, ?)",
                request.id.as_str(),
                question_ordinal,
                question.section.as_str(),
                question.question.as_str(),
                question.multi_select
            )
            .execute()
            .await?;
            for (option_ordinal, option) in question.option.iter().enumerate() {
                query!(
                    tx,
                    "INSERT INTO session_pending_ask_user_question_options_v2 (request_id, question_ordinal, option_ordinal, value) VALUES (?, ?, ?, ?)",
                    request.id.as_str(),
                    question_ordinal,
                    i64::try_from(option_ordinal)
                        .map_err(|error| overflow_error("open question option ordinal", &error))?,
                    option.as_str()
                )
                .execute()
                .await?;
            }
        }
        self.append_ask_user_request_message_in_tx(
            &mut tx,
            session_id,
            request.id.as_str(),
            &history_questions(&request.questions),
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Withdraw an open question of `session_id`. Returns whether it was
    /// still open; a question that was answered or never existed is left
    /// alone, and so is a prompt a run is waiting on.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn withdraw_ask_user_question(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<bool, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let withdrawn = query!(
            tx,
            "DELETE FROM session_pending_ask_user_requests_v2 WHERE request_id = ? AND session_id = ? AND blocking = 0 RETURNING session_id, blocking",
            request_id,
            session_id
        )
        .fetch_optional::<PendingAskUserLookup>()
        .await?
        .is_some();
        if withdrawn {
            query!(
                tx,
                "DELETE FROM session_pending_ask_user_question_options_v2 WHERE request_id = ?",
                request_id
            )
            .execute()
            .await?;
            query!(
                tx,
                "DELETE FROM session_pending_ask_user_questions_v2 WHERE request_id = ?",
                request_id
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        Ok(withdrawn)
    }

    /// The open questions of `session_id`, oldest first, read in one
    /// transaction so a concurrent answer never leaves a request without its
    /// questions.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn open_ask_user_questions(
        &self,
        session_id: &str,
    ) -> Result<Vec<PersistedAskUserRequest>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let requests = query!(
            tx,
            "SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND blocking = 0 ORDER BY created_at ASC, rowid ASC",
            session_id
        )
        .fetch_all::<PendingAskUserRequestRow>()
        .await?;
        let mut open = Vec::with_capacity(requests.len());
        for row in requests {
            open.push(Self::load_ask_user_request_in_tx(&mut tx, row).await?);
        }
        tx.commit().await?;
        Ok(open)
    }

    async fn load_ask_user_request_in_tx(
        tx: &mut skyzen_services::DbTransaction,
        row: PendingAskUserRequestRow,
    ) -> Result<PersistedAskUserRequest, ChatDbError> {
        let questions = query!(
            tx,
            "SELECT * FROM session_pending_ask_user_questions_v2 WHERE request_id = ? ORDER BY ordinal ASC",
            row.request_id.as_str()
        )
        .fetch_all::<PendingAskUserQuestionRow>()
        .await?;
        let options = query!(
            tx,
            "SELECT * FROM session_pending_ask_user_question_options_v2 WHERE request_id = ? ORDER BY question_ordinal ASC, option_ordinal ASC",
            row.request_id.as_str()
        )
        .fetch_all::<PendingAskUserQuestionOptionRow>()
        .await?;
        Ok(Self::persisted_ask_user_request_from_parts(
            row, questions, options,
        ))
    }

    /// The session of the question `request_id` once it was answered, or
    /// `None` when no answer was committed for it: withdrawn or never asked.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn answered_ask_user_session(
        &self,
        request_id: &str,
    ) -> Result<Option<String>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT m.session_id AS session_id FROM message_ask_user_responses r JOIN messages m ON m.id = r.message_id WHERE r.request_id = ? LIMIT 1",
            request_id
        )
        .fetch_optional::<PendingPermissionLookup>()
        .await?
        .map(|lookup| lookup.session_id))
    }

    /// The answers committed for the question `request_id`, read back from
    /// the history, or `None` when no answer was committed for it.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or a stored answer of an
    /// unknown kind.
    pub async fn committed_ask_user_answers(
        &self,
        request_id: &str,
    ) -> Result<Option<Vec<QuestionAnswer>>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let Some(response) = query!(
            tx,
            "SELECT message_id FROM message_ask_user_responses WHERE request_id = ? LIMIT 1",
            request_id
        )
        .fetch_optional::<CommittedResponseLookup>()
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let rows = query!(
            tx,
            "SELECT ordinal, section, answer_kind, custom_text FROM message_ask_user_response_answers WHERE message_id = ? ORDER BY ordinal ASC",
            response.message_id
        )
        .fetch_all::<CommittedAnswerRow>()
        .await?;
        let selected = query!(
            tx,
            "SELECT answer_ordinal, value FROM message_ask_user_response_selected_options WHERE message_id = ? ORDER BY answer_ordinal ASC, option_ordinal ASC",
            response.message_id
        )
        .fetch_all::<CommittedSelectedOptionRow>()
        .await?;
        tx.commit().await?;
        let answers = rows
            .into_iter()
            .map(|row| {
                let answer = match (row.answer_kind.as_str(), row.custom_text) {
                    ("selected", None) => AnswerValue::Selected(
                        selected
                            .iter()
                            .filter(|option| option.answer_ordinal == row.ordinal)
                            .map(|option| option.value.clone())
                            .collect(),
                    ),
                    ("custom", Some(text)) => AnswerValue::Custom(text),
                    (kind, _) => {
                        return Err(super::common::storage_error(format!(
                            "committed ask-user answer {request_id} has unreadable kind {kind}"
                        )));
                    }
                };
                Ok(QuestionAnswer {
                    section: row.section,
                    answer,
                })
            })
            .collect::<Result<Vec<_>, ChatDbError>>()?;
        Ok(Some(answers))
    }

    /// The decision committed for the workspace request `request_id`, read
    /// back from the history, or `None` when none was committed for it.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn committed_workspace_decision(
        &self,
        request_id: &str,
    ) -> Result<Option<bool>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT approved FROM message_workspace_responses WHERE request_id = ? LIMIT 1",
            request_id
        )
        .fetch_optional::<CommittedDecisionRow>()
        .await?
        .map(|row| row.approved))
    }

    /// The pending question `request_id` and the session it belongs to, or
    /// `None` when it is not pending: answered, withdrawn or never asked.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn pending_ask_user_request(
        &self,
        request_id: &str,
    ) -> Result<Option<PendingAskUserQuestion>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let Some(lookup) = query!(
            tx,
            "SELECT session_id, blocking FROM session_pending_ask_user_requests_v2 WHERE request_id = ? LIMIT 1",
            request_id
        )
        .fetch_optional::<PendingAskUserLookup>()
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let request = Self::load_ask_user_request_in_tx(
            &mut tx,
            PendingAskUserRequestRow {
                request_id: request_id.to_string(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(Some(PendingAskUserQuestion {
            session_id: lookup.session_id,
            blocking: lookup.blocking,
            request,
        }))
    }

    /// Commit the answer to `request_id`: the request leaves the pending
    /// set, the answer joins the history and, when `delivery` builds one, the
    /// message carrying it joins the session queue. All of it commits
    /// together or not at all.
    ///
    /// Returns `None`, changing nothing, when the question is not pending:
    /// answered, withdrawn or never asked.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn commit_ask_user_answer(
        &self,
        request_id: &str,
        answers: &[QuestionAnswer],
        delivery: Option<impl FnOnce(&PersistedAskUserRequest) -> PendingUserMessage>,
    ) -> Result<Option<CommittedAskUserAnswer>, ChatDbError> {
        // The transaction opens with the delete so it takes the write lock
        // up front rather than upgrading a read lock.
        let mut tx = self.db.begin().await?;
        let Some(pending) = query!(
            tx,
            "DELETE FROM session_pending_ask_user_requests_v2 WHERE request_id = ? RETURNING session_id, blocking",
            request_id
        )
        .fetch_optional::<PendingAskUserLookup>()
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let questions = query!(
            tx,
            "SELECT * FROM session_pending_ask_user_questions_v2 WHERE request_id = ? ORDER BY ordinal ASC",
            request_id
        )
        .fetch_all::<PendingAskUserQuestionRow>()
        .await?;
        let options = query!(
            tx,
            "SELECT * FROM session_pending_ask_user_question_options_v2 WHERE request_id = ? ORDER BY question_ordinal ASC, option_ordinal ASC",
            request_id
        )
        .fetch_all::<PendingAskUserQuestionOptionRow>()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_ask_user_question_options_v2 WHERE request_id = ?",
            request_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_ask_user_questions_v2 WHERE request_id = ?",
            request_id
        )
        .execute()
        .await?;
        let request = Self::persisted_ask_user_request_from_parts(
            PendingAskUserRequestRow {
                request_id: request_id.to_string(),
            },
            questions,
            options,
        );
        self.append_ask_user_response_message_in_tx(
            &mut tx,
            pending.session_id.as_str(),
            request_id,
            answers,
            None,
        )
        .await?;
        let delivered = if let Some(delivery) = delivery {
            let message = delivery(&request);
            self.insert_pending_message_in_tx(&mut tx, pending.session_id.as_str(), &message)
                .await?;
            true
        } else {
            false
        };
        tx.commit().await?;
        Ok(Some(CommittedAskUserAnswer {
            session_id: pending.session_id,
            blocking: pending.blocking,
            request,
            delivered,
        }))
    }

    /// Delete every pending prompt and open question of `session_id`, and
    /// with `None` of every session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub(super) async fn delete_session_ask_user_requests(
        &self,
        session_id: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let mut tx = self.db.begin().await?;
        if let Some(session_id) = session_id {
            for sql in [
                "DELETE FROM session_pending_ask_user_question_options_v2 WHERE request_id IN (SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE session_id = ?)",
                "DELETE FROM session_pending_ask_user_questions_v2 WHERE request_id IN (SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE session_id = ?)",
                "DELETE FROM session_pending_ask_user_requests_v2 WHERE session_id = ?",
            ] {
                query!(tx, sql, session_id).execute().await?;
            }
        } else {
            for sql in [
                "DELETE FROM session_pending_ask_user_question_options_v2",
                "DELETE FROM session_pending_ask_user_questions_v2",
                "DELETE FROM session_pending_ask_user_requests_v2",
            ] {
                query!(tx, sql).execute().await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Clear the pending prompt `request_id` by its id, leaving any newer
    /// prompt of the session in place.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn clear_waiting_ask_user_request(
        &self,
        request_id: &str,
    ) -> Result<(), ChatDbError> {
        let mut tx = self.db.begin().await?;
        for sql in [
            "DELETE FROM session_pending_ask_user_question_options_v2 WHERE request_id = ?",
            "DELETE FROM session_pending_ask_user_questions_v2 WHERE request_id = ?",
            "DELETE FROM session_pending_ask_user_requests_v2 WHERE request_id = ?",
        ] {
            query!(tx, sql, request_id).execute().await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Commit a permission decision: the pending request `request_id` is
    /// cleared by its id and the decision joins the history, together.
    /// Returns the request's session, or `None` when it is not pending.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn commit_permission_response(
        &self,
        request_id: &str,
        approved: bool,
    ) -> Result<Option<String>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let Some(pending) = query!(
            tx,
            "DELETE FROM session_pending_permissions WHERE request_id = ? RETURNING session_id",
            request_id
        )
        .fetch_optional::<PendingPermissionLookup>()
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        self.append_permission_response_message_in_tx(
            &mut tx,
            pending.session_id.as_str(),
            request_id,
            approved,
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(Some(pending.session_id))
    }

    /// Commit a workspace decision: the pending request `request_id` is
    /// cleared by its id and the decision joins the history, together.
    /// Returns the request's session, or `None` when it is not pending.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn commit_workspace_response(
        &self,
        request_id: &str,
        approved: bool,
    ) -> Result<Option<String>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let Some(pending) = query!(
            tx,
            "DELETE FROM session_pending_workspace WHERE request_id = ? RETURNING session_id",
            request_id
        )
        .fetch_optional::<PendingWorkspaceLookup>()
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        self.append_workspace_response_message_in_tx(
            &mut tx,
            pending.session_id.as_str(),
            request_id,
            approved,
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(Some(pending.session_id))
    }
}
