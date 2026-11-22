//! Agent-runtime hooks wired onto [`super::may::MayAgent`].
//!
//! [`MayHooks`] is the one [`Hooks`] value an agent is built with. Its parts:
//!
//! - the pending-steer check ends the current agent turn the moment the
//!   persisted session-runtime state holds a queued "immediate" user message,
//!   so steering takes effect without waiting for the model to finish the
//!   current reasoning loop; it is a no-op when the agent was created without
//!   a live [`SessionRuntimeService`] (for example in exec mode);
//! - secret redaction replaces stored secret values in every tool result
//!   with the secret's name before the agent commits it, so the session
//!   context, the persisted history and the published events only ever carry
//!   the name;
//! - the park rule ends a run whose turn only re-read unchanged output while
//!   background commands still run: another request would read the same
//!   output again, and only a background event can say something new;
//! - loop detection feeds every tool turn to the agent's [`LoopWatch`] and
//!   ends the run once a loop goes on past its alert.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aither_agent::{
    AfterTool, Hooks, ToolDecision, ToolInvocation, ToolOutcome, TurnBoundaryAction,
    TurnBoundaryContext,
};
use aither_sandbox::{Interpreter, JobStatus, Jobs, Refused, UnchangedRead};

use super::loop_watch::LoopWatch;
use crate::command_policy::RefusedReads;
use crate::harness::compact_tool::{COMPACT_TOOL_NAME, CompactSignal};
use crate::ipc::CallImageStore;
use crate::secrets::SecretRedactor;
use crate::session::{SessionId, SessionRuntimeService};
use crate::vision_ocr::VisionOcr;

/// The interpreter each `terminal` call's script runs under, keyed by call
/// so an approval request for the same call renders the script in its
/// language. Every clone shares the record.
#[derive(Debug, Clone, Default)]
pub struct CallInterpreters(
    std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<aither_core::llm::tool::CallIdentity, Interpreter>,
        >,
    >,
);

impl CallInterpreters {
    /// Record the interpreter a `terminal` call's script runs under.
    fn record(&self, call: aither_core::llm::tool::CallIdentity, interpreter: Interpreter) {
        self.0
            .lock()
            .expect("the interpreter record is never held across a panic")
            .insert(call, interpreter);
    }

    /// The interpreter recorded for `call`, when any.
    ///
    /// # Panics
    /// Panics when the record's lock is poisoned.
    #[must_use]
    pub fn get(&self, call: &aither_core::llm::tool::CallIdentity) -> Option<Interpreter> {
        self.0
            .lock()
            .expect("the interpreter record is never held across a panic")
            .get(call)
            .copied()
    }
}

/// What a May agent decides at the loop's control points.
#[derive(Debug, Clone)]
pub struct MayHooks {
    /// The session's persisted runtime, where a waiting steer message shows.
    pub(super) runtime: Option<Arc<SessionRuntimeService>>,
    pub(super) session_id: SessionId,
    pub(super) redactor: SecretRedactor,
    pub(super) refused: RefusedReads,
    /// The interpreter each `terminal` call runs under, for the approver's
    /// prompt to highlight its script.
    pub(super) interpreters: CallInterpreters,
    pub(super) jobs: Jobs,
    /// The `compact` tool's valve: a request it holds ends the turn.
    pub(super) valve: Arc<CompactSignal>,
    /// Set when the valve ended a turn for a compaction; the runner takes it.
    pub(super) compaction: Arc<AtomicBool>,
    /// The loop detection every tool turn is fed to.
    pub(super) loops: LoopWatch,
    /// Native on-device OCR + conditional downscaling for command-returned images.
    pub(super) vision_ocr: VisionOcr,
    /// Per-tool-call store collecting images produced by IPC subcommands.
    pub(super) call_images: CallImageStore,
    /// Session output directory where downscaled image files are persisted.
    pub(super) outputs_dir: PathBuf,
}

impl MayHooks {
    /// Whether the persisted runtime holds a user message waiting to steer
    /// the conversation.
    async fn steer_pending(&self) -> bool {
        let Some(service) = &self.runtime else {
            return false;
        };
        let Ok(runtime) = service
            .db()
            .get_session_runtime(self.session_id.as_str())
            .await
        else {
            tracing::warn!(
                session_id = %self.session_id,
                "failed to inspect runtime for steer boundary"
            );
            return false;
        };
        runtime.is_some_and(|summary| summary.has_pending_immediate())
    }

