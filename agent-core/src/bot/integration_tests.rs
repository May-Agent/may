//! End-to-end integration tests for May Bot (`May-Agent/may#1310`).

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aither_core::llm::model::{Parameters, Profile};
use aither_core::llm::{Classify, Event, Failure, LLMRequest, LanguageModel, ToolCall};
use futures_lite::Stream;
use may_db::{ChatDb, NewBotInboxEntry};
use may_harness::harness::approval_pause::ApprovalPause;
use may_protocol::bot::{
    BotApprovalDecision, BotApprovalGate, BotApprovalPolicy, BotApprovalStatus, BotDelegateStatus,
    BotInboxKind, BotSseEvent, CreateBotRequest, GatePolicy, UpdateBotRequest,
};
use may_repo_search::backend::testing::MemoryCollections;

use crate::agent_link::{AgentDirectory, AgentMessaging, CacheLifetimes, ForkHosts};
use crate::bot::{
    ApprovalOutcome, BotMemoryError, BotModelFactory, BotService, BotServiceDeps, CuratorFactory,
    CuratorSession, DelegateRunResult, DelegateRunner, NoopDelegateRunner, NoopSessionBridge,
    SessionBridge, bot_native_tool_names,
};
use crate::session::{MayHomePaths, SessionRuntimeService};
use crate::sleep::SleepState;
use crate::task_wake::SessionPrompts;
use crate::ui_event_bus::UiEventBus;

#[derive(Debug)]
struct TestLlmError;
impl std::fmt::Display for TestLlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("test llm error")
    }
}
impl std::error::Error for TestLlmError {}
impl Classify for TestLlmError {
    fn failure(&self) -> Failure {
        Failure::permanent()
    }
}

#[derive(Clone)]
struct ScriptedBotLlm {
    script: Arc<Mutex<VecDeque<Vec<Event>>>>,
    requests: Arc<Mutex<Vec<LLMRequest>>>,
}

impl ScriptedBotLlm {
    fn new(turns: Vec<Vec<Event>>) -> (Self, Arc<Mutex<Vec<LLMRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = Self {
            script: Arc::new(Mutex::new(VecDeque::from(turns))),
            requests: requests.clone(),
        };
        (llm, requests)
    }
}

impl LanguageModel for ScriptedBotLlm {
    type Error = TestLlmError;

    fn respond(
        &self,
        request: LLMRequest,
    ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
        self.requests.lock().expect("requests").push(request);
        let events = self
            .script
            .lock()
            .expect("script")
            .pop_front()
            .unwrap_or_else(|| vec![Event::Text("idle internal trace".to_string())]);
        futures_lite::stream::iter(events.into_iter().map(Ok))
    }

    fn profile(&self) -> impl Future<Output = Result<Profile, Self::Error>> + Send {
        std::future::ready(Ok(Profile::new(
            "scripted",
            "test",
            "scripted-bot",
            "scripted test model",
            128_000,
        )))
    }
}

struct PerBotModelFactory {
    models: Mutex<std::collections::HashMap<String, ScriptedBotLlm>>,
    default_model: ScriptedBotLlm,
}

