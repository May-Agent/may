//! The `wake` command a scheduled task's run uses to prompt the session that
//! created the task.
//!
//! The prompt takes the path a message from another agent takes: it is stored
//! in the session's pending-message queue first, so a busy or restarting
//! session does not lose it, and the daemon is then nudged to dispatch what is
//! stored. An idle session starts a turn with it; a busy one receives it after
//! its current turn. Either way the session continues with its own agent.

use std::path::PathBuf;
use std::sync::Arc;

use aither_core::llm::{Tool, ToolContext, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::agent_link::{AgentLinkError, AttributeText, UntrustedText, resolve_prompt_path};
use crate::db::ChatDbError;
use crate::session::{
    PendingMessageState, PendingUserMessage, PendingUserMessageMode, SessionRuntimeService,
};
use crate::xml::{XmlBlock, XmlBlockError};

/// Failure of a task's `wake`.
#[derive(Debug, thiserror::Error)]
pub enum TaskWakeError {
    /// The task was created through the API or the web UI.
    #[error(
        "task \"{0}\" was not created by a session, so there is no session to prompt; `wake` only works in tasks an agent created"
    )]
    NoSession(String),
    /// The runtime serving this run has no session queue.
    #[error("wake is unavailable in this runtime")]
    Unavailable,
    /// The prompt file is empty.
    #[error("the prompt in {0} is empty")]
    EmptyPrompt(PathBuf),
    /// The prompt file could not be located or read.
    #[error(transparent)]
    PromptPath(#[from] AgentLinkError),
    /// The prompt file could not be read.
    #[error("failed to read prompt file {path}: {source}")]
    Read {
        /// File path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Storage failed.
    #[error(transparent)]
    Db(#[from] ChatDbError),
    /// The delivery text could not be wrapped as hidden context.
    #[error(transparent)]
    Context(#[from] XmlBlockError),
}

/// The queue of the sessions this daemon hosts, as a task's run reaches it.
#[derive(Clone)]
pub struct SessionPrompts {
    runtime_service: Arc<SessionRuntimeService>,
    /// Receives the id of each session with newly stored work; the daemon
    /// dispatches it.
    wake: async_channel::Sender<String>,
}

impl std::fmt::Debug for SessionPrompts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPrompts").finish_non_exhaustive()
    }
}

#[derive(Template)]
#[template(path = "task_prompt.txt", escape = "none")]
struct TaskPromptText<'a> {
    task_id: AttributeText<'a>,
    task_name: AttributeText<'a>,
    body: UntrustedText<'a>,
}

#[derive(Template)]
#[template(path = "task_wake_sent.txt", escape = "none")]
struct TaskWakeSentText<'a> {
    task_name: &'a str,
}

impl SessionPrompts {
    /// Prompts stored in `runtime_service`'s queues; each woken session id is
    /// sent on `wake`.
    #[must_use]
    pub const fn new(
        runtime_service: Arc<SessionRuntimeService>,
        wake: async_channel::Sender<String>,
    ) -> Self {
        Self {
            runtime_service,
            wake,
        }
    }

    /// Queue `body` for `session_id` as a message from task `task_id`, behind
    /// whatever the session is running, and wake it.
    ///
    /// # Errors
    /// [`TaskWakeError`] on storage failure or unusable text.
    ///
    /// # Panics
    /// When the compile-time message template fails to render.
    pub async fn deliver(
        &self,
        session_id: &str,
        task_id: &str,
        task_name: &str,
        body: &str,
    ) -> Result<(), TaskWakeError> {
        let word = self
            .runtime_service
            .db()
            .alias::<crate::model_ids::TaskAlias>(task_id)
            .await?;
        let text = TaskPromptText {
            task_id: AttributeText(&word),
            task_name: AttributeText(task_name),
            body: UntrustedText(body),
        }
        .render()
        .expect("task prompt template must render");
        let message = PendingUserMessage {
            id: None,
            state: PendingMessageState::default(),
            error: None,
            content: String::new(),
            attachments: Vec::new(),
            hidden_context_xml: Some(XmlBlock::parse(text)?),
            created_at: jiff::Timestamp::now().to_string(),
            delivery_mode: PendingUserMessageMode::Queue,
        };
        self.runtime_service
            .enqueue(session_id, message, PendingUserMessageMode::Queue)
            .await?;
        // The receiver lives as long as the daemon; a closed channel means it
        // is shutting down and the stored prompt is dispatched on restart.
        let _ = self.wake.try_send(session_id.to_string());
        Ok(())
    }
}

/// Prompt the session that created this task with the text of a file.
///
/// Usage: `wake <prompt-file>`. The file is read when the command runs, so the
/// script decides what the prompt holds, the trigger payload
/// (`$MAY_TASK_TRIGGER_FILE`) included. The prompt reaches the session as a
/// message from the task: it starts a turn when the session is idle and
/// arrives after the current turn when it is busy.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WakeArgs {
    /// File holding the prompt, as the task's shell spells its path.
    pub prompt_file: String,
}

/// The `wake` command of one task run.
#[derive(Debug, Clone)]
pub struct WakeTool {
    prompts: Option<SessionPrompts>,
    task_id: String,
    task_name: String,
    /// Session that created the task; `None` when it was created through the
    /// API or the web UI.
    session_id: Option<String>,
    /// Host directory the run's shell works in.
    work_dir: PathBuf,
    /// Where `work_dir` appears inside the task's container, if it runs in one.
    workspace_mount: Option<aither_sandbox::MountRoot>,
}

impl WakeTool {
    /// The command for a run of task `task_id`.
    #[must_use]
    pub const fn new(
        prompts: Option<SessionPrompts>,
        task_id: String,
        task_name: String,
        session_id: Option<String>,
        work_dir: PathBuf,
        workspace_mount: Option<aither_sandbox::MountRoot>,
    ) -> Self {
        Self {
            prompts,
            task_id,
            task_name,
            session_id,
            work_dir,
            workspace_mount,
        }
    }

    async fn wake(&self, prompt_file: &str) -> Result<(), TaskWakeError> {
        let session_id = self
            .session_id
            .as_deref()
            .ok_or_else(|| TaskWakeError::NoSession(self.task_name.clone()))?;
        let prompts = self.prompts.as_ref().ok_or(TaskWakeError::Unavailable)?;
        let path = resolve_prompt_path(&self.work_dir, self.workspace_mount.as_ref(), prompt_file)?;
        let body = async_fs::read_to_string(&path)
            .await
            .map_err(|source| TaskWakeError::Read {
                path: path.clone(),
                source,
            })?;
        if body.trim().is_empty() {
            return Err(TaskWakeError::EmptyPrompt(path));
        }
        prompts
            .deliver(session_id, &self.task_id, &self.task_name, body.trim())
            .await
    }
}

impl Tool for WakeTool {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("wake")
    }

    type Arguments = WakeArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> aither_core::Result<Self::Res> {
        self.wake(&args.prompt_file).await?;
        Ok(ToolResult::text(
            TaskWakeSentText {
                task_name: &self.task_name,
            }
            .render()
            .expect("task wake sent template must render"),
        ))
    }
}
