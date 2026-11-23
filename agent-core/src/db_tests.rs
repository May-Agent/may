//! Database tests that exercise session/history helpers living above `may-db`.

#![cfg(test)]

use crate::db::*;
use may_db::query;

mod cache_misses_tests {
    use std::time::Duration;

    use aither_agent::context::{PrefixChain, PrefixRewrites, PromptPrefix};
    use aither_core::llm::tool::ToolDefinition;
    use aither_core::llm::{Message, Usage};
    use may_protocol::cache_lifetime::CacheLifetime;
    use tempfile::tempdir;

    use super::*;
    use crate::agent::cache_miss::{CacheMiss, CacheMissCause};
    use crate::agent::{SessionUsageReport, assemble_usage_report};
    use crate::db::SessionIntervals;

    const SESSION: &str = "session-cache";
    const START_MS: i64 = 1_700_000_000_000;

    fn tool(name: &'static str) -> ToolDefinition {
        ToolDefinition::from_parts(
            name.into(),
            "runs a command".into(),
            serde_json::json!({"type": "object"}),
        )
        .expect("an object schema is valid")
    }

    /// A session's requests on one route, as the turn runner reports them.
    struct Session {
        db: ChatDb,
        tools: Vec<ToolDefinition>,
        messages: Vec<Message>,
        at_ms: i64,
        cached: u32,
        rewrites: PrefixRewrites,
        first_request: bool,
        model: &'static str,
    }

    impl Session {
        async fn open(dir: &tempfile::TempDir) -> Self {
            let db = ChatDb::open(&dir.path().join("history.db"))
                .await
                .expect("db opens");
            db.create_session(SESSION, Some("Cache"), Some("opus"))
                .await
                .expect("session is created");
            Self {
                db,
                tools: vec![tool("terminal")],
                messages: vec![Message::system("you are may"), Message::user("start")],
                at_ms: START_MS,
                cached: 0,
                rewrites: PrefixRewrites::default(),
                first_request: true,
                model: "opus",
            }
        }

        /// Sends one request `after` the previous one that reads `read`
        /// tokens from the cache and writes 1000 more.
        async fn request(&mut self, after: Duration, read: u32) -> Option<CacheMiss> {
            self.at_ms += i64::try_from(after.as_millis()).expect("fits");
            let mut messages = self.messages.clone();
            messages.push(Message::system("live tail"));
            let usage = Usage::new(read + 1_000 + 200, 50).with_cache_tokens(read, 1_000);
            let miss = self
                .db
                .observe_prompt_cache(PromptCacheRequest {
                    session_id: SESSION,
                    provider_id: "anthropic",
                    model: self.model,
                    at: jiff::Timestamp::from_millisecond(self.at_ms).expect("in range"),
                    usage: &usage,
                    prefix: PromptPrefix {
                        chain: PrefixChain::of(&self.tools, &messages, true),
                        rewrites: std::mem::take(&mut self.rewrites),
                        first_request: std::mem::replace(&mut self.first_request, false),
                    },
                    lifetime: CacheLifetime::Limited(Duration::from_mins(5)),
                    published_block: None,
                })
                .await
                .expect("the request is observed");
            self.cached = read + 1_000;
            self.messages.push(Message::assistant("done"));
            self.messages.push(Message::user("next"));
            miss
        }

        /// A request that reads everything the route had cached.
        async fn warm_request(&mut self) -> Option<CacheMiss> {
            self.request(Duration::from_secs(30), self.cached).await
        }

        async fn report(&self) -> SessionUsageReport {
            let misses = self
                .db
                .cache_misses_for_session(SESSION)
                .await
                .expect("misses load");
            assemble_usage_report(
                SESSION,
                &[],
                &misses,
                &SessionIntervals::default(),
                Some(START_MS),
                self.at_ms,
            )
        }
    }

    /// Each cause produces exactly one record, and the warm requests around
    /// it produce none.
    async fn one_miss_named(session: &mut Session, miss: Option<CacheMiss>, cause: CacheMissCause) {
        assert_eq!(miss.map(|miss| miss.cause), Some(cause));
        assert_eq!(
            session.warm_request().await,
            None,
            "the next request is warm"
        );
        let report = session.report().await;
        assert_eq!(report.cache_miss_causes.len(), 1, "{report:?}");
        assert_eq!(report.cache_miss_causes[0].cause, cause);
        assert_eq!(report.cache_miss_causes[0].misses, 1);
        assert_eq!(report.recent_cache_misses.len(), 1);
        assert_eq!(report.recent_cache_misses[0].cause, cause);
        assert_eq!(report.models[0].cache_misses, 1);
    }