impl BotModelFactory<ScriptedBotLlm> for PerBotModelFactory {
    fn build_model(
        &self,
        bot: &may_db::BotRow,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<(ScriptedBotLlm, Parameters)>> + Send + '_>>
    {
        let llm = self
            .models
            .lock()
            .expect("models lock")
            .get(&bot.name)
            .cloned()
            .unwrap_or_else(|| self.default_model.clone());
        Box::pin(async move { Ok((llm, Parameters::default())) })
    }
}

/// Deterministic CJK-capable curator that writes leaf `.md` files and updates `MEMORY.md`,
/// deliberately failing on the first step when `fail_first_invariant` is set to verify warm retry.
struct ScriptedCuratorFactory {
    fail_first_invariant: bool,
}

struct ScriptedCuratorSession {
    staging_dir: PathBuf,
    fail_first_invariant: bool,
    steps: usize,
}

impl CuratorSession for ScriptedCuratorSession {
    fn step<'a>(
        &'a mut self,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BotMemoryError>> + Send + 'a>> {
        let staging_dir = self.staging_dir.clone();
        let prompt = prompt.to_string();
        Box::pin(async move {
            self.steps += 1;
            let pref_dir = staging_dir.join("preference");
            async_fs::create_dir_all(&pref_dir).await?;
            if self.fail_first_invariant && self.steps == 1 {
                // Write leaf without updating MEMORY.md so Rust invariant check triggers warm retry!
                async_fs::write(
                    pref_dir.join("food.md"),
                    "# Food\n\nLikes spicy Sichuan hotpot (麻辣烫), Japanese sushi (寿司), and Korean bibimbap (비빔밥).\n",
                )
                .await?;
                return Ok(());
            }
            if self.steps >= 2 || !self.fail_first_invariant {
                let body = if prompt.contains("ramen") || prompt.contains("拉面") {
                    "# Food\n\nUpdated: prefers tonkotsu ramen (拉面 / ラーメン / 라면) and spicy Sichuan hotpot.\n"
                } else {
                    "# Food\n\nLikes spicy Sichuan hotpot (麻辣烫), Japanese sushi (寿司), and Korean bibimbap (비빔밥).\n"
                };
                async_fs::write(pref_dir.join("food.md"), body).await?;
                async_fs::write(
                    pref_dir.join("MEMORY.md"),
                    "# Preferences\n\n- `food.md`: CJK food preferences (麻辣烫, 寿司, 비빔밥)\n",
                )
                .await?;
                async_fs::write(
                    staging_dir.join("MEMORY.md"),
                    "# Memory\n\n- `preference/`: User preferences\n",
                )
                .await?;
            }
            Ok(())
        })
    }
}

impl CuratorFactory for ScriptedCuratorFactory {
    fn open_session(
        &self,
        _bot_id: &str,
        staging_dir: PathBuf,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn CuratorSession>, BotMemoryError>> + Send + '_>>
    {
        let fail_first = self.fail_first_invariant;
        Box::pin(async move {
            Ok(Box::new(ScriptedCuratorSession {
                staging_dir,
                fail_first_invariant: fail_first,
                steps: 0,
            }) as Box<dyn CuratorSession>)
        })
    }
}

struct RecordingSessionBridge {
    db: ChatDb,
    created: Arc<Mutex<Vec<(String, String, String)>>>,
    stopped: Arc<Mutex<Vec<String>>>,
}

impl SessionBridge for RecordingSessionBridge {
    fn create_session(
        &self,
        bot_id: String,
        prompt: String,
        title: Option<String>,
        _profile_id: Option<String>,
        _model: Option<String>,
        _cwd: Option<String>,
        _repo: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let sid = crate::id::new();
            self.db
                .create_session(&sid, title.as_deref(), None)
                .await
                .map_err(|e| e.to_string())?;
            self.db
                .register_bot_supervised_session(&sid, &bot_id)
                .await
                .map_err(|e| e.to_string())?;
            self.created
                .lock()
                .expect("created")
                .push((bot_id, sid.clone(), prompt));
            Ok(sid)
        })
    }

    fn stop_session(
        &self,
        session_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            self.stopped.lock().expect("stopped").push(session_id);
            Ok(())
        })
    }
}

async fn wait_until<F, Fut>(mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..100 {
        if predicate().await {
            return;
        }
        async_io::Timer::after(Duration::from_millis(20)).await;
    }
    panic!("condition was not met within timeout");
}