    /// Whether every call of the turn re-read unchanged output.
    ///
    /// Every outcome is asked, so no refused call stays on the record.
    fn only_unchanged_reads(&self, outcomes: &[ToolOutcome]) -> bool {
        outcomes
            .iter()
            .map(|outcome| {
                let refused = self.refused.take(outcome.call.call());
                let polled =
                    outcome.result.as_ref().err().is_some_and(|error| {
                        error.downcast_ref::<Refused<UnchangedRead>>().is_some()
                    });
                refused || polled
            })
            .fold(!outcomes.is_empty(), |all, unchanged| all && unchanged)
    }
}

impl Hooks for MayHooks {
    fn before_tool(
        &self,
        call: &ToolInvocation<'_>,
    ) -> impl std::future::Future<Output = ToolDecision> + Send {
        if call.name == "terminal"
            && let Some(interpreter) = call
                .arguments
                .get("interpreter")
                .and_then(|value| serde_json::from_value::<Interpreter>(value.clone()).ok())
        {
            self.interpreters.record(call.call.clone(), interpreter);
        }
        std::future::ready(ToolDecision::Allow)
    }

    async fn after_tool(&self, outcome: &ToolOutcome) -> AfterTool {
        if outcome.name != COMPACT_TOOL_NAME {
            self.valve.acted();
        }
        // A tool result is never shown to the model unredacted: when the
        // stored secrets cannot be read, the run ends with that error.
        let redactions = match self.redactor.current().await {
            Ok(redactions) => redactions,
            Err(error) => return AfterTool::Abort(error.to_string()),
        };
        let shown = outcome.for_model();
        let mut result = shown.clone();
        redactions.redact_tool_result(&mut result);
        let redacted = result != shown;
        if let Some((mut augmented, attachments)) = crate::vision_ocr::augment_tool_outcome(
            &self.vision_ocr,
            Some(&self.call_images),
            Some(self.outputs_dir.as_path()),
            outcome.call.call(),
            &outcome.name,
            result.clone(),
        )
        .await
        {
            redactions.redact_tool_result(&mut augmented);
            return AfterTool::ReplaceWithAttachments {
                result: augmented,
                attachments,
            };
        }
        if redacted {
            AfterTool::Replace(result)
        } else {
            AfterTool::Keep
        }
    }

    async fn on_turn_boundary(&self, cx: &TurnBoundaryContext<'_>) -> TurnBoundaryAction {
        // Every tool turn is counted, whichever rule ends the run.
        let looped = self.loops.observe_turn(cx.outcomes);
        // Take the request before steering can end the turn: a request left
        // pending across an early return would sit in `REQUESTED` forever.
        let compact = self.valve.take_request();
        if compact {
            self.compaction.store(true, Ordering::Release);
            return TurnBoundaryAction::EndTurn;
        }
        if self.steer_pending().await {
            return TurnBoundaryAction::EndTurn;
        }
        if self.only_unchanged_reads(cx.outcomes)
            && self
                .jobs
                .list()
                .await
                .iter()
                .any(|job| job.status == JobStatus::Running)
        {
            tracing::info!(
                turn = cx.turn,
                "every call re-read unchanged output; parking the run until background work reports"
            );
            return TurnBoundaryAction::EndTurn;
        }
        if looped {
            tracing::info!(
                session_id = %self.session_id,
                turn = cx.turn,
                "the agent kept looping after its alert; ending the run"
            );
            return TurnBoundaryAction::EndTurn;
        }
        TurnBoundaryAction::Continue
    }
}

#[cfg(all(test, feature = "vision-ocr", target_os = "macos"))]
mod tests {
    use std::borrow::Cow;
    use std::collections::VecDeque;
    use std::ops::ControlFlow;
    use std::sync::Mutex;

    use aither_agent::Agent;
    use aither_core::llm::model::Profile;
    use aither_core::llm::{
        Classify, Event, Failure, LLMRequest, LanguageModel, Role, Tool, ToolCall, ToolContext,
        ToolResult,
    };
    use futures_lite::Stream;
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;
    use crate::ipc::RecordedCallImage;
    use crate::test_sandbox::TestTerminal;