    #[tokio::test]
    async fn a_session_whose_prefix_never_changes_records_no_miss() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        for _ in 0..10 {
            assert_eq!(session.warm_request().await, None);
        }
        let report = session.report().await;
        assert!(report.cache_miss_causes.is_empty());
        assert!(report.recent_cache_misses.is_empty());
    }

    /// A provider that reports no writes and publishes no block size: the
    /// block is inferred from the route's reads, so sub-block shortfalls stay
    /// unrecorded once it is known and a real drop is still recorded.
    #[tokio::test]
    async fn an_inferred_block_absorbs_sub_block_shortfalls() {
        let dir = tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db opens");
        db.create_session(SESSION, Some("Cache"), Some("opus"))
            .await
            .expect("session is created");
        let mut at_ms = START_MS;
        let mut observe = async |prompt: u32, read: u32| {
            at_ms += 1_000;
            let mut usage = Usage::new(prompt, 10);
            usage.cache_read_tokens = Some(read);
            db.observe_prompt_cache(PromptCacheRequest {
                session_id: SESSION,
                provider_id: "custom",
                model: "m",
                at: jiff::Timestamp::from_millisecond(at_ms).expect("in range"),
                usage: &usage,
                prefix: PromptPrefix {
                    chain: PrefixChain::of(&[], &[Message::system("you are may")], false),
                    rewrites: PrefixRewrites::default(),
                    first_request: false,
                },
                lifetime: CacheLifetime::default(),
                published_block: None,
            })
            .await
            .expect("the request is observed")
        };
        // No nonzero read yet: nothing is credited beyond what was read.
        assert_eq!(observe(12_009, 0).await, None);
        assert_eq!(observe(12_244, 11_968).await, None);
        assert_eq!(observe(12_451, 12_224).await, None);
        assert_eq!(observe(13_842, 12_544).await, None);
        assert_eq!(observe(14_140, 13_824).await, None);
        // The reads so far share the block 64: 14_140 rounds to 14_080.
        assert_eq!(observe(14_300, 14_080).await, None);
        let miss = observe(14_900, 12_800).await.expect("a real drop");
        assert_eq!(miss.cause, CacheMissCause::ProviderDropped);
        assert_eq!(miss.missed_tokens, 14_272 - 12_800);
    }

    #[tokio::test]
    async fn an_expired_cache_lifetime_records_one_expiry() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        session.warm_request().await;
        session.warm_request().await;
        let miss = session.request(Duration::from_mins(6), 0).await;
        assert_eq!(miss.as_ref().map(|miss| miss.missed_tokens), Some(2_000));
        one_miss_named(&mut session, miss, CacheMissCause::Expired).await;
    }

    #[tokio::test]
    async fn a_compaction_records_one_compaction() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        for _ in 0..4 {
            session.warm_request().await;
        }
        session.messages = vec![Message::system("you are may"), Message::user("handoff")];
        session.rewrites.compacted = true;
        let miss = session.request(Duration::from_secs(30), 300).await;
        one_miss_named(&mut session, miss, CacheMissCause::Compaction).await;
    }

    #[tokio::test]
    async fn a_changed_tool_set_records_one_tool_change() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        session.warm_request().await;
        session.warm_request().await;
        session.tools.push(tool("monitor"));
        let miss = session.request(Duration::from_secs(30), 0).await;
        one_miss_named(&mut session, miss, CacheMissCause::ToolsChanged).await;
    }

    /// The route outlives the agent: a rebuilt agent's changed system
    /// prompt is measured against what its predecessor cached.
    #[tokio::test]
    async fn a_rebuilt_agent_is_measured_against_its_predecessor() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        session.warm_request().await;
        session.warm_request().await;
        session.first_request = true;
        session.messages[0] = Message::system("you are may, rebuilt");
        let miss = session.request(Duration::from_secs(30), 0).await;
        let stored = session
            .db
            .cache_misses_for_session(SESSION)
            .await
            .expect("misses load");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].diverged_kind.as_deref(), Some("system"));
        assert_eq!(stored[0].diverged_index, Some(0));
        assert!(stored[0].agent_rebuilt);
        one_miss_named(&mut session, miss, CacheMissCause::AgentRebuilt).await;
    }

    /// Another model's request in between is no miss on either route, and
    /// is named when the first route later misses.
    #[tokio::test]
    async fn a_model_switch_is_carried_by_the_next_miss() {
        let dir = tempdir().expect("temp dir");
        let mut session = Session::open(&dir).await;
        session.warm_request().await;
        session.warm_request().await;
        let cached = session.cached;
        session.model = "sonnet";
        assert_eq!(session.request(Duration::from_secs(10), 0).await, None);
        session.model = "opus";
        session.cached = cached;
        let miss = session.request(Duration::from_secs(10), 0).await;
        assert!(miss.as_ref().is_some_and(|miss| miss.since.model_switched));
        one_miss_named(&mut session, miss, CacheMissCause::ModelSwitched).await;
    }
}

mod session_intervals_tests {
    use may_protocol::wall_clock::Interval;
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
            keep_warm: false,
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

mod mod_tests {
    use tempfile::tempdir;

    use crate::PermissionRequestMode;
    use crate::db::ProfileEnvironmentKind;
    use crate::db::entity::scheduled_task_runs;
    use crate::profile::ProfileEnvironment;
    use crate::session::{
        PendingMessageState, PendingUserMessage, PendingUserMessageMode, PersistedAskUserQuestion,
        PersistedAskUserRequest, PersistedPermissionRequest, PersistedPermissionRequestPayload,
        PersistedWorkspaceRequest, PersistedWorkspaceRequestPayload, SessionPlanState,
        SessionRuntimeState,
    };
    use crate::task_trigger::PersistedTaskTrigger;

    use super::{ChatDb, CreateProfileParams, CreateScheduledTaskParams, RunFence, query};

    fn test_pending_message(content: &str, mode: PendingUserMessageMode) -> PendingUserMessage {
        PendingUserMessage {
            id: None,
            state: PendingMessageState::default(),
            error: None,
            content: content.to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            created_at: jiff::Timestamp::now().to_string(),
            delivery_mode: mode,
        }
    }

    #[tokio::test]
    async fn archive_and_unarchive_reset_pin_state_and_capacity() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let profile_id = "profile-test";
        db.create_profile(CreateProfileParams {
            id: profile_id,
            name: "Test",
            environment: ProfileEnvironmentKind::Local,
            slug: None,
            icon: None,
            persona: None,
            preferences: None,
            location_aware: false,
        })
        .await
        .expect("profile should be created");