#[tokio::test]
async fn bot_non_bash_surface_explicit_reply_guard_and_silent_background_wake() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = ChatDb::open(&dir.path().join("history.db"))
        .await
        .expect("open db");
    let ui_bus = UiEventBus::new();
    let sleep = Arc::new(SleepState::load(dir.path(), ui_bus.clone(), db.clone()));
    let sse_events: Arc<Mutex<Vec<BotSseEvent>>> = Arc::new(Mutex::new(Vec::new()));

    // Turn 1: UserMessage arrives; bot outputs ONLY plain text -> Moment::FinalTurn fires!
    // Turn 2: After FinalTurn reminder, bot calls send_message(to="user", ...) with CJK text.
    // Turn 3: Final confirmation text (internal trace only).
    // Turn 4: Background ScheduleWake arrives; bot stays silent (no send_message to user) and exits cleanly!
    let (llm, requests) = ScriptedBotLlm::new(vec![
        vec![Event::Text(
            "internal thought before calling tool".to_string(),
        )],
        vec![Event::ToolCall(ToolCall::new(
            "call-reply-1",
            "send_message",
            serde_json::json!({
                "to": "user",
                "message": "收到！了解しました！알겠습니다!"
            }),
        ))],
        vec![Event::Text("sent user reply".to_string())],
        vec![Event::Text(
            "silent background schedule check complete".to_string(),
        )],
    ]);

    let factory = Arc::new(PerBotModelFactory {
        models: Mutex::new(std::collections::HashMap::new()),
        default_model: llm,
    });
    let events_sink = sse_events.clone();
    let service = BotService::new(BotServiceDeps {
        db: db.clone(),
        may_dir: dir.path().to_path_buf(),
        model_factory: factory,
        curator_factory: Arc::new(ScriptedCuratorFactory {
            fail_first_invariant: false,
        }),
        delegate_runner: Arc::new(NoopDelegateRunner),
        search_backend: MemoryCollections::default(),
        sessions: Arc::new(NoopSessionBridge),
        runtime_service: None,
        notifications: None,
        sleep: Some(sleep.clone()),
        ui_event_bus: ui_bus,
        publish_event: Arc::new(move |evt| {
            events_sink.lock().expect("sse").push(evt);
        }),
    });

    let bot = service
        .create_bot(CreateBotRequest {
            name: "alpha".to_string(),
            persona: Some("Helpful multilingual bridge".to_string()),
            model: None,
            profile_id: None,
            enabled: true,
            approval_policy: BotApprovalPolicy::default(),
        })
        .await
        .expect("create bot");

    // Send user message; FinalTurn guard forces send_message(to="user") after Turn 1 plain text.
    let (_user_msg, _inbox) = service
        .send_user_message(&bot.id, "你好 / こんにちは / 안녕하세요")
        .await
        .expect("send user message");

    wait_until(|| {
        let svc = service.clone();
        let id = bot.id.clone();
        async move { svc.list_messages(&id, 20).await.unwrap_or_default().len() >= 2 }
    })
    .await;

    // Wait for Turn 3 trace & checkpoint persistence.
    wait_until(|| {
        let db = db.clone();
        let id = bot.id.clone();
        async move {
            db.get_bot_context_snapshot(&id)
                .await
                .ok()
                .flatten()
                .is_some()
        }
    })
    .await;

    // Verify non-bash-first tool surface: NEVER `terminal` or `monitor`, ONLY narrow native tools!
    let (req_len, tool_names) = {
        let reqs = requests.lock().expect("requests");
        let len = reqs.len();
        let names: Vec<String> = reqs[0]
            .tool_definitions()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        drop(reqs);
        (len, names)
    };
    assert!(
        req_len >= 2,
        "FinalTurn reminder must trigger a second LLM request"
    );
    assert!(
        !tool_names.iter().any(|n| n == "terminal"),
        "bot must NEVER expose terminal"
    );
    assert!(
        !tool_names.iter().any(|n| n == "monitor"),
        "bot must NEVER expose monitor"
    );
    for expected in bot_native_tool_names() {
        assert!(
            tool_names.iter().any(|n| n == expected),
            "bot must expose native tool `{expected}`"
        );
    }

    // Verify plain text stayed in trace ONLY and never polluted user chat!
    let msgs = service
        .list_messages(&bot.id, 50)
        .await
        .expect("list messages");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[1].role, "bot");
    assert_eq!(msgs[1].content, "收到！了解しました！알겠습니다!");

    let trace = service.list_trace(&bot.id, 50).await.expect("list trace");
    assert!(
        trace
            .iter()
            .any(|t| t.content.contains("internal thought before calling tool")),
        "plain assistant text must be captured in internal trace"
    );

    // Enqueue a background schedule wake and confirm bot finishes silently without user chat or FinalTurn loop!
    let _ = db
        .enqueue_bot_inbox(NewBotInboxEntry {
            bot_id: &bot.id,
            kind: BotInboxKind::ScheduleWake,
            sender_id: Some("sched-1"),
            sender_name: Some("schedule"),
            body: "Daily silent check",
            meta_json: None,
        })
        .await
        .expect("enqueue schedule wake");
    service.wake_bot(&bot.id);

    wait_until(|| {
        let svc = service.clone();
        let id = bot.id.clone();
        async move {
            svc.list_trace(&id, 50)
                .await
                .unwrap_or_default()
                .iter()
                .any(|t| {
                    t.content
                        .contains("silent background schedule check complete")
                })
        }
    })
    .await;

    let msgs_after = service
        .list_messages(&bot.id, 50)
        .await
        .expect("list messages");
    assert_eq!(
        msgs_after.len(),
        2,
        "silent background wake must NOT emit user chat messages"
    );

    // Verify SSE events emitted onto the single `/api/events` stream (`bot_state`, `bot_chat`, `bot_trace`).
    let events = sse_events.lock().expect("sse").clone();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BotSseEvent::BotState { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BotSseEvent::BotChat { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BotSseEvent::BotTrace { .. }))
    );
}

struct MockDelegateRunner {
    db: ChatDb,
}

impl DelegateRunner for MockDelegateRunner {
    fn run_worker(
        &self,
        bot_id: String,
        agent_id: String,
        _name: String,
        _definition: String,
        _work_dir: PathBuf,
        _grants: aither_sandbox::Grants,
        prompt: Option<String>,
        _checkpoint: Option<aither_agent::AgentCheckpoint>,
        _approvals: crate::bot::BotApprovalRouter,
        _pause: ApprovalPause,
        _steer_rx: async_channel::Receiver<String>,
        _cancel_out: Arc<Mutex<Option<aither_agent::CancelHandle>>>,
        _active_command: Arc<Mutex<Option<String>>>,
    ) -> Pin<Box<dyn Future<Output = DelegateRunResult> + Send + 'static>> {
        let db = self.db.clone();
        Box::pin(async move {
            let text = format!(
                "Worker completed task: {} (中文/日本語/한국어)",
                prompt.as_deref().unwrap_or("resumed")
            );
            let _ = db
                .append_bot_delegate_message(&agent_id, &bot_id, "assistant", &text)
                .await;
            let (llm, _) = ScriptedBotLlm::new(vec![]);
            let mut agent = aither_agent::Agent::builder(llm).build();
            let cp = agent.export_checkpoint().await.ok();
            DelegateRunResult {
                status: BotDelegateStatus::Completed,
                summary: text,
                prompt_tokens: 42,
                completion_tokens: 18,
                checkpoint: cp,
            }
        })
    }
}