    #[derive(Debug)]
    struct StubErr;
    impl std::fmt::Display for StubErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("stub error")
        }
    }
    impl std::error::Error for StubErr {}
    impl Classify for StubErr {
        fn failure(&self) -> Failure {
            Failure::permanent()
        }
    }

    #[derive(Clone)]
    struct ScriptedModel {
        script: Arc<Mutex<VecDeque<Vec<Event>>>>,
        requests: Arc<Mutex<Vec<LLMRequest>>>,
    }

    impl LanguageModel for ScriptedModel {
        type Error = StubErr;

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
                .expect("scripted turn");
            futures_lite::stream::iter(events.into_iter().map(Ok))
        }

        fn profile(
            &self,
        ) -> impl std::future::Future<Output = Result<Profile, Self::Error>> + Send {
            std::future::ready(Ok(Profile::new(
                "scripted",
                "test",
                "scripted",
                "scripted model",
                128_000,
            )))
        }
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct NoArgs {}

    struct ScreenshotTool {
        call_images: CallImageStore,
        image: RecordedCallImage,
    }

    impl Tool for ScreenshotTool {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("terminal")
        }

        fn description(&self) -> Cow<'static, str> {
            Cow::Borrowed("Captures a test screenshot.")
        }

        type Arguments = NoArgs;
        type Res = ToolResult;

        fn call(
            &self,
            _args: Self::Arguments,
            cx: ToolContext,
        ) -> impl std::future::Future<Output = aither_core::Result<Self::Res>> + Send {
            if let Some(call) = cx.identity() {
                self.call_images
                    .record(call.call(), vec![self.image.clone()]);
            }
            std::future::ready(Ok(ToolResult::text("Captured screen")))
        }
    }

    #[tokio::test]
    async fn after_tool_attaches_ocr_and_redacts_secrets_from_recognized_text() {
        crate::test_support::install_global_executor();
        let dir = tempfile::tempdir().expect("tempdir");
        let terminal = TestTerminal::new(dir.path()).await;
        let screenshot_path = dir.path().join("screen.png");
        let png = may_media::test_support::render_multilingual_text_png(
            &[
                "Deployment token leak test hunter2-secret-value",
                "中文状态：运行正常",
                "日本語ログ：完了",
                "한국어 상태: 정상",
            ],
            1200,
            600,
        );
        async_fs::write(&screenshot_path, &png)
            .await
            .expect("write png");

        let call_images = CallImageStore::default();
        let hooks = MayHooks {
            runtime: None,
            session_id: SessionId::from_string("ocr-session"),
            redactor: SecretRedactor::fixed(&[("DEPLOY_TOKEN", "hunter2-secret-value")]),
            refused: RefusedReads::default(),
            interpreters: CallInterpreters::default(),
            jobs: terminal.shell().jobs(),
            valve: Arc::new(CompactSignal::default()),
            compaction: Arc::new(AtomicBool::new(false)),
            loops: LoopWatch::new(crate::agent::LoopLimits::default()),
            vision_ocr: VisionOcr::new(),
            call_images: call_images.clone(),
            outputs_dir: dir.path().join("outputs"),
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = ScriptedModel {
            script: Arc::new(Mutex::new(VecDeque::from([
                vec![Event::ToolCall(ToolCall::new(
                    "call-may-ocr",
                    "terminal",
                    serde_json::json!({}),
                ))],
                vec![Event::Text("done".to_owned())],
            ]))),
            requests: Arc::clone(&requests),
        };
        let mut agent = Agent::builder(llm)
            .tool(ScreenshotTool {
                call_images,
                image: RecordedCallImage {
                    path: screenshot_path,
                    media_type: "image/png".to_owned(),
                    source: "computer screenshot".to_owned(),
                },
            })
            .hooks(hooks)
            .build();

        let _ = agent
            .run("capture", std::iter::empty())
            .drive((), std::future::pending::<()>(), |(), _| async {
                ControlFlow::Continue(())
            })
            .await
            .expect("agent completes");

        let tool_message = {
            let captured = requests.lock().expect("requests");
            captured[1]
                .messages()
                .iter()
                .find(|msg| msg.role() == Role::Tool && msg.tool_call_id() == Some("call-may-ocr"))
                .expect("tool message reaches model")
                .clone()
        };
        assert_eq!(tool_message.attachments().len(), 1);
        let rendered = tool_message.content();
        assert!(rendered.contains("Captured screen"), "{rendered}");
        assert!(rendered.contains("<recognized_text"), "{rendered}");
        assert!(
            rendered.contains("DEPLOY_TOKEN"),
            "redacted token name expected: {rendered}"
        );
        assert!(
            !rendered.contains("hunter2-secret-value"),
            "OCR text must also be redacted: {rendered}"
        );
    }
}
