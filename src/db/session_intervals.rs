//! Tool-call and user-wait intervals of a session, for the `/usage`
//! wall-clock split.
//!
//! A tool call is appended by the turn runner when it ends. A wait on the
//! user is recorded by triggers on the pending-prompt tables, so every way a
//! prompt leaves them (answered, cancelled, withdrawn, replaced, settled
//! with its run) ends the wait in the same transaction that clears it.

use skyzen_services::Db as SkyDb;

use crate::agent::{Interval, IntervalSet};

use super::{
    ChatDb, ChatDbError,
    db_sql::{self, SESSION_INTERVALS_SCHEMA},
    entity::session_intervals,
    query,
};

const TOOL: &str = "tool";
const USER_WAIT: &str = "user_wait";

/// A session's recorded intervals.
#[derive(Debug, Clone, Default)]
pub struct SessionIntervals {
    /// Tool calls.
    pub tools: IntervalSet,
    /// Waits on the user.
    pub user_waits: IntervalSet,
}

impl ChatDb {
    /// Install the triggers that record a wait when a pending prompt is
    /// cleared.
    pub(super) async fn ensure_session_interval_triggers(db: &SkyDb) -> Result<(), ChatDbError> {
        for statement in db_sql::blocks(SESSION_INTERVALS_SCHEMA) {
            query!(db, statement).execute().await?;
        }
        Ok(())
    }