#[tokio::test]
async fn bot_delegated_worker_checkpoint_resume_and_four_gate_approval_triage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = ChatDb::open(&dir.path().join("history.db"))
        .await
        .expect("open db");
    let ui_bus = UiEventBus::new();
    let runtime_service = Arc::new(SessionRuntimeService::new(
        db.clone(),
        ui_bus.clone(),
        MayHomePaths::new(dir.path().to_path_buf()),
    ));

    let (llm, _) = ScriptedBotLlm::new(vec![]);
    let factory = Arc::new(PerBotModelFactory {
        models: Mutex::new(std::collections::HashMap::new()),
        default_model: llm,
    });
    let session_bridge = Arc::new(RecordingSessionBridge {
        db: db.clone(),
        created: Arc::new(Mutex::new(Vec::new())),
        stopped: Arc::new(Mutex::new(Vec::new())),
    });

    let service = BotService::new(BotServiceDeps {
        db: db.clone(),
        may_dir: dir.path().to_path_buf(),
        model_factory: factory,
        curator_factory: Arc::new(ScriptedCuratorFactory {
            fail_first_invariant: true,
        }),
        delegate_runner: Arc::new(MockDelegateRunner { db: db.clone() }),
        search_backend: MemoryCollections::default(),
        sessions: session_bridge.clone(),
        runtime_service: Some(runtime_service.clone()),
        notifications: None,
        sleep: None,
        ui_event_bus: ui_bus,
        publish_event: Arc::new(|_| {}),
    });

    // Create TWO bots running concurrently to verify multi-bot isolation & AgentDirectory discovery.
    let bot_a = service
        .create_bot(CreateBotRequest {
            name: "coordinator-bot".to_string(),
            persona: None,
            model: None,
            profile_id: None,
            enabled: true,
            approval_policy: BotApprovalPolicy::default(),
        })
        .await
        .expect("bot a");
    let bot_b = service
        .create_bot(CreateBotRequest {
            name: "research-bot".to_string(),
            persona: None,
            model: None,
            profile_id: None,
            enabled: true,
            approval_policy: BotApprovalPolicy {
                unsafe_cmd: GatePolicy::AutoApprove,
                ssh: GatePolicy::BotTriage,
                publish: GatePolicy::AlwaysEscalate,
                workspace: GatePolicy::BotTriage,
            },
        })
        .await
        .expect("bot b");
    assert_ne!(bot_a.agent_id, bot_b.agent_id);

    // Verify AgentDirectory + AgentMessaging + SessionPrompts (TaskScheduler wake & webhooks) reach bots!
    let directory = AgentDirectory::new(runtime_service.clone(), dir.path().to_path_buf(), |_| {})
        .with_bot_wake(service.wake_callback());
    let live = directory.live().await.expect("live agents");
    assert!(live.iter().any(|a| a.session_id == bot_a.agent_id));
    assert!(live.iter().any(|a| a.session_id == bot_b.agent_id));

    let messaging = AgentMessaging {
        directory,
        runtime_service: runtime_service.clone(),
        cache_lifetime: CacheLifetimes::new(|_| crate::cache_lifetime::CacheLifetime::default()),
        forks: ForkHosts::default(),
    };
    let recipient = messaging
        .send(&bot_b.agent_id, &bot_a.agent_id, "Ping from research-bot")
        .await
        .expect("send to bot");
    assert_eq!(recipient, "coordinator-bot");

    let (wake_tx, _) = async_channel::unbounded();
    let prompts = SessionPrompts::new(runtime_service.clone(), wake_tx)
        .with_bot_wake(service.wake_callback());
    prompts
        .deliver(&bot_a.agent_id, "task-99", "nightly", "Nightly check alert")
        .await
        .expect("task wake to bot");
    assert!(
        prompts
            .deliver_webhook_to_bot(&bot_a.agent_id, "github", "PR merged webhook")
            .await
            .expect("webhook wake")
    );

    // Verify delegated worker lifecycle + checkpoint resume independent of cache expiry!
    let worker = service
        .delegates()
        .spawn(crate::bot::DelegateSpawnSpec {
            bot_id: bot_a.id.clone(),
            name: "build-worker".to_string(),
            definition: "general".to_string(),
            prompt: "Run cargo check".to_string(),
            worktrees: Vec::new(),
        })
        .await
        .expect("spawn delegate");

    wait_until(|| {
        let db = db.clone();
        let id = worker.agent_id.clone();
        async move {
            db.get_bot_delegate(&id)
                .await
                .ok()
                .flatten()
                .is_some_and(|w| w.status == BotDelegateStatus::Completed)
        }
    })
    .await;

    // Checkpoint is persisted in SQLite and can be resumed!
    assert!(
        db.get_bot_delegate_checkpoint(&worker.agent_id)
            .await
            .expect("get checkpoint")
            .is_some()
    );
    let resumed = service
        .delegates()
        .resume(
            &bot_a.id,
            &worker.agent_id,
            Some("Second pass after checkpoint".to_string()),
        )
        .await
        .expect("resume delegate");
    assert_eq!(resumed.agent_id, worker.agent_id);

    wait_until(|| {
        let db = db.clone();
        let id = worker.agent_id.clone();
        async move {
            db.list_bot_delegate_messages(&id, 10)
                .await
                .unwrap_or_default()
                .iter()
                .any(|m| m.content.contains("Second pass after checkpoint"))
        }
    })
    .await;

    // Verify all 4 gates: AutoApprove on bot_b unsafe, BotTriage + Escalate on bot_a ssh/workspace/publish!
    let pause = ApprovalPause::new();
    let auto_res = service
        .approvals()
        .request(
            &bot_b.id,
            "delegate",
            &worker.agent_id,
            BotApprovalGate::Unsafe,
            "sudo something",
            None,
            &pause,
            None,
        )
        .await
        .expect("auto approve");
    assert_eq!(auto_res, ApprovalOutcome::Approved);

    // Live policy update on bot_b (no restart!): switch unsafe from AutoApprove -> BotTriage.
    let updated_b = service
        .update_bot(
            &bot_b.id,
            UpdateBotRequest {
                name: None,
                persona: None,
                model: None,
                enabled: None,
                approval_policy: Some(BotApprovalPolicy {
                    unsafe_cmd: GatePolicy::BotTriage,
                    ssh: GatePolicy::BotTriage,
                    publish: GatePolicy::BotTriage,
                    workspace: GatePolicy::BotTriage,
                }),
            },
        )
        .await
        .expect("live policy update");
    assert_eq!(updated_b.approval_policy.unsafe_cmd, GatePolicy::BotTriage);

    // Test Escalate { note } on BotTriage: NO UserWait before Escalate, UserWait WITH note after Escalate!
    let router = service.approvals();
    let bot_a_id = bot_a.id.clone();
    let worker_id = worker.agent_id.clone();
    let req_task = tokio::spawn(async move {
        let p = ApprovalPause::new();
        router
            .request(
                &bot_a_id,
                "delegate",
                &worker_id,
                BotApprovalGate::Publish,
                "Open upstream PR",
                Some("Needs maintainer review"),
                &p,
                None,
            )
            .await
    });

    wait_until(|| {
        let db = db.clone();
        let id = bot_a.id.clone();
        async move {
            !db.list_bot_approvals(&id)
                .await
                .unwrap_or_default()
                .is_empty()
        }
    })
    .await;

    let pending = db
        .list_bot_approvals(&bot_a.id)
        .await
        .expect("list approvals");
    assert_eq!(pending[0].status, BotApprovalStatus::PendingBot);
    // Escalate with note:
    let escalated = service
        .decide_approval_by_bot(
            &bot_a.id,
            &pending[0].request_id,
            BotApprovalDecision::Escalate {
                note: "Publishing to external repo — please confirm".to_string(),
            },
        )
        .await
        .expect("escalate");
    assert_eq!(escalated.status, BotApprovalStatus::Escalated);
    assert_eq!(
        escalated.decision_note.as_deref(),
        Some("Publishing to external repo — please confirm")
    );

    // User approves escalated request -> unblocks parked worker!
    let resolved = service
        .resolve_approval_by_user(&bot_a.id, &pending[0].request_id, true, None)
        .await
        .expect("user approve");
    assert_eq!(resolved.status, BotApprovalStatus::Approved);
    let outcome = req_task.await.expect("join").expect("outcome");
    assert_eq!(outcome, ApprovalOutcome::Approved);
}