        let session_id = "session-test";
        db.create_session_in_profile(
            session_id,
            Some("Test session"),
            Some("test-model"),
            Some(profile_id),
        )
        .await
        .expect("session should be created");

        db.pin_session(session_id)
            .await
            .expect("session should be pinned");
        assert_eq!(
            db.pinned_count(profile_id)
                .await
                .expect("pinned count should load"),
            1
        );

        db.archive_session(session_id)
            .await
            .expect("session should be archived");
        let archived = db
            .get_session(session_id)
            .await
            .expect("session fetch should succeed")
            .expect("session should exist");
        assert!(archived.archived, "session must be archived");
        assert!(
            !archived.pinned,
            "archived session must be removed from pinned list"
        );
        assert!(
            archived.folder_id.is_none(),
            "archived session must not keep folder assignment"
        );
        assert!(
            archived.pin_sort_order.is_none(),
            "archived session must not keep pin order"
        );
        assert_eq!(
            db.pinned_count(profile_id)
                .await
                .expect("pinned count should load"),
            0
        );

        db.unarchive_session(session_id)
            .await
            .expect("session should be unarchived");
        let reopened = db
            .get_session(session_id)
            .await
            .expect("session fetch should succeed")
            .expect("session should exist");
        assert!(
            !reopened.archived,
            "unarchived session must return to regular list"
        );
        assert!(
            !reopened.pinned,
            "unarchived session must not auto-return to pinned"
        );
        assert!(
            reopened.folder_id.is_none(),
            "unarchived session must not keep folder assignment"
        );
        assert!(
            reopened.pin_sort_order.is_none(),
            "unarchived session must not keep pin order"
        );
        assert_eq!(
            db.pinned_count(profile_id)
                .await
                .expect("pinned count should load"),
            0
        );
    }

    #[tokio::test]
    async fn move_to_folder_requires_pinned_and_non_archived_session() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let profile_id = "profile-test";
        db.create_profile(CreateProfileParams {
            id: profile_id,
            name: "Test",
            environment: ProfileEnvironmentKind::Local,
            slug: None,
            icon: None,
            persona: None,
            preferences: None,
            location_aware: false,
        })
        .await
        .expect("profile should be created");

        let session_id = "session-test";
        db.create_session_in_profile(
            session_id,
            Some("Test session"),
            Some("test-model"),
            Some(profile_id),
        )
        .await
        .expect("session should be created");

        let folder_id = "folder-test";
        db.create_folder(folder_id, profile_id, "Pinned Folder")
            .await
            .expect("folder should be created");

        let unpinned_err = db
            .move_session_to_folder(session_id, Some(folder_id))
            .await
            .expect_err("moving unpinned session into folder must fail");
        assert!(
            unpinned_err
                .to_string()
                .contains("cannot move unpinned session"),
            "unexpected error: {unpinned_err}"
        );

        db.pin_session(session_id)
            .await
            .expect("session should be pinned");
        db.archive_session(session_id)
            .await
            .expect("session should be archived");

