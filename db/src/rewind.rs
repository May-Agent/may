//! Atomic conversation rewind across UI history, model transcript/checkpoints,
//! pending queues, open interactions, and the one-shot world-state alert.

use aither_core::llm::Message as LlmMessage;
use may_protocol::session_messages::MessageRole;

use super::common::{
    I64ValueRow, StoredSessionContextSnapshotRow, StringValueRow, TranscriptEntryRow,
    overflow_error, storage_error, to_i32,
};
use super::context_snapshot::CONTEXT_SNAPSHOT_FORMAT;
use super::session_run::RunFence;
use super::{ChatDb, ChatDbError, query, truncate_preview};

/// Outcome of rewinding a session's persisted state to before a user prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindOutcome {
    /// Deleted `messages` rows.
    pub deleted_messages: u64,
    /// Transcript `seq` from which `session_transcript_entries` was truncated.
    pub cut_seq: i64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RawContextCheckpoint {
    handoff: Option<aither_agent::Handoff>,
    items: Vec<aither_agent::ContextItem>,
    #[serde(default)]
    next_seq: u64,
}

/// Truncates `checkpoint` to items strictly below `cut_seq`, returning `None`
/// when nothing remains or when compaction already folded `cut_seq` into its
/// handoff summary.
fn truncate_checkpoint(
    mut checkpoint: aither_agent::AgentCheckpoint,
    cut_seq: i64,
) -> Result<Option<(aither_agent::AgentCheckpoint, i64)>, ChatDbError> {
    if cut_seq <= 0 && checkpoint.context.handoff().is_none() {
        return Ok(None);
    }
    let cut_u64 = u64::try_from(cut_seq.max(0)).unwrap_or(0);
    let fork_point = checkpoint.context.fork_point();
    if checkpoint.context.handoff().is_some() && cut_u64 < fork_point {
        let post_handoff_cut = checkpoint.context.items().iter().any(|item| {
            item.seq() == cut_u64
                && matches!(
                    item.message(),
                    LlmMessage::User { .. } | LlmMessage::System { .. }
                )
        });
        if !post_handoff_cut {
            return Ok(None);
        }
    }

    let raw_bytes = rmp_serde::to_vec_named(&checkpoint.context)
        .map_err(|error| storage_error(format!("failed to encode context checkpoint: {error}")))?;
    let mut raw: RawContextCheckpoint = rmp_serde::from_slice(&raw_bytes)
        .map_err(|error| storage_error(format!("failed to decode context checkpoint: {error}")))?;
    raw.items.retain(|item| item.seq() < cut_u64);
    if raw.handoff.is_none() && raw.items.is_empty() {
        return Ok(None);
    }
    raw.next_seq = cut_u64;
    let rebuilt_bytes = rmp_serde::to_vec_named(&raw)
        .map_err(|error| storage_error(format!("failed to encode trimmed context: {error}")))?;
    let trimmed_context: aither_agent::ContextCheckpoint = rmp_serde::from_slice(&rebuilt_bytes)
        .map_err(|error| storage_error(format!("failed to decode trimmed context: {error}")))?;

    checkpoint.context_window.messages = trimmed_context
        .items()
        .iter()
        .map(|item| item.message().clone())
        .collect();
    checkpoint.context_window.metrics.recent_message_count = trimmed_context.items().len();
    checkpoint
        .context_window
        .metrics
        .recent_system_message_count = trimmed_context
        .items()
        .iter()
        .filter(|item| matches!(item.message(), LlmMessage::System { .. }))
        .count();
    checkpoint.context_window.metrics.has_handoff = trimmed_context.handoff().is_some();
    checkpoint.context = trimmed_context;

    let applied_seq = cut_seq - 1;
    Ok(Some((checkpoint, applied_seq)))
}