#[tokio::test]
async fn bot_memory_curator_warm_retry_cjk_provenance_and_dedup_rag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = ChatDb::open(&dir.path().join("history.db"))
        .await
        .expect("open db");
    let ui_bus = UiEventBus::new();

    // Scripted bot turn: calls `store_fact` citing the user inbox timestamp, then replies to user.
    let (llm, _) = ScriptedBotLlm::new(vec![]);
    let factory = Arc::new(PerBotModelFactory {
        models: Mutex::new(std::collections::HashMap::new()),
        default_model: llm,
    });
    let search_backend = MemoryCollections::default();
    let service = BotService::new(BotServiceDeps {
        db: db.clone(),
        may_dir: dir.path().to_path_buf(),
        model_factory: factory,
        curator_factory: Arc::new(ScriptedCuratorFactory {
            fail_first_invariant: true,
        }),
        delegate_runner: Arc::new(NoopDelegateRunner),
        search_backend: search_backend.clone(),
        sessions: Arc::new(NoopSessionBridge),
        runtime_service: None,
        notifications: None,
        sleep: None,
        ui_event_bus: ui_bus,
        publish_event: Arc::new(|_| {}),
    });

    let bot = service
        .create_bot(CreateBotRequest {
            name: "memory-bot".to_string(),
            persona: None,
            model: None,
            profile_id: None,
            enabled: true,
            approval_policy: BotApprovalPolicy::default(),
        })
        .await
        .expect("create bot");

    // Enqueue CJK user message into bot_inbox (covering Chinese, Japanese, Korean).
    let inbox_row = db
        .enqueue_bot_inbox(NewBotInboxEntry {
            bot_id: &bot.id,
            kind: BotInboxKind::UserMessage,
            sender_id: Some("user"),
            sender_name: Some("user"),
            body: "I love 麻辣烫, 寿司, and 비빔밥!",
            meta_json: None,
        })
        .await
        .expect("enqueue cjk");

    // 1. Fail fast on unknown timestamp!
    let paths = MayHomePaths::new(dir.path().to_path_buf()).bot_paths(&bot.id);
    let rag = crate::bot::BotMemoryRag::new(search_backend.clone());
    let curator = ScriptedCuratorFactory {
        fail_first_invariant: true,
    };
    let mut hashes = std::collections::HashMap::new();
    assert!(
        crate::bot::run_one_store_fact(
            &db,
            &bot.id,
            &bot.agent_id,
            &paths,
            "bogus fact",
            "1999-01-01T00:00:00Z",
            None,
            &curator,
            &rag,
            &mut hashes,
        )
        .await
        .is_err(),
        "unknown source_time must fail fast"
    );

    // 2. Valid source_time -> warm curator fails invariant on attempt 1, fixes on retry, commits to memory.git, publishes atomically, and indexes leaf in repo-search!
    let sha1 = crate::bot::run_one_store_fact(
        &db,
        &bot.id,
        &bot.agent_id,
        &paths,
        "User likes spicy Sichuan hotpot (麻辣烫), Japanese sushi (寿司), and Korean bibimbap (비빔밥)",
        &inbox_row.ts_utc,
        None,
        &curator,
        &rag,
        &mut hashes,
    )
    .await
    .expect("first curation succeeds after warm invariant retry");

    // Verify 1 commit in memory.git containing verbatim quote + source_agent + source_time!
    let commits = service
        .memory_history(&bot.id, 10)
        .await
        .expect("memory history");
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].sha, sha1);
    assert!(commits[0].message.contains(&inbox_row.ts_utc));
    assert!(commits[0].message.contains("麻辣烫, 寿司, and 비빔밥"));

    // Verify leaf-only recall(path) normalizes `preference/food` -> `preference/food.md` and rejects MEMORY.md/traversal!
    let (norm, content, _hash) = crate::bot::recall_leaf(&paths.memory_dir(), "preference/food")
        .await
        .expect("recall leaf");
    assert_eq!(norm, "preference/food.md");
    assert!(content.contains("비빔밥"));
    assert!(
        crate::bot::recall_leaf(&paths.memory_dir(), "MEMORY.md")
            .await
            .is_err()
    );
    assert!(
        crate::bot::recall_leaf(&paths.memory_dir(), "../escape")
            .await
            .is_err()
    );

    // Verify RAG deduplication:
    // - First query returns <memory-recall> without [UPDATED]
    // - Second query in same uncompacted window returns None (ZERO duplicates!)
    // - After a second store_fact updates the file's blake3 hash, query re-injects with [UPDATED]!
    // - After reset_seen_window (compaction), query re-injects afresh without [UPDATED]!
    let mut ledger = may_protocol::bot::SeenMemoryLedger::default();
    let hit1 = rag
        .recall_for_event(&bot.id, "寿司", &paths.memory_dir(), &mut ledger, 5)
        .await
        .expect("rag 1")
        .expect("must find leaf");
    assert!(hit1.contains("preference/food.md"));
    assert!(!hit1.contains("[UPDATED]"));

    let hit2 = rag
        .recall_for_event(&bot.id, "寿司", &paths.memory_dir(), &mut ledger, 5)
        .await
        .expect("rag 2");
    assert!(
        hit2.is_none(),
        "must NOT duplicate unchanged leaf in uncompacted window"
    );

    let inbox_row2 = db
        .enqueue_bot_inbox(NewBotInboxEntry {
            bot_id: &bot.id,
            kind: BotInboxKind::UserMessage,
            sender_id: Some("user"),
            sender_name: Some("user"),
            body: "Actually I prefer tonkotsu ramen (拉面 / ラーメン / 라면) now!",
            meta_json: None,
        })
        .await
        .expect("enqueue update");

    let curator_ok = ScriptedCuratorFactory {
        fail_first_invariant: false,
    };
    let _sha2 = crate::bot::run_one_store_fact(
        &db,
        &bot.id,
        &bot.agent_id,
        &paths,
        "Updated food preference to tonkotsu ramen (拉面 / ラーメン / 라면)",
        &inbox_row2.ts_utc,
        None,
        &curator_ok,
        &rag,
        &mut hashes,
    )
    .await
    .expect("second curation");

    let hit3 = rag
        .recall_for_event(&bot.id, "ramen", &paths.memory_dir(), &mut ledger, 5)
        .await
        .expect("rag 3")
        .expect("must re-inject updated leaf");
    assert!(
        hit3.contains("[UPDATED]"),
        "changed blake3 hash must re-inject with [UPDATED] marker: {hit3}"
    );

    crate::bot::reset_seen_window(&mut ledger);
    let hit4 = rag
        .recall_for_event(&bot.id, "ramen", &paths.memory_dir(), &mut ledger, 5)
        .await
        .expect("rag 4")
        .expect("post-compaction recall");
    assert!(
        !hit4.contains("[UPDATED]"),
        "post-compaction recall must start fresh without [UPDATED]"
    );
}