        let archived_err = db
            .move_session_to_folder(session_id, Some(folder_id))
            .await
            .expect_err("moving archived session into folder must fail");
        assert!(
            archived_err
                .to_string()
                .contains("cannot move archived session"),
            "unexpected error: {archived_err}"
        );
    }

    #[tokio::test]
    async fn ensure_default_profiles_preserve_existing_profiles_by_slug() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_profile(CreateProfileParams {
            id: "daily-id",
            name: "Custom Daily",
            environment: ProfileEnvironmentKind::Container,
            slug: Some("daily"),
            icon: Some("D"),
            persona: None,
            preferences: None,
            location_aware: false,
        })
        .await
        .expect("daily profile should be created");
        db.create_profile(CreateProfileParams {
            id: "build-id",
            name: "Custom Build",
            environment: ProfileEnvironmentKind::Local,
            slug: Some("build"),
            icon: Some("B"),
            persona: None,
            preferences: None,
            location_aware: true,
        })
        .await
        .expect("build profile should be created");

        let fallback_id = db
            .ensure_default_profiles()
            .await
            .expect("default profiles should exist");
        assert_eq!(fallback_id, "build-id");

        let all = db.list_profiles().await.expect("profiles should be listed");
        assert_eq!(
            all.len(),
            3,
            "existing slugged profiles are reused and the missing default is created"
        );

        let daily = db
            .get_profile_by_slug("daily")
            .await
            .expect("daily query should succeed")
            .expect("daily profile should exist");
        assert_eq!(daily.id, "daily-id");
        assert_eq!(daily.name, "Custom Daily");
        assert_eq!(daily.environment, "container");
        assert_eq!(daily.icon.as_deref(), Some("D"));
        assert!(!daily.location_aware);

        let build = db
            .get_profile_by_slug("build")
            .await
            .expect("build query should succeed")
            .expect("build profile should exist");
        assert_eq!(build.id, "build-id");
        assert_eq!(build.name, "Custom Build");
        assert_eq!(build.environment, "local");
        assert_eq!(build.icon.as_deref(), Some("B"));
        assert!(build.location_aware);

        let work = db
            .get_profile_by_slug("work")
            .await
            .expect("work query should succeed")
            .expect("work profile should exist");
        assert_eq!(work.id, "work");
        assert_eq!(work.name, "Work");
        assert_eq!(work.environment, "local");
        assert!(!work.location_aware);
    }

    #[tokio::test]
    async fn preserves_older_steer_messages_when_new_steer_arrives() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");

        db.set_pending_immediate_message(
            session_id,
            test_pending_message("first steer", PendingUserMessageMode::Steer),
        )
        .await
        .expect("first steer should persist");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("second steer", PendingUserMessageMode::Steer),
        )
        .await
        .expect("second steer should persist");

        let runtime = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(
            runtime
                .pending_immediate
                .as_ref()
                .map(|message| message.content.as_str()),
            Some("second steer")
        );
        assert_eq!(runtime.queued_messages.len(), 1);
        assert_eq!(runtime.queued_messages[0].content, "first steer");
        assert_eq!(
            runtime.queued_messages[0].delivery_mode,
            PendingUserMessageMode::Steer
        );

        let first = db
            .claim_next_pending_message(
                &RunFence {
                    session_id: session_id.to_string(),
                    run_id: "run".to_string(),
                },
                session_id,
            )
            .await
            .expect("first pending should load")
            .expect("first pending should exist");
        assert_eq!(first.content, "second steer");

        let second = db
            .claim_next_pending_message(
                &RunFence {
                    session_id: session_id.to_string(),
                    run_id: "run".to_string(),
                },
                session_id,
            )
            .await
            .expect("second pending should load")
            .expect("second pending should exist");
        assert_eq!(second.content, "first steer");
        assert_eq!(second.delivery_mode, PendingUserMessageMode::Steer);
    }

    #[tokio::test]
    async fn rejects_archiving_active_sessions() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_session_runtime_state(session_id, SessionRuntimeState::Running)
            .await
            .expect("runtime state should persist");

        let error = db
            .archive_session(session_id)
            .await
            .expect_err("active session must not archive");
        assert!(
            error.to_string().contains("cannot archive active session"),
            "unexpected error: {error}"
        );
    }

    /// The turn boundary is the largest stored row id of the session, as the
    /// full history load it replaced took it: 0 when nothing is stored, and
    /// never another session's rows.
    #[tokio::test]
    async fn max_message_id_matches_the_largest_history_row_id() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        db.create_session("session-a", Some("A"), Some("test-model"))
            .await
            .expect("first session should be created");
        db.create_session("session-b", Some("B"), Some("test-model"))
            .await
            .expect("second session should be created");
        assert_eq!(
            db.max_message_id("session-a")
                .await
                .expect("empty session boundary"),
            None
        );

        for (session, text) in [
            ("session-a", "one"),
            ("session-b", "other"),
            ("session-a", "two"),
            ("session-b", "other two"),
        ] {
            db.append_message(
                session,
                crate::session::MessageRole::User,
                text,
                None,
                crate::db::MessageMeta::default(),
            )
            .await
            .expect("message should append");
        }

        let from_history = db
            .get_messages("session-a")
            .await
            .expect("history should load")
            .iter()
            .map(|message| i64::from(message.id))
            .max();
        assert!(from_history.is_some());
        assert_eq!(
            db.max_message_id("session-a")
                .await
                .expect("boundary should load"),
            from_history
        );
    }

    #[tokio::test]
    async fn browser_takeover_state_persists_and_is_consumable() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_a = "session-a";
        let session_b = "session-b";
        db.create_session(session_a, Some("Session A"), Some("test-model"))
            .await
            .expect("first session should be created");
        db.create_session(session_b, Some("Session B"), Some("test-model"))
            .await
            .expect("second session should be created");

        db.set_browser_takeover_owner(session_a)
            .await
            .expect("browser takeover should persist");
        assert_eq!(
            db.find_browser_takeover_session()
                .await
                .expect("takeover owner should load"),
            Some(session_a.to_string())
        );
        assert!(
            db.session_has_browser_takeover(session_a)
                .await
                .expect("session takeover state should load")
        );

        db.clear_browser_takeover_owner(session_a)
            .await
            .expect("browser takeover release should persist");
        assert!(
            db.take_browser_takeover_alert_pending(session_a)
                .await
                .expect("pending takeover alert should be consumable")
        );
        assert!(
            !db.take_browser_takeover_alert_pending(session_a)
                .await
                .expect("pending takeover alert should clear after consumption")
        );

        db.set_browser_takeover_owner(session_b)
            .await
            .expect("browser takeover owner should switch");
        assert_eq!(
            db.find_browser_takeover_session()
                .await
                .expect("switched takeover owner should load"),
            Some(session_b.to_string())
        );
        assert!(
            !db.session_has_browser_takeover(session_a)
                .await
                .expect("previous takeover owner should be cleared")
        );

        db.recover_session_runtimes_after_restart(super::PreviousDaemon::Stopped)
            .await
            .expect("runtime recovery should clear stale takeover ownership");
        assert_eq!(
            db.find_browser_takeover_session()
                .await
                .expect("no active takeover should remain after restart"),
            None
        );
        assert!(
            db.take_browser_takeover_alert_pending(session_b)
                .await
                .expect("restart recovery should preserve takeover alert as pending")
        );
    }

    #[tokio::test]
    async fn takeover_resume_becomes_dispatchable_after_cancelled_run_settles() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");
        let session_id = "takeover-race";
        db.create_session(session_id, Some("Takeover race"), Some("test-model"))
            .await
            .expect("session should be created");
        let fence = RunFence {
            session_id: session_id.to_string(),
            run_id: "run-1".to_string(),
        };
        assert!(
            db.claim_session_run(&fence)
                .await
                .expect("run claim should succeed")
        );
        db.set_browser_takeover_owner(session_id)
            .await
            .expect("takeover should be active");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("", PendingUserMessageMode::Steer),
        )
        .await
        .expect("resume message should be queued");

        let released = db
            .clear_browser_takeover_owner(session_id)
            .await
            .expect("takeover should be released");
        assert!(
            !released.can_dispatch_follow_up(),
            "the still-running cancelled turn must retain the dispatch gate"
        );

        assert!(
            db.finish_session_run(&fence)
                .await
                .expect("cancelled run should settle")
        );
        let settled = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert!(
            settled.can_dispatch_follow_up(),
            "resume must become dispatchable after the cancelled run settles"
        );
        assert!(settled.interactions.browser.takeover_alert_pending);
    }

    #[tokio::test]
    async fn request_logs_persist_in_database() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.append_request_log(super::RequestLogParams {
            session_id: "session-test",
            timestamp: "2026-03-14T00:00:00Z",
            model: "gpt-5",
            provider_id: "openai",
            prompt_tokens: Some(10),
            completion_tokens: Some(20),
            total_tokens: Some(30),
            cache_read_tokens: Some(5),
            cache_write_tokens: Some(0),
            context_window: Some(200_000),
            elapsed_ms: 42,
            ok: false,
            error: Some("boom"),
            keep_warm: false,
        })
        .await
        .expect("request log should persist");

        let entries = db
            .list_request_logs(10)
            .await
            .expect("request logs should load");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "session-test");
        assert_eq!(entries[0].model, "gpt-5");
        assert_eq!(entries[0].provider_id, "openai");
        assert_eq!(entries[0].prompt_tokens, Some(10));
        assert_eq!(entries[0].completion_tokens, Some(20));
        assert_eq!(entries[0].total_tokens, Some(30));
        assert_eq!(entries[0].cache_read_tokens, Some(5));
        assert_eq!(entries[0].context_window, Some(200_000));
        assert_eq!(entries[0].elapsed_ms, 42);
        assert!(!entries[0].ok);
        assert_eq!(entries[0].error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn rejects_archiving_sessions_with_pending_follow_up_work() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("steer me", PendingUserMessageMode::Steer),
        )
        .await
        .expect("pending steer should persist");

        let error = db
            .archive_session(session_id)
            .await
            .expect_err("session with pending work must not archive");
        assert!(
            error.to_string().contains("pending follow-up work exists"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn rejects_archiving_sessions_waiting_on_workspace_access() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_waiting_workspace(
            session_id,
            Some(PersistedWorkspaceRequest {
                id: "workspace-1".to_string(),
                request: PersistedWorkspaceRequestPayload {
                    path: "/tmp/project".to_string(),
                    reason: "Need write access".to_string(),
                    session_id: Some(session_id.to_string()),
                },
            }),
        )
        .await
        .expect("waiting workspace should persist");

        let error = db
            .archive_session(session_id)
            .await
            .expect_err("session with pending workspace request must not archive");
        assert!(
            error.to_string().contains("cannot archive active session"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn deleting_a_profile_moves_its_sessions_to_the_default_profile() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        let default_id = db
            .ensure_default_profiles()
            .await
            .expect("default profiles should exist");
        db.create_profile(CreateProfileParams {
            id: "doomed",
            name: "Doomed",
            environment: ProfileEnvironmentKind::Local,
            slug: None,
            icon: None,
            persona: None,
            preferences: None,
            location_aware: false,
        })
        .await
        .expect("profile should be created");
        db.create_session_in_profile("s1", Some("one"), None, Some("doomed"))
            .await
            .expect("session should be created");

        db.delete_profile("doomed")
            .await
            .expect("a non-default profile can be deleted");

        assert!(
            db.get_profile("doomed")
                .await
                .expect("profile query")
                .is_none()
        );
        let session = db
            .get_session("s1")
            .await
            .expect("session query")
            .expect("session survives");
        assert_eq!(session.profile_id.as_deref(), Some(default_id.as_str()));
    }

    #[tokio::test]
    async fn the_default_profile_cannot_be_deleted() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        let default_id = db
            .ensure_default_profiles()
            .await
            .expect("default profiles should exist");

        let error = db
            .delete_profile(&default_id)
            .await
            .expect_err("the default profile is protected");
        assert!(matches!(
            error,
            crate::db::ChatDbError::DefaultProfileProtected
        ));
        assert!(
            db.get_profile(&default_id)
                .await
                .expect("profile query")
                .is_some()
        );
    }

    #[tokio::test]
    async fn profile_remote_attachments_round_trip() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let model = db
            .create_profile(CreateProfileParams {
                id: "local-profile",
                name: "Local",
                environment: ProfileEnvironmentKind::Local,
                slug: None,
                icon: None,
                persona: None,
                preferences: None,
                location_aware: false,
            })
            .await
            .expect("local profile should be created");

        db.set_profile_remote_servers(
            &model.id,
            &[
                "prod".to_string(),
                "staging".to_string(),
                "edge".to_string(),
            ],
        )
        .await
        .expect("attachments should persist");

        let info = db
            .profile_info_from_model(model)
            .await
            .expect("profile should deserialize");
        assert!(matches!(info.environment.kind(), ProfileEnvironment::Local));
        assert_eq!(
            info.remote_attachments,
            vec![
                "prod".to_string(),
                "staging".to_string(),
                "edge".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn profile_remote_attachments_replace_on_rewrite() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let model = db
            .create_profile(CreateProfileParams {
                id: "attach-profile",
                name: "Attach",
                environment: ProfileEnvironmentKind::Local,
                slug: None,
                icon: None,
                persona: None,
                preferences: None,
                location_aware: false,
            })
            .await
            .expect("profile should be created");

        db.set_profile_remote_servers(&model.id, &["a".to_string(), "b".to_string()])
            .await
            .expect("initial attachments");
        db.set_profile_remote_servers(&model.id, &["c".to_string()])
            .await
            .expect("rewrite attachments");
        let attachments = db
            .list_profile_remote_servers(&model.id)
            .await
            .expect("attachments should load");
        assert_eq!(attachments, vec!["c".to_string()]);
    }

    #[tokio::test]
    async fn restart_recovery_preserves_waiting_requests_and_pending_dispatch() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("steer me", PendingUserMessageMode::Steer),
        )
        .await
        .expect("pending steer should persist");
        db.set_waiting_permission(
            session_id,
            Some(PersistedPermissionRequest {
                id: "perm-1".to_string(),
                request: PersistedPermissionRequestPayload::new(
                    PermissionRequestMode::Default,
                    "echo hi".to_string(),
                    Some(session_id.to_string()),
                    None,
                ),
            }),
        )
        .await
        .expect("waiting permission should persist");
        db.set_waiting_ask_user(
            session_id,
            Some(PersistedAskUserRequest {
                id: "ask-1".to_string(),
                questions: vec![PersistedAskUserQuestion {
                    section: "default".to_string(),
                    question: "Proceed?".to_string(),
                    option: vec!["yes".to_string()],
                    multi_select: false,
                    secret: None,
                }],
            }),
        )
        .await
        .expect("waiting ask-user should persist");
        db.set_waiting_workspace(
            session_id,
            Some(PersistedWorkspaceRequest {
                id: "workspace-1".to_string(),
                request: PersistedWorkspaceRequestPayload {
                    path: "/tmp/project".to_string(),
                    reason: "Need write access".to_string(),
                    session_id: Some(session_id.to_string()),
                },
            }),
        )
        .await
        .expect("waiting workspace should persist");

        db.recover_session_runtimes_after_restart(super::PreviousDaemon::Stopped)
            .await
            .expect("runtime recovery should succeed");

        let runtime = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(runtime.state, SessionRuntimeState::WaitingPermission);
        assert!(runtime.waiting_permission.is_some());
        assert!(runtime.waiting_workspace.is_some());
        assert!(runtime.waiting_ask_user.is_some());
        assert!(runtime.has_pending_immediate());
        assert_eq!(runtime.queued_count, 1);

        let resumable = db
            .sessions_requiring_pending_dispatch()
            .await
            .expect("pending dispatch lookup should succeed");
        assert_eq!(resumable, [] as [std::string::String; 0]);
    }

    #[tokio::test]
    async fn dispatchable_pending_message_requires_idle_runtime() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("resume later", PendingUserMessageMode::Steer),
        )
        .await
        .expect("pending steer should persist");
        db.set_waiting_permission(
            session_id,
            Some(PersistedPermissionRequest {
                id: "perm-1".to_string(),
                request: PersistedPermissionRequestPayload::new(
                    PermissionRequestMode::Default,
                    "echo hi".to_string(),
                    Some(session_id.to_string()),
                    None,
                ),
            }),
        )
        .await
        .expect("waiting permission should persist");

        let blocked = db
            .take_next_dispatchable_pending_message(
                &RunFence {
                    session_id: session_id.to_string(),
                    run_id: "run".to_string(),
                },
                session_id,
            )
            .await
            .expect("dispatchable lookup should succeed");
        assert!(blocked.is_none());

        db.set_waiting_permission(session_id, None)
            .await
            .expect("waiting permission should clear");
        db.set_session_runtime_state(session_id, SessionRuntimeState::Idle)
            .await
            .expect("runtime should become idle before resumed dispatch");

        let resumed = db
            .take_next_dispatchable_pending_message(
                &RunFence {
                    session_id: session_id.to_string(),
                    run_id: "run".to_string(),
                },
                session_id,
            )
            .await
            .expect("dispatchable lookup should succeed")
            .expect("pending steer should dispatch once idle");
        assert_eq!(resumed.content, "resume later");
    }

    #[tokio::test]
    async fn task_run_recovery_marks_running_runs_interrupted() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_scheduled_task(CreateScheduledTaskParams {
            id: "task-1",
            profile_id: "profile-1",
            session_id: Some("session-1"),
            name: "Demo task",
            trigger: &PersistedTaskTrigger::Event {
                event_name: "chat.turn.completed".to_string(),
            },
            cron_expression: None,
            run_at: None,
            script_path: None,
            script_inline: Some("echo hi"),
            needs_network: false,
            timeout_secs: 60,
        })
        .await
        .expect("task should persist");
        let task = db
            .get_scheduled_task("task-1")
            .await
            .expect("task lookup should succeed")
            .expect("task should exist");
        db.start_task_run("run-1", &task, "event")
            .await
            .expect("task run should persist");

        db.recover_task_runs_after_restart()
            .await
            .expect("task run recovery should succeed");

        let recovered = query!(
            db.db,
            "SELECT * FROM scheduled_task_runs WHERE id = ?",
            "run-1"
        )
        .fetch_optional::<scheduled_task_runs::Model>()
        .await
        .expect("task run lookup should succeed")
        .expect("task run should exist");
        assert_eq!(recovered.status, "interrupted");
        assert!(recovered.finished_at.is_some());
        assert_eq!(
            recovered.output.as_deref(),
            Some("May server restarted before the task run completed")
        );
    }

    #[tokio::test]
    async fn open_puts_the_database_in_wal_mode() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        let mode = query!(db.db, "PRAGMA journal_mode")
            .fetch_one::<JournalMode>()
            .await
            .expect("journal mode should be readable");
        assert_eq!(mode.journal_mode, "wal");
    }

    /// The audit record of a permission request keeps how May read the
    /// script next to the script.
    #[tokio::test]
    async fn a_permission_request_is_audited_with_its_reading() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        db.create_session("s", Some("t"), Some("m"))
            .await
            .expect("session should be created");
        let script = "cat src/lib.rs";
        let reading = crate::command_reading::CommandReading::of(script);
        db.append_permission_request_message(
            "s",
            "r1",
            crate::PermissionHistoryMode::Unsafe,
            script,
            Some(&reading),
            None,
        )
        .await
        .expect("request should be recorded");
        let row = query!(
            db.db,
            "SELECT script, reading FROM message_permission_requests WHERE request_id = ?",
            "r1"
        )
        .fetch_one::<AuditedRequest>()
        .await
        .expect("audit row should be readable");
        assert_eq!(row.script, script);
        let stored: crate::command_reading::CommandReading =
            serde_json::from_str(&row.reading.expect("reading is recorded"))
                .expect("reading is stored as the wire form");
        assert_eq!(stored, reading);
    }

    #[derive(Debug, serde::Deserialize, skyzen::FromRow)]
    struct AuditedRequest {
        script: String,
        reading: Option<String>,
    }

    #[derive(Debug, serde::Deserialize, skyzen::FromRow)]
    struct JournalMode {
        journal_mode: String,
    }

    /// Concurrent dequeues of distinct sessions' pending messages on one
    /// database file must all succeed and hand back every queued message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_pending_message_takes_never_hit_lock_errors() {
        const SESSIONS: usize = 16;
        const MESSAGES: usize = 10;

        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        let session_ids = (0..SESSIONS)
            .map(|index| format!("session-{index}"))
            .collect::<Vec<_>>();
        for session_id in &session_ids {
            db.create_session(session_id, Some("Session"), Some("test-model"))
                .await
                .expect("session should persist");
            for index in 0..MESSAGES {
                db.push_queued_message(
                    session_id,
                    test_pending_message(
                        &format!("message {index}"),
                        PendingUserMessageMode::Queue,
                    ),
                )
                .await
                .expect("queued message should persist");
            }
        }

        let tasks = session_ids
            .into_iter()
            .map(|session_id| {
                let db = db.clone();
                tokio::spawn(async move {
                    for index in 0..MESSAGES {
                        let message = db
                            .claim_next_pending_message(
                                &RunFence {
                                    session_id: session_id.clone(),
                                    run_id: "run".to_string(),
                                },
                                &session_id,
                            )
                            .await
                            .unwrap_or_else(|error| {
                                panic!("take {index} of {session_id} failed: {error}")
                            })
                            .unwrap_or_else(|| {
                                panic!("take {index} of {session_id} found no message")
                            });
                        assert_eq!(message.content, format!("message {index}"));
                    }
                    assert!(
                        db.claim_next_pending_message(
                            &RunFence {
                                session_id: session_id.clone(),
                                run_id: "run".to_string()
                            },
                            &session_id
                        )
                        .await
                        .expect("final take should succeed")
                        .is_none(),
                        "{session_id} should have no pending message left"
                    );
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            task.await.expect("take task should not panic");
        }
    }

    /// A session whose plan is still a draft accepts a new request but is not
    /// archived: the draft only blocks auto-archive, not admission.
    #[tokio::test]
    async fn a_draft_session_admits_requests_but_is_not_archived() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db should initialize successfully");
        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("session should be created");

        let runtime = db
            .set_session_plan_state("session", SessionPlanState::Draft)
            .await
            .expect("plan state should be set");
        assert!(runtime.admits_new_request());
        assert!(runtime.blocks_auto_archive());

        let error = db
            .archive_session("session")
            .await
            .expect_err("a draft plan must block archiving");
        assert!(
            format!("{error}").contains("draft"),
            "the refusal should name the draft plan: {error}"
        );
        assert!(
            !db.get_session("session")
                .await
                .expect("session lookup")
                .expect("session exists")
                .archived,
            "the auto-archive pass must leave the draft session open"
        );
    }

    fn queue_entry_fence(session_id: &str) -> RunFence {
        RunFence {
            session_id: session_id.to_string(),
            run_id: "run-1".to_string(),
        }
    }

    #[tokio::test]
    async fn a_prep_failure_after_claim_returns_the_message_to_pending() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let session_id = "claim-release";
        db.create_session(session_id, Some("t"), Some("model"))
            .await
            .expect("session");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("queued input", PendingUserMessageMode::Queue),
        )
        .await
        .expect("enqueue");
        let fence = queue_entry_fence(session_id);
        assert!(db.claim_session_run(&fence).await.expect("run claim"));

        let claimed = db
            .claim_next_pending_message(&fence, session_id)
            .await
            .expect("claim")
            .expect("message waits in the queue");
        assert_eq!(claimed.state, PendingMessageState::Claimed);
        let entry_id = claimed.id.expect("claimed rows carry their entry id");
        assert_eq!(
            db.load_session_runtime_summary(session_id)
                .await
                .expect("summary")
                .queued_messages
                .len(),
            1,
            "a claimed entry stays visible to the queue's readers"
        );

        db.release_pending_claim(entry_id, &fence)
            .await
            .expect("release");
        let reclaimed = db
            .claim_next_pending_message(&fence, session_id)
            .await
            .expect("second claim")
            .expect("the released entry is pending again and still runs");
        assert_eq!(reclaimed.content, "queued input");
    }

    #[tokio::test]
    async fn a_lost_claim_returns_to_pending_on_recovery() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let session_id = "claim-crash";
        db.create_session(session_id, Some("t"), Some("model"))
            .await
            .expect("session");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("queued input", PendingUserMessageMode::Queue),
        )
        .await
        .expect("enqueue");
        let fence = queue_entry_fence(session_id);
        assert!(db.claim_session_run(&fence).await.expect("run claim"));
        db.claim_next_pending_message(&fence, session_id)
            .await
            .expect("claim")
            .expect("message waits in the queue");

        db.recover_session_runtimes_after_restart(super::PreviousDaemon::Stopped)
            .await
            .expect("recovery");
        let fence2 = RunFence {
            session_id: session_id.to_string(),
            run_id: "run-2".to_string(),
        };
        assert!(
            db.claim_session_run(&fence2).await.expect("new run claim"),
            "the expired lease frees the session"
        );
        let reclaimed = db
            .claim_next_pending_message(&fence2, session_id)
            .await
            .expect("second claim")
            .expect("the dead run's claim returns to pending");
        assert_eq!(reclaimed.content, "queued input");
    }

    #[tokio::test]
    async fn a_committed_pending_message_never_runs_again() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let session_id = "claim-commit";
        db.create_session(session_id, Some("t"), Some("model"))
            .await
            .expect("session");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("queued input", PendingUserMessageMode::Queue),
        )
        .await
        .expect("enqueue");
        let fence = queue_entry_fence(session_id);
        assert!(db.claim_session_run(&fence).await.expect("run claim"));
        let claimed = db
            .claim_next_pending_message(&fence, session_id)
            .await
            .expect("claim")
            .expect("message waits in the queue");
        let entry_id = claimed.id.expect("claimed rows carry their entry id");

        db.append_chat_message_as_run_and_commit_pending(
            &fence,
            entry_id,
            &crate::session::ChatMessage::user("queued input"),
        )
        .await
        .expect("commit");
        assert!(
            db.claim_next_pending_message(&fence, session_id)
                .await
                .expect("second claim")
                .is_none(),
            "a committed entry never runs twice"
        );
        assert!(
            db.load_session_runtime_summary(session_id)
                .await
                .expect("summary")
                .queued_messages
                .is_empty(),
            "a committed entry leaves the queue"
        );
        let history = crate::session::load_history_from_db(&db, session_id)
            .await
            .expect("history");
        assert_eq!(
            history
                .messages()
                .iter()
                .filter(|m| m.role == crate::session::MessageRole::User)
                .count(),
            1,
            "the committed input lands in the transcript exactly once"
        );
    }

    #[tokio::test]
    async fn sessions_requiring_pending_dispatch_filters_to_idle_pending_sessions_newest_first() {
        let dir = tempdir().expect("temp dir should be created");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");

        db.create_session("idle-empty", Some("empty"), Some("model"))
            .await
            .expect("idle-empty");
        db.create_session("idle-committed", Some("committed"), Some("model"))
            .await
            .expect("idle-committed");
        db.set_pending_immediate_message(
            "idle-committed",
            test_pending_message("done", PendingUserMessageMode::Queue),
        )
        .await
        .expect("enqueue committed");
        let committed_fence = queue_entry_fence("idle-committed");
        assert!(db.claim_session_run(&committed_fence).await.expect("run"));
        let committed = db
            .claim_next_pending_message(&committed_fence, "idle-committed")
            .await
            .expect("claim")
            .expect("entry");
        db.append_chat_message_as_run_and_commit_pending(
            &committed_fence,
            committed.id.expect("id"),
            &crate::session::ChatMessage::user("done"),
        )
        .await
        .expect("commit");
        db.finish_session_run(&committed_fence)
            .await
            .expect("finish");

        db.create_session("older-ready", Some("older"), Some("model"))
            .await
            .expect("older-ready");
        db.set_pending_immediate_message(
            "older-ready",
            test_pending_message("run me first", PendingUserMessageMode::Queue),
        )
        .await
        .expect("enqueue older");

        db.create_session("blocked-running", Some("running"), Some("model"))
            .await
            .expect("blocked-running");
        db.set_pending_immediate_message(
            "blocked-running",
            test_pending_message("wait", PendingUserMessageMode::Queue),
        )
        .await
        .expect("enqueue running");
        db.set_session_runtime_state("blocked-running", SessionRuntimeState::Running)
            .await
            .expect("running");

        db.create_session("newer-ready", Some("newer"), Some("model"))
            .await
            .expect("newer-ready");
        db.set_pending_immediate_message(
            "newer-ready",
            test_pending_message("run me too", PendingUserMessageMode::Steer),
        )
        .await
        .expect("enqueue newer");

        db.execute_for_test(
            "UPDATE sessions SET updated_at = '2026-11-01T00:00:00Z' WHERE id = 'older-ready'",
        )
        .await
        .expect("set older updated_at");
        db.execute_for_test(
            "UPDATE sessions SET updated_at = '2026-11-02T00:00:00Z' WHERE id = 'newer-ready'",
        )
        .await
        .expect("set newer updated_at");

        let resumable = db
            .sessions_requiring_pending_dispatch()
            .await
            .expect("pending dispatch lookup");
        assert_eq!(resumable, vec!["newer-ready", "older-ready"]);
    }
}