    /// Record that a tool call of `session_id` ran for `interval`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the insert fails.
    pub async fn append_tool_interval(
        &self,
        session_id: &str,
        interval: Interval,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "INSERT INTO session_intervals (session_id, kind, start_ms, end_ms) VALUES (?, ?, ?, ?)",
            session_id,
            TOOL,
            interval.start_ms(),
            interval.end_ms()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// The tool-call and user-wait intervals recorded for `session_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn session_intervals(
        &self,
        session_id: &str,
    ) -> Result<SessionIntervals, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT * FROM session_intervals WHERE session_id = ?",
            session_id
        )
        .fetch_all::<session_intervals::Model>()
        .await?;
        let spans = |kind: &str| {
            IntervalSet::new(
                rows.iter()
                    .filter(|row| row.kind == kind)
                    .filter_map(|row| Interval::new(row.start_ms, row.end_ms)),
            )
        };
        Ok(SessionIntervals {
            tools: spans(TOOL),
            user_waits: spans(USER_WAIT),
        })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::agent::assemble_usage_report;
    use crate::ask_user::{AnswerValue, QuestionAnswer};
    use crate::db::RequestLogParams;
    use crate::session::{
        PendingUserMessage, PersistedAskUserQuestion, PersistedAskUserRequest,
        PersistedPermissionRequest, PersistedPermissionRequestPayload,
    };

    use super::*;

    fn iso(ms: i64) -> String {
        jiff::Timestamp::from_millisecond(ms)
            .expect("test times are in range")
            .to_string()
    }

    async fn open(dir: &tempfile::TempDir, session: &str) -> ChatDb {
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db opens");
        db.create_session(session, Some("Usage"), Some("test-model"))
            .await
            .expect("session is created");
        db
    }

    fn question(id: &str) -> PersistedAskUserRequest {
        PersistedAskUserRequest {
            id: id.to_string(),
            questions: vec![PersistedAskUserQuestion {
                section: "default".to_string(),
                question: "Proceed?".to_string(),
                option: vec!["yes".to_string()],
                multi_select: false,
                secret: None,
            }],
        }
    }

    fn permission(id: &str, session: &str) -> PersistedPermissionRequest {
        PersistedPermissionRequest {
            id: id.to_string(),
            request: PersistedPermissionRequestPayload::new(
                crate::PermissionRequestMode::Default,
                "echo hi".to_string(),
                Some(session.to_string()),
                None,
            ),
        }
    }

    /// A session with a request, a tool call and an answered `ask_user`: the
    /// split's four parts sum to the wall clock, and the answered question's
    /// wait is the part no request covers.
    #[tokio::test]
    async fn usage_splits_a_session_with_tool_calls_and_an_answered_ask_user() {
        let dir = tempdir().expect("temp dir");
        let session = "session-usage";
        let db = open(&dir, session).await;

        let now = jiff::Timestamp::now().as_millisecond();
        let created = now - 60_000;

        // A request that ended 40s ago after 4s, overlapping the tool call
        // that started 42s ago and ran 10s.
        db.append_request_log(RequestLogParams {
            session_id: session,
            timestamp: &iso(now - 40_000),
            model: "test-model",
            provider_id: "test",
            prompt_tokens: Some(10),
            completion_tokens: Some(10),
            total_tokens: Some(20),
            cache_read_tokens: None,
            cache_write_tokens: None,
            context_window: None,
            elapsed_ms: 4_000,
            ok: true,
            error: None,
        })
        .await
        .expect("request log persists");
        db.append_tool_interval(
            session,
            Interval::new(now - 42_000, now - 32_000).expect("forward"),
        )
        .await
        .expect("tool interval persists");

        // A blocking question asked 20s ago, answered now.
        db.set_waiting_ask_user(session, Some(question("ask-1")))
            .await
            .expect("question persists");
        db.execute_for_test(&format!(
            "UPDATE session_pending_ask_user_requests_v2 SET created_at = '{}'",
            iso(now - 20_000)
        ))
        .await
        .expect("question is backdated");
        db.commit_ask_user_answer(
            "ask-1",
            &[QuestionAnswer {
                section: "default".to_string(),
                answer: AnswerValue::Selected(vec!["yes".to_string()]),
            }],
            None::<fn(&PersistedAskUserRequest) -> PendingUserMessage>,
        )
        .await
        .expect("answer commits")
        .expect("question was pending");

        let rows = db.request_logs_for_session(session).await.expect("logs");
        let intervals = db.session_intervals(session).await.expect("intervals");
        let report =
            assemble_usage_report(session, &rows, &[], &intervals, Some(created), now + 1_000);
        let split = report.split;

        assert_eq!(report.wall_ms, 61_000);
        assert_eq!(split.wall_ms(), report.wall_ms, "the parts sum to the wall");
        assert_eq!(split.api_ms, 4_000);
        // The tool call minus the 2s the request overlaps it.
        assert_eq!(split.tools_ms, 8_000);
        // The wait runs from the backdated creation to the commit, give or
        // take the SQLite float rounding and the commit's own latency.
        assert!(
            (20_000..21_000).contains(&split.user_wait_ms),
            "user wait {}",
            split.user_wait_ms
        );
        assert!(split.idle_ms > 0);
    }

    #[tokio::test]
    async fn an_answered_permission_prompt_is_a_user_wait() {
        let dir = tempdir().expect("temp dir");
        let session = "session-permission";
        let db = open(&dir, session).await;
        db.set_waiting_permission(session, Some(permission("perm-1", session)))
            .await
            .expect("permission persists");
        db.execute_for_test(&format!(
            "UPDATE session_pending_permissions SET created_at = '{}'",
            iso(jiff::Timestamp::now().as_millisecond() - 5_000)
        ))
        .await
        .expect("permission is backdated");
        db.commit_permission_response("perm-1", true)
            .await
            .expect("decision commits")
            .expect("prompt was pending");

        let waits = db.session_intervals(session).await.expect("intervals");
        assert!((5_000..6_000).contains(&waits.user_waits.total_ms()));
    }

    #[tokio::test]
    async fn a_prompt_with_a_corrupt_timestamp_cannot_be_cleared_silently() {
        let dir = tempdir().expect("temp dir");
        let session = "session-corrupt";
        let db = open(&dir, session).await;
        db.set_waiting_permission(session, Some(permission("perm-1", session)))
            .await
            .expect("permission persists");
        db.execute_for_test("UPDATE session_pending_permissions SET created_at = 'garbage'")
            .await
            .expect("permission is corrupted");
        let error = db
            .clear_waiting_permission_request(session, "perm-1")
            .await
            .expect_err("clearing aborts");
        assert!(
            error
                .to_string()
                .contains("session_pending_permissions.created_at is not a timestamp"),
            "{error}"
        );
    }

    /// A prompt that is cancelled rather than answered kept the session
    /// waiting until it was cleared.
    #[tokio::test]
    async fn a_cancelled_prompt_records_its_wait_when_cleared() {
        let dir = tempdir().expect("temp dir");
        let session = "session-cancelled";
        let db = open(&dir, session).await;
        let now = jiff::Timestamp::now().as_millisecond();

        db.set_waiting_permission(session, Some(permission("perm-1", session)))
            .await
            .expect("permission persists");
        db.execute_for_test(&format!(
            "UPDATE session_pending_permissions SET created_at = '{}'",
            iso(now - 7_000)
        ))
        .await
        .expect("permission is backdated");
        db.clear_waiting_permission_request(session, "perm-1")
            .await
            .expect("permission is cancelled");

        db.set_waiting_ask_user(session, Some(question("ask-1")))
            .await
            .expect("question persists");
        db.execute_for_test(&format!(
            "UPDATE session_pending_ask_user_requests_v2 SET created_at = '{}'",
            iso(now - 3_000)
        ))
        .await
        .expect("question is backdated");
        db.clear_waiting_ask_user_request("ask-1")
            .await
            .expect("question is cancelled");

        let waits = db.session_intervals(session).await.expect("intervals");
        let total = waits.user_waits.total_ms();
        // Two waits ending now, 7s and 3s long, overlap for the last 3s.
        assert!((7_000..8_000).contains(&total), "waited {total}");
    }
}