#[derive(Clone)]
struct MidRunInjectLlm {
    db: ChatDb,
    bot_id: Arc<Mutex<Option<String>>>,
    script: Arc<Mutex<VecDeque<Vec<Event>>>>,
    requests: Arc<Mutex<Vec<LLMRequest>>>,
    injected: Arc<std::sync::atomic::AtomicBool>,
}

impl LanguageModel for MidRunInjectLlm {
    type Error = TestLlmError;

    fn respond(
        &self,
        request: LLMRequest,
    ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
        self.requests.lock().expect("requests").push(request);
        let events = self
            .script
            .lock()
            .expect("script")
            .pop_front()
            .unwrap_or_else(|| vec![Event::Text("done".to_string())]);
        let db = self.db.clone();
        let bot_id = self.bot_id.lock().expect("bot_id").clone();
        let should_inject = !self
            .injected
            .swap(true, std::sync::atomic::Ordering::AcqRel);
        futures_lite::stream::unfold(
            (Some((should_inject, db, bot_id)), events.into_iter()),
            |(inject, mut iter)| async move {
                if let Some((true, db, Some(bid))) = inject {
                    let _ = db
                        .enqueue_bot_inbox(NewBotInboxEntry {
                            bot_id: &bid,
                            kind: BotInboxKind::AgentMessage,
                            sender_id: Some("peer-bot"),
                            sender_name: Some("peer-bot"),
                            body: "Mid-run interrupt (中文 / 日本語 / 한국어)",
                            meta_json: None,
                        })
                        .await;
                }
                let next = iter.next()?;
                Some((Ok(next), (None, iter)))
            },
        )
    }