impl ChatDb {
    /// Records the transcript `seq` boundary at which the prompt `message_id`
    /// begins, under the active run `fence`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on lease loss or storage failure.
    pub async fn record_prompt_watermark(
        &self,
        fence: &RunFence,
        message_id: i64,
        transcript_seq: i64,
    ) -> Result<(), ChatDbError> {
        let mut tx = self.begin_fenced(fence).await?;
        query!(
            tx,
            "INSERT OR REPLACE INTO session_prompt_watermarks (session_id, message_id, transcript_seq) VALUES (?, ?, ?)",
            fence.session_id.as_str(),
            message_id,
            transcript_seq
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically consumes the one-shot rewind alert `session_id` is owed;
    /// returns whether one was pending.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn take_rewind_alert(&self, session_id: &str) -> Result<bool, ChatDbError> {
        let result = query!(
            self.db,
            "DELETE FROM session_rewind_alerts WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        Ok(result.rows_written > 0)
    }

    /// Arms or restores the one-shot rewind alert for `session_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn restore_rewind_alert(&self, session_id: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "INSERT OR REPLACE INTO session_rewind_alerts (session_id, created_at) VALUES (?, ?)",
            session_id,
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Rewinds `fence.session_id` to before `cut_id` (whose user prompt row is
    /// `user_message_id`) in one fenced transaction: truncates `messages` and
    /// its satellites, `message_tool_cards`, `session_transcript_entries`, and
    /// `session_context_snapshots`, drops pending messages and open questions
    /// from the cut turns, and arms `session_rewind_alerts`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on lease loss or storage failure.
    pub async fn rewind_session_from(
        &self,
        fence: &RunFence,
        cut_id: i64,
        user_message_id: i64,
    ) -> Result<RewindOutcome, ChatDbError> {
        const SATELLITE_TABLES: &[&str] = &[
            "message_permission_requests",
            "message_permission_responses",
            "message_workspace_requests",
            "message_workspace_responses",
            "message_ask_user_requests",
            "message_ask_user_request_questions",
            "message_ask_user_request_options",
            "message_ask_user_responses",
            "message_ask_user_response_answers",
            "message_ask_user_response_selected_options",
        ];

        let session_id = fence.session_id.as_str();
        let mut tx = self.begin_fenced(fence).await?;

        let watermark = query!(
            tx,
            "SELECT transcript_seq AS value FROM session_prompt_watermarks WHERE session_id = ? AND message_id = ?",
            session_id,
            user_message_id
        )
        .fetch_optional::<I64ValueRow>()
        .await?
        .map(|row| row.value);

        let cut_seq = if let Some(seq) = watermark {
            seq
        } else {
            let preceding_users = usize::try_from(
                query!(
                    tx,
                    "SELECT COUNT(*) AS value FROM messages WHERE session_id = ? AND id < ? AND role = ?",
                    session_id,
                    cut_id,
                    MessageRole::User.as_str()
                )
                .fetch_one::<I64ValueRow>()
                .await?
                .value
                .max(0),
            )
            .map_err(|error| overflow_error("preceding_users", &error))?;

            let entries = query!(
                tx,
                "SELECT seq, message_blob, committed_at FROM session_transcript_entries WHERE session_id = ? ORDER BY seq ASC",
                session_id
            )
            .fetch_all::<TranscriptEntryRow>()
            .await?;

            let mut decoded = Vec::with_capacity(entries.len());
            for entry in entries {
                let message =
                    rmp_serde::from_slice::<LlmMessage>(&entry.message_blob).map_err(|error| {
                        storage_error(format!(
                            "failed to decode transcript entry {}: {error}",
                            entry.seq
                        ))
                    })?;
                decoded.push((entry.seq, message));
            }

            let mut seen_users = 0usize;
            let mut found_idx = None;
            for (idx, (_, message)) in decoded.iter().enumerate() {
                if matches!(message, LlmMessage::User { .. }) {
                    if seen_users == preceding_users {
                        found_idx = Some(idx);
                        break;
                    }
                    seen_users += 1;
                }
            }

            found_idx.map_or_else(
                || decoded.last().map_or(0, |(seq, _)| seq.saturating_add(1)),
                |idx| {
                    if cut_id < user_message_id
                        && idx > 0
                        && matches!(decoded[idx - 1].1, LlmMessage::System { .. })
                    {
                        decoded[idx - 1].0
                    } else {
                        decoded[idx].0
                    }
                },
            )
        };

        let released_words: Vec<String> = query!(
            tx,
            "SELECT word AS value FROM message_ask_user_requests WHERE word IS NOT NULL AND message_id IN (SELECT id FROM messages WHERE session_id = ? AND id >= ?)",
            session_id,
            cut_id
        )
        .fetch_all::<StringValueRow>()
        .await?
        .into_iter()
        .map(|row| row.value)
        .collect();

        query!(
            tx,
            "DELETE FROM session_pending_ask_user_question_options_v2 WHERE request_id IN (SELECT request_id FROM message_ask_user_requests WHERE message_id IN (SELECT id FROM messages WHERE session_id = ? AND id >= ?)) OR request_id IN (SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND blocking = 1)",
            session_id,
            cut_id,
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_ask_user_questions_v2 WHERE request_id IN (SELECT request_id FROM message_ask_user_requests WHERE message_id IN (SELECT id FROM messages WHERE session_id = ? AND id >= ?)) OR request_id IN (SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND blocking = 1)",
            session_id,
            cut_id,
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_ask_user_requests_v2 WHERE request_id IN (SELECT request_id FROM message_ask_user_requests WHERE message_id IN (SELECT id FROM messages WHERE session_id = ? AND id >= ?)) OR (session_id = ? AND blocking = 1)",
            session_id,
            cut_id,
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_permissions WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_workspace WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;

        query!(
            tx,
            "DELETE FROM message_tool_cards WHERE session_id = ? AND call_id IN (SELECT tool_call_id FROM messages WHERE session_id = ? AND id >= ? AND tool_call_id IS NOT NULL)",
            session_id,
            session_id,
            cut_id
        )
        .execute()
        .await?;

        for table in SATELLITE_TABLES {
            let sql = format!(
                "DELETE FROM {table} WHERE message_id IN (SELECT id FROM messages WHERE session_id = ? AND id >= ?)"
            );
            query!(tx, sql.as_str(), session_id, cut_id)
                .execute()
                .await?;
        }
        let deleted_messages = query!(
            tx,
            "DELETE FROM messages WHERE session_id = ? AND id >= ?",
            session_id,
            cut_id
        )
        .execute()
        .await?
        .rows_written;
        query!(
            tx,
            "DELETE FROM session_prompt_watermarks WHERE session_id = ? AND message_id >= ?",
            session_id,
            cut_id
        )
        .execute()
        .await?;

        query!(
            tx,
            "DELETE FROM session_transcript_entries WHERE session_id = ? AND seq >= ?",
            session_id,
            cut_seq
        )
        .execute()
        .await?;

        if let Some(snapshot_row) = query!(
            tx,
            "SELECT format_version, checkpoint_blob, applied_seq, updated_at FROM session_context_snapshots WHERE session_id = ?",
            session_id
        )
        .fetch_optional::<StoredSessionContextSnapshotRow>()
        .await?
        {
            if snapshot_row.format_version == CONTEXT_SNAPSHOT_FORMAT {
                let applied_seq = snapshot_row.applied_seq.unwrap_or(0);
                if applied_seq >= cut_seq {
                    let checkpoint = rmp_serde::from_slice::<aither_agent::AgentCheckpoint>(
                        &snapshot_row.checkpoint_blob,
                    )
                    .map_err(|error| {
                        storage_error(format!(
                            "failed to decode checkpoint blob for session {session_id}: {error}"
                        ))
                    })?;
                    match truncate_checkpoint(checkpoint, cut_seq)? {
                        Some((trimmed, new_applied_seq)) => {
                            let blob = rmp_serde::to_vec_named(&trimmed).map_err(|error| {
                                storage_error(format!("failed to encode checkpoint blob: {error}"))
                            })?;
                            query!(
                                tx,
                                "UPDATE session_context_snapshots SET checkpoint_blob = ?, applied_seq = ?, updated_at = ? WHERE session_id = ?",
                                blob,
                                new_applied_seq,
                                jiff::Timestamp::now().to_string(),
                                session_id
                            )
                            .execute()
                            .await?;
                        }
                        None => {
                            query!(
                                tx,
                                "DELETE FROM session_context_snapshots WHERE session_id = ?",
                                session_id
                            )
                            .execute()
                            .await?;
                        }
                    }
                }
            } else {
                query!(
                    tx,
                    "DELETE FROM session_context_snapshots WHERE session_id = ?",
                    session_id
                )
                .execute()
                .await?;
            }
        }

        query!(
            tx,
            "DELETE FROM session_pending_message_attachments WHERE message_id IN (SELECT id FROM session_pending_message_entries WHERE session_id = ?)",
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "DELETE FROM session_pending_message_entries WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;

        let now = jiff::Timestamp::now().to_string();
        query!(
            tx,
            "INSERT OR REPLACE INTO session_rewind_alerts (session_id, created_at) VALUES (?, ?)",
            session_id,
            now.as_str()
        )
        .execute()
        .await?;

        let preview_row = query!(
            tx,
            "SELECT content AS value FROM messages WHERE session_id = ? AND role IN (?, ?, ?) ORDER BY id DESC LIMIT 1",
            session_id,
            MessageRole::User.as_str(),
            MessageRole::Assistant.as_str(),
            MessageRole::System.as_str()
        )
        .fetch_optional::<super::common::StringValueRow>()
        .await?;
        let count_row = query!(
            tx,
            "SELECT COUNT(*) AS value FROM messages WHERE session_id = ? AND role IN (?, ?, ?)",
            session_id,
            MessageRole::User.as_str(),
            MessageRole::Assistant.as_str(),
            MessageRole::System.as_str()
        )
        .fetch_one::<I64ValueRow>()
        .await?;
        query!(
            tx,
            "UPDATE sessions SET message_count = ?, preview = ?, updated_at = ? WHERE id = ?",
            to_i32(
                u32::try_from(count_row.value)
                    .map_err(|error| overflow_error("message_count", &error))?,
                "message_count",
            )?,
            preview_row.map(|row| truncate_preview(&row.value)),
            now.as_str(),
            session_id
        )
        .execute()
        .await?;

        tx.commit().await?;

        if !released_words.is_empty()
            && let Ok(pool) = self.session_ids(session_id).await
        {
            for word in released_words {
                pool.release(&word);
            }
        }

        Ok(RewindOutcome {
            deleted_messages,
            cut_seq,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use aither_agent::{
        AgentCheckpoint, Context, ContextWindowMetrics, ContextWindowPhase, ContextWindowSnapshot,
        Handoff,
    };
    use aither_core::llm::Message as LlmMessage;
    use may_protocol::command_reading::Card;
    use may_protocol::session_messages::ChatMessage;
    use may_protocol::session_runtime::{
        PendingMessageState, PendingUserMessage, PendingUserMessageMode, PersistedAskUserQuestion,
        PersistedAskUserRequest,
    };
    use tempfile::tempdir;

    use crate::{ChatDb, RunFence, SessionContextSnapshotUpsert, TranscriptEntry};

    fn checkpoint_of(context: &Context) -> AgentCheckpoint {
        let cp = context.checkpoint();
        AgentCheckpoint {
            tool_surface_hash: "tool-surface".to_string(),
            context_window: ContextWindowSnapshot {
                phase: ContextWindowPhase::Stable,
                metrics: ContextWindowMetrics {
                    usage_fraction: 0.25,
                    context_window: 200_000,
                    estimated_tokens: 0,
                    chars_per_token: 4.0,
                    system_block_count: 1,
                    recent_message_count: cp.items().len(),
                    recent_system_message_count: 0,
                    has_handoff: cp.handoff().is_some(),
                    has_staged_system_updates: false,
                    prompt_cache_epoch: 0,
                },
                messages: cp.items().iter().map(|i| i.message().clone()).collect(),
            },
            context: cp,
        }
    }

    fn transcript_entry(seq: i64, message: &LlmMessage) -> TranscriptEntry {
        TranscriptEntry {
            seq,
            message_blob: rmp_serde::to_vec_named(message).expect("encode message"),
            committed_at: jiff::Timestamp::now().to_string(),
        }
    }

    fn sample_question(id: &str, text: &str) -> PersistedAskUserRequest {
        PersistedAskUserRequest {
            id: id.to_string(),
            questions: vec![PersistedAskUserQuestion {
                section: "Scope".to_string(),
                question: text.to_string(),
                option: vec!["Yes".to_string(), "No".to_string()],
                multi_select: false,
                secret: None,
            }],
        }
    }

    #[tokio::test]
    async fn rewind_truncates_messages_transcript_cards_queue_questions_and_arms_alert() {
        let dir = tempdir().expect("tempdir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open db");
        let session_id = "rewind-atomic";
        db.create_session(session_id, Some("Atomic"), None)
            .await
            .expect("create session");
        let fence = RunFence {
            session_id: session_id.to_string(),
            run_id: "run-1".to_string(),
        };
        assert!(db.claim_session_run(&fence).await.expect("claim"));

        let at = jiff::Timestamp::now();
        let u1 = db
            .append_chat_message_as_run(&fence, &ChatMessage::user("first"))
            .await
            .expect("u1");
        db.record_prompt_watermark(&fence, i64::from(u1.id), 0)
            .await
            .expect("w1");
        let _t1 = db
            .append_chat_message_as_run(&fence, &ChatMessage::tool("call-1", "{}", at))
            .await
            .expect("t1");
        db.set_tool_card(
            session_id,
            "call-1",
            &Card::Tasks {
                changes: Vec::new(),
            },
        )
        .await
        .expect("card 1");
        let kept_word = db
            .open_ask_user_question(session_id, &sample_question("req-1", "keep?"))
            .await
            .expect("open q1");
        let _a1 = db
            .append_chat_message_as_run(&fence, &ChatMessage::assistant("reply 1", at))
            .await
            .expect("a1");

        let hidden = db
            .append_chat_message_as_run(
                &fence,
                &ChatMessage::hidden_context("<system-reminder>cut</system-reminder>"),
            )
            .await
            .expect("hidden");
        let u2 = db
            .append_chat_message_as_run(&fence, &ChatMessage::user("第二步 次の手順 다음 단계"))
            .await
            .expect("u2");
        db.record_prompt_watermark(&fence, i64::from(u2.id), 2)
            .await
            .expect("w2");
        let _t2 = db
            .append_chat_message_as_run(&fence, &ChatMessage::tool("call-2", "{}", at))
            .await
            .expect("t2");
        db.set_tool_card(
            session_id,
            "call-2",
            &Card::Tasks {
                changes: Vec::new(),
            },
        )
        .await
        .expect("card 2");
        let cut_word = db
            .open_ask_user_question(session_id, &sample_question("req-2", "cut?"))
            .await
            .expect("open q2");
        let _a2 = db
            .append_chat_message_as_run(&fence, &ChatMessage::assistant("reply 2", at))
            .await
            .expect("a2");

        let mut ctx = Context::new();
        ctx.push(LlmMessage::user("first"));
        ctx.push(LlmMessage::assistant("reply 1"));
        ctx.push(LlmMessage::system("<system-reminder>cut</system-reminder>"));
        ctx.push(LlmMessage::user("第二步 次の手順 다음 단계"));
        ctx.push(LlmMessage::assistant("reply 2"));
        let entries = ctx
            .checkpoint()
            .items()
            .iter()
            .map(|item| transcript_entry(i64::try_from(item.seq()).expect("seq"), item.message()))
            .collect();
        db.upsert_session_context_snapshot(
            session_id,
            SessionContextSnapshotUpsert {
                checkpoint: checkpoint_of(&ctx),
                applied_seq: 4,
                transcript_entries: entries,
            },
        )
        .await
        .expect("upsert snapshot");

        db.push_queued_message(
            session_id,
            PendingUserMessage {
                id: None,
                state: PendingMessageState::Pending,
                error: None,
                content: "queued follow-up".to_string(),
                attachments: Vec::new(),
                hidden_context_xml: None,
                created_at: at.to_string(),
                delivery_mode: PendingUserMessageMode::Queue,
            },
        )
        .await
        .expect("enqueue");

        let outcome = db
            .rewind_session_from(&fence, i64::from(hidden.id), i64::from(u2.id))
            .await
            .expect("rewind");
        assert_eq!(outcome.cut_seq, 2);

        let remaining_msgs = db.get_messages(session_id).await.expect("messages");
        assert!(remaining_msgs.iter().all(|m| m.id < hidden.id));

        let cards = db.tool_cards(session_id).await.expect("cards");
        assert!(cards.contains_key("call-1"));
        assert!(!cards.contains_key("call-2"));

        let open_questions = db
            .named_open_ask_user_questions(session_id)
            .await
            .expect("open questions");
        assert_eq!(open_questions.len(), 1);
        assert_eq!(open_questions[0].word, kept_word);
        assert_ne!(kept_word, cut_word);

        let runtime = db
            .load_session_runtime_summary(session_id)
            .await
            .expect("runtime");
        assert_eq!(
            runtime.queued_messages,
            [] as [may_protocol::session_runtime::PendingUserMessage; 0]
        );
        assert_eq!(runtime.queued_count, 0);

        let snapshot = db
            .get_session_context_snapshot(session_id)
            .await
            .expect("snapshot")
            .expect("snapshot retained");
        assert_eq!(snapshot.applied_seq, 1);
        assert_eq!(snapshot.checkpoint.context.items().len(), 2);
        assert_eq!(snapshot.checkpoint.context.fork_point(), 2);
        assert!(
            db.transcript_entries_after(session_id, 1)
                .await
                .expect("after")
                .is_empty()
        );

        assert!(db.take_rewind_alert(session_id).await.expect("take alert"));
        assert!(
            !db.take_rewind_alert(session_id)
                .await
                .expect("alert is one-shot")
        );
    }

    #[tokio::test]
    async fn rewind_handles_compacted_snapshots_both_after_and_across_handoff() {
        let dir = tempdir().expect("tempdir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open db");
        let session_id = "rewind-compaction";
        db.create_session(session_id, Some("Compaction"), None)
            .await
            .expect("create session");
        let fence = RunFence {
            session_id: session_id.to_string(),
            run_id: "run-compact".to_string(),
        };
        assert!(db.claim_session_run(&fence).await.expect("claim"));

        let at = jiff::Timestamp::now();
        let mut msg_ids = Vec::new();
        for (idx, prompt) in ["p0", "p1", "p2", "p3"].into_iter().enumerate() {
            let u = db
                .append_chat_message_as_run(&fence, &ChatMessage::user(prompt))
                .await
                .expect("user");
            db.record_prompt_watermark(&fence, i64::from(u.id), i64::try_from(idx * 2).unwrap())
                .await
                .expect("watermark");
            db.append_chat_message_as_run(&fence, &ChatMessage::assistant("ans", at))
                .await
                .expect("assistant");
            msg_ids.push(i64::from(u.id));
        }

        let mut ctx = Context::new();
        let mut raw_entries = Vec::new();
        for (idx, prompt) in ["p0", "p1"].into_iter().enumerate() {
            let u = LlmMessage::user(prompt);
            let a = LlmMessage::assistant("ans");
            ctx.push(u.clone());
            ctx.push(a.clone());
            let base = i64::try_from(idx * 2).unwrap();
            raw_entries.push(transcript_entry(base, &u));
            raw_entries.push(transcript_entry(base + 1, &a));
        }
        let fork = ctx.fork_point();
        assert_eq!(fork, 4);
        ctx.apply_handoff(Handoff::new("summary of p0..p1"), fork, 0, &HashSet::new());
        for (idx, prompt) in ["p2", "p3"].into_iter().enumerate() {
            let u = LlmMessage::user(prompt);
            let a = LlmMessage::assistant("ans");
            ctx.push(u.clone());
            ctx.push(a.clone());
            let base = 4 + i64::try_from(idx * 2).unwrap();
            raw_entries.push(transcript_entry(base, &u));
            raw_entries.push(transcript_entry(base + 1, &a));
        }
        db.upsert_session_context_snapshot(
            session_id,
            SessionContextSnapshotUpsert {
                checkpoint: checkpoint_of(&ctx),
                applied_seq: 7,
                transcript_entries: raw_entries,
            },
        )
        .await
        .expect("upsert");

        // 1. Rewind to p3 (`cut_seq = 6`, after compaction): handoff + p2 stay.
        db.rewind_session_from(&fence, msg_ids[3], msg_ids[3])
            .await
            .expect("rewind to p3");
        let after_p3 = db
            .get_session_context_snapshot(session_id)
            .await
            .expect("read")
            .expect("snapshot kept");
        assert!(after_p3.checkpoint.context.handoff().is_some());
        assert_eq!(after_p3.applied_seq, 5);
        assert_eq!(after_p3.checkpoint.context.items().len(), 2);
        assert_eq!(after_p3.checkpoint.context.fork_point(), 6);

        // 2. Rewind to p2 (`cut_seq = 4`, very first prompt after compaction):
        //    handoff stays, items become empty, `fork_point` stays 4.
        db.rewind_session_from(&fence, msg_ids[2], msg_ids[2])
            .await
            .expect("rewind to p2");
        let after_p2 = db
            .get_session_context_snapshot(session_id)
            .await
            .expect("read")
            .expect("snapshot kept at handoff boundary");
        assert!(after_p2.checkpoint.context.handoff().is_some());
        assert_eq!(after_p2.checkpoint.context.items(), []);
        assert_eq!(after_p2.applied_seq, 3);
        assert_eq!(after_p2.checkpoint.context.fork_point(), 4);

        // 3. Rewind to p1 (`cut_seq = 2`, inside handoff): snapshot is dropped
        //    and transcript retains only seq 0..1 (`p0` + `ans`).
        db.rewind_session_from(&fence, msg_ids[1], msg_ids[1])
            .await
            .expect("rewind to p1");
        assert!(
            db.get_session_context_snapshot(session_id)
                .await
                .expect("read")
                .is_none(),
            "compacted snapshot covering cut_seq must be dropped"
        );
        let remaining = db
            .transcript_entries_after(session_id, -1)
            .await
            .expect("entries");
        assert_eq!(
            remaining.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }
}