    fn profile(&self) -> impl Future<Output = Result<Profile, Self::Error>> + Send {
        std::future::ready(Ok(Profile::new(
            "scripted",
            "test",
            "mid-run-bot",
            "mid-run test model",
            128_000,
        )))
    }
}

struct MidRunFactory(MidRunInjectLlm);
impl BotModelFactory<MidRunInjectLlm> for MidRunFactory {
    fn build_model(
        &self,
        _bot: &may_db::BotRow,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<(MidRunInjectLlm, Parameters)>> + Send + '_>>
    {
        let llm = self.0.clone();
        Box::pin(async move { Ok((llm, Parameters::default())) })
    }
}

#[tokio::test]
async fn bot_turn_boundary_inbox_drain_preserves_thinking_blocks_and_survives_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = ChatDb::open(&dir.path().join("history.db"))
        .await
        .expect("open db");
    let ui_bus = UiEventBus::new();

    let bot_id_slot = Arc::new(Mutex::new(None));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm = MidRunInjectLlm {
        db: db.clone(),
        bot_id: bot_id_slot.clone(),
        script: Arc::new(Mutex::new(VecDeque::from(vec![
            vec![
                Event::Reasoning("planning reply".to_string()),
                Event::ReasoningState(aither_core::llm::ReasoningState::new(
                    "claude",
                    "THINKING_SIGNATURE_KEEP_ME_INTACT",
                )),
                Event::ToolCall(ToolCall::new(
                    "call-send-1",
                    "send_message",
                    serde_json::json!({
                        "to": "user",
                        "message": "First reply"
                    }),
                )),
            ],
            vec![Event::Text("Handled mid-run interrupt cleanly".to_string())],
        ]))),
        requests: requests.clone(),
        injected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let service = BotService::new(BotServiceDeps {
        db: db.clone(),
        may_dir: dir.path().to_path_buf(),
        model_factory: Arc::new(MidRunFactory(llm)),
        curator_factory: Arc::new(ScriptedCuratorFactory {
            fail_first_invariant: false,
        }),
        delegate_runner: Arc::new(NoopDelegateRunner),
        search_backend: MemoryCollections::default(),
        sessions: Arc::new(NoopSessionBridge),
        runtime_service: None,
        notifications: None,
        sleep: None,
        ui_event_bus: ui_bus.clone(),
        publish_event: Arc::new(|_| {}),
    });

    let bot = service
        .create_bot(CreateBotRequest {
            name: "thinking-bot".to_string(),
            persona: None,
            model: None,
            profile_id: None,
            enabled: true,
            approval_policy: BotApprovalPolicy::default(),
        })
        .await
        .expect("create bot");
    *bot_id_slot.lock().expect("set bot_id") = Some(bot.id.clone());

    let _ = service
        .send_user_message(&bot.id, "Start long multi-step work")
        .await
        .expect("send user message");

    wait_until(|| {
        let svc = service.clone();
        let id = bot.id.clone();
        async move {
            svc.list_trace(&id, 50)
                .await
                .unwrap_or_default()
                .iter()
                .any(|t| t.content.contains("Handled mid-run interrupt cleanly"))
        }
    })
    .await;

    wait_until(|| {
        let db = db.clone();
        let id = bot.id.clone();
        async move {
            db.get_bot_context_snapshot(&id)
                .await
                .ok()
                .flatten()
                .is_some()
        }
    })
    .await;

    // Verify Turn 2 received BOTH the mid-run inbox entry AND the preserved Turn 1 reasoning block!
    let (turn_count, turn2_debug) = {
        let reqs = requests.lock().expect("requests");
        let len = reqs.len();
        let dbg = reqs
            .get(1)
            .map_or_default(|r| format!("{:?}", r.messages()));
        drop(reqs);
        (len, dbg)
    };
    assert!(turn_count >= 2, "expected at least 2 LLM turns");
    assert!(
        turn2_debug.contains("Mid-run interrupt"),
        "on_turn_boundary must drain newly arrived inbox entry into Turn 2: {turn2_debug}"
    );
    assert!(
        turn2_debug.contains("THINKING_SIGNATURE_KEEP_ME_INTACT"),
        "on_turn_boundary pause+resume must preserve thinking blocks intact: {turn2_debug}"
    );

    // Stop the old BotService loop and simulate a daemon restart with a brand new BotService!
    drop(service);
    let (llm_after_restart, requests_after_restart) = ScriptedBotLlm::new(vec![
        vec![Event::ToolCall(ToolCall::new(
            "call-after-restart",
            "send_message",
            serde_json::json!({
                "to": "user",
                "message": "Back after restart!"
            }),
        ))],
        vec![Event::Text("restarted turn complete".to_string())],
    ]);
    let factory_after = Arc::new(PerBotModelFactory {
        models: Mutex::new(std::collections::HashMap::new()),
        default_model: llm_after_restart,
    });
    let restarted = BotService::new(BotServiceDeps {
        db: db.clone(),
        may_dir: dir.path().to_path_buf(),
        model_factory: factory_after,
        curator_factory: Arc::new(ScriptedCuratorFactory {
            fail_first_invariant: false,
        }),
        delegate_runner: Arc::new(NoopDelegateRunner),
        search_backend: MemoryCollections::default(),
        sessions: Arc::new(NoopSessionBridge),
        runtime_service: None,
        notifications: None,
        sleep: None,
        ui_event_bus: ui_bus,
        publish_event: Arc::new(|_| {}),
    });
    restarted
        .start_all()
        .await
        .expect("start_all after restart");
    let _ = restarted
        .send_user_message(&bot.id, "Are you still remembering our prior turn?")
        .await
        .expect("post restart message");

    wait_until(|| {
        let svc = restarted.clone();
        let id = bot.id.clone();
        async move {
            svc.list_messages(&id, 50)
                .await
                .unwrap_or_default()
                .iter()
                .any(|m| m.content == "Back after restart!")
        }
    })
    .await;

    let restored_debug = {
        let reqs_after = requests_after_restart.lock().expect("requests after");
        let dbg = reqs_after
            .first()
            .map_or_default(|r| format!("{:?}", r.messages()));
        drop(reqs_after);
        dbg
    };
    assert!(
        restored_debug.contains("Start long multi-step work"),
        "restarted BotService must restore persisted context snapshot from history.db: {restored_debug}"
    );
}
