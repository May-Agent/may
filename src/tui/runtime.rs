use aither_core::llm::model::ReasoningEffort;
use executor_core::spawn;

use crate::agent::{AgentHandle, AgentWorker};
use crate::ask_user::QuestionAnswer;
use crate::session::AttachmentKind;
use crate::tui::{App, Attachment, PendingMessage};
use crate::{ChatMessage, MayConfig, SessionId};

/// Shown when a cloud session opens while its codespace is suspended.
const SUSPENDED_NOTICE: &str = "This session's codespace is suspended. Send a message to wake it; its history appears once it is running.";

/// Error type for TUI runtime operations.
#[derive(Debug, thiserror::Error)]
pub enum TuiRuntimeError {
    /// Database error.
    #[error("database error: {0}")]
    Database(#[from] crate::db::ChatDbError),
    /// Session error.
    #[error("session error: {0}")]
    Session(#[from] crate::session::SessionError),
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Agent environment resolution failed.
    #[error("agent environment resolution failed: {0}")]
    ResolveAgentEnvironment(#[from] crate::agent::ResolveAgentEnvironmentError),
    /// No active session.
    #[error("No active session")]
    NoActiveSession,
    /// A turn was started without holding the session's run lease.
    #[error("no session run lease is held for this turn")]
    NoRunLease,
    /// The May daemon refused or could not be reached.
    #[error(transparent)]
    Daemon(#[from] crate::client::DaemonClientError),
    /// The May daemon is not attached yet.
    #[error("the May daemon is not attached")]
    DaemonUnavailable,
    /// History replay failed.
    #[error("history replay failed: {0}")]
    HistoryReplay(#[from] crate::session::HistoryReplayError),
    /// Foreign transcript import failed.
    #[error("import failed: {0}")]
    Import(#[from] crate::session::import::ImportError),
    /// Invalid pending message timestamp.
    #[error("invalid pending message timestamp: {0}")]
    InvalidPendingMessageTimestamp(#[source] jiff::Error),
    /// Persisted image attachment is missing content.
    #[error("persisted image attachment is missing content")]
    PersistedImageMissingContent,
    /// Unsupported persisted attachment type in TUI runtime.
    #[error("unsupported persisted attachment type in TUI runtime: {0}")]
    UnsupportedPersistedAttachmentType(AttachmentKind),
    /// Failed to convert path into file URL.
    #[error("failed to convert path into file URL: {path}")]
    FileUrlConversion {
        /// Path that could not be converted.
        path: std::path::PathBuf,
    },
    /// Failed to parse attachment URL.
    #[error("failed to parse attachment URL {url}: {source}")]
    AttachmentUrlParse {
        /// URL that failed to parse.
        url: String,
        /// Underlying URL parse error.
        #[source]
        source: url::ParseError,
    },
    /// Attachment URL is not a local file URL.
    #[error("attachment URL is not a local file URL: {url}")]
    NonLocalAttachmentUrl {
        /// URL that is not a local `file://` URL.
        url: String,
    },
    /// An answer to an open question was refused.
    #[error("{0}")]
    Answer(#[from] crate::session::AnswerError),
    /// Selected model is empty.
    #[error("Selected model is empty")]
    EmptySelectedModel,
    /// No model configured.
    #[error("No model configured")]
    NoModelConfigured,
    /// Location fetch failed.
    #[error("Location fetch failed: {0}")]
    LocationFetch(#[source] crate::location::LocationError),
    /// Agent initialization failed.
    #[error("agent initialization failed: {0}")]
    MayAgent(#[from] crate::agent::MayAgentError),
    /// Agent operation failed.
    #[error("agent operation failed: {0}")]
    Agent(#[from] aither_agent::AgentError),
    /// Failed to convert attachment path to URL.
    #[error("Failed to convert attachment path to URL: {path}")]
    AttachmentPathToUrl {
        /// Path that could not be converted.
        path: std::path::PathBuf,
    },
    /// Unable to determine attachment MIME type.
    #[error("Unable to determine attachment MIME type: {path}")]
    UnknownAttachmentMediaType {
        /// Path whose MIME type could not be determined.
        path: std::path::PathBuf,
    },
    /// Invalid data URL.
    #[error("invalid data URL")]
    InvalidDataUrl,
    /// Failed to decode data URL.
    #[error("failed to decode data URL: {0}")]
    DecodeDataUrl(#[source] base64::DecodeError),
}

/// Whether the caller committed its response to a prompt a run is blocked on.
///
/// Only the committer wakes the run with its own response. When another
/// process sharing the database committed first, it finds no waiter
/// in its own registry and takes the resume path, so this process answers
/// the live run with what was committed (`R`), read back by request id, and
/// the user is told their own response was not used.
#[derive(Debug, Clone)]
#[must_use]
pub enum ResponseCommit<R> {
    /// This call committed the response; the caller wakes the run with it.
    Committed,
    /// Another response was committed first; the caller answers the run with
    /// it.
    Lost(R),
    /// The prompt was withdrawn and nothing was committed; the run fails.
    Withdrawn,
}

/// Read the daemon's answer to `POST /api/ask-user/respond`: `false` when the
/// question is no longer open, an error for every other failure.
fn committed_or_not_open<T>(
    result: Result<T, crate::client::DaemonClientError>,
) -> Result<bool, TuiRuntimeError> {
    match result {
        Ok(_) => Ok(true),
        Err(crate::client::DaemonClientError::Status { status, .. })
            if status == crate::daemon::ASK_USER_NOT_OPEN =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod ask_user_commit_tests {
    use super::*;
    use crate::client::DaemonClientError;

    fn refused(status: hyper::StatusCode, message: &str) -> Result<(), DaemonClientError> {
        Err(DaemonClientError::Status {
            path: "/api/ask-user/respond".to_string(),
            status,
            message: message.to_string(),
        })
    }

    #[test]
    fn an_answer_the_daemon_took_is_committed() {
        assert!(committed_or_not_open(Ok(())).expect("committed"));
    }

    #[test]
    fn a_question_that_is_not_open_is_not_an_error() {
        let result = committed_or_not_open(refused(
            crate::daemon::ASK_USER_NOT_OPEN,
            "ask_user request not found or withdrawn",
        ));
        assert!(!result.expect("not open is not a failure"));
    }

    #[test]
    fn a_storage_failure_is_an_error_naming_it() {
        let error = committed_or_not_open(refused(
            hyper::StatusCode::INTERNAL_SERVER_ERROR,
            "failed to store secret deploy_token: keychain error",
        ))
        .expect_err("a storage failure is not 'not open'");
        assert!(error.to_string().contains("keychain error"), "{error}");
    }
}

/// Answers as `POST /api/ask-user/respond` takes them. A skipped answer is
/// the user declining to give a secret.
fn answers_json(answers: &[QuestionAnswer]) -> serde_json::Value {
    use crate::ask_user::AnswerValue;
    serde_json::Value::Array(
        answers
            .iter()
            .map(|answer| {
                let value = match &answer.answer {
                    AnswerValue::Selected(values) => {
                        serde_json::json!({ "kind": "selected", "values": values })
                    }
                    AnswerValue::Custom(text) => {
                        serde_json::json!({ "kind": "custom", "text": text })
                    }
                    AnswerValue::Skipped => serde_json::json!({ "kind": "declined" }),
                };
                serde_json::json!({ "section": answer.section, "answer": value })
            })
            .collect(),
    )
}

async fn persisted_attachment_from_tui(
    client: &crate::client::DaemonClient,
    session_id: &str,
    attachment: &Attachment,
) -> Result<crate::session::PersistedAttachment, TuiRuntimeError> {
    let id = crate::id::new();
    match attachment {
        Attachment::Image { path, data } => {
            let name = file_name_or_default(path, "image.png");
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let record: crate::session::PersistedAttachment = client
                .post_raw(
                    &format!(
                        "/api/sessions/{session_id}/attachments?name={}",
                        urlencoding(&name)
                    ),
                    data.clone(),
                    mime.as_ref(),
                )
                .await?;
            Ok(crate::session::PersistedAttachment {
                id,
                name,
                attachment_type: AttachmentKind::Image,
                url: record.url,
                text_content: None,
            })
        }
        Attachment::FileReference { path } => Ok(crate::session::PersistedAttachment {
            id,
            name: file_name_or_default(path, "file"),
            attachment_type: AttachmentKind::File,
            url: file_url(path)?,
            text_content: None,
        }),
        Attachment::PastedText {
            content,
            saved_path,
        } => Ok(crate::session::PersistedAttachment {
            id,
            name: saved_path.as_ref().map_or_else(
                || "pasted.txt".to_string(),
                |path| file_name_or_default(path, "pasted.txt"),
            ),
            attachment_type: AttachmentKind::Text,
            url: saved_path
                .as_ref()
                .map(|path| file_url(path))
                .transpose()?
                .unwrap_or_else(|| "may://inline-text".to_string()),
            text_content: Some(content.clone()),
        }),
    }
}

pub(super) async fn tui_attachment_from_persisted(
    client: Option<&crate::client::DaemonClient>,
    attachment: crate::session::PersistedAttachment,
) -> Result<Attachment, TuiRuntimeError> {
    match attachment.attachment_type {
        AttachmentKind::Image => {
            let display_path = path_from_attachment_url(&attachment.url)
                .unwrap_or_else(|_| std::path::PathBuf::from(&attachment.name));
            let data = match path_from_attachment_url(&attachment.url) {
                Ok(path) if async_fs::metadata(&path).await.is_ok() => {
                    async_fs::read(&path).await?
                }
                _ if attachment.url.starts_with("/api/sessions/") => {
                    client
                        .ok_or(TuiRuntimeError::PersistedImageMissingContent)?
                        .get_bytes(&attachment.url)
                        .await?
                }
                _ => return Err(TuiRuntimeError::PersistedImageMissingContent),
            };
            Ok(Attachment::Image {
                path: display_path,
                data,
            })
        }
        AttachmentKind::File => Ok(Attachment::FileReference {
            path: path_from_attachment_url(&attachment.url)?,
        }),
        AttachmentKind::Text => Ok(Attachment::PastedText {
            content: if let Some(content) = attachment.text_content {
                content
            } else {
                let path = path_from_attachment_url(&attachment.url)?;
                async_fs::read_to_string(path).await?
            },
            saved_path: if attachment.url.starts_with("file://") {
                Some(path_from_attachment_url(&attachment.url)?)
            } else {
                None
            },
        }),
        other => Err(TuiRuntimeError::UnsupportedPersistedAttachmentType(other)),
    }
}

fn file_url(path: &std::path::Path) -> Result<String, TuiRuntimeError> {
    url::Url::from_file_path(path)
        .map_err(|()| TuiRuntimeError::FileUrlConversion {
            path: path.to_path_buf(),
        })
        .map(|url| url.to_string())
}

fn path_from_attachment_url(url: &str) -> Result<std::path::PathBuf, TuiRuntimeError> {
    url::Url::parse(url)
        .map_err(|source| TuiRuntimeError::AttachmentUrlParse {
            url: url.to_string(),
            source,
        })?
        .to_file_path()
        .map_err(|()| TuiRuntimeError::NonLocalAttachmentUrl {
            url: url.to_string(),
        })
}

fn file_name_or_default(path: &std::path::Path, default_name: &str) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| default_name.to_string(), ToOwned::to_owned)
}

fn urlencoding(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Result of starting the next queued turn.
#[derive(Debug)]
pub enum QueuedRun {
    /// No message is dispatchable; the session was left untouched.
    Nothing,
    /// Another run holds the session; the queue was left untouched.
    Busy,
    /// The lease is held and the message was taken for the turn.
    Started(PendingMessage, Box<crate::session::SessionRunGuard>),
}

/// Outcome of [`RuntimeDeps::begin_queued_run`].
#[derive(Debug)]
pub enum QueuedRunStart {
    /// No message is dispatchable.
    Nothing,
    /// Another run holds the session; the queue was left untouched.
    Busy,
    /// The app holds the lease and the message is to run.
    Started(PendingMessage),
}

/// Runtime dependencies shared by session and agent operations.
#[derive(Debug, Clone)]
pub struct RuntimeDeps {
    tool_environments: crate::agent::ToolEnvironmentCache,
    oneshot_model: Option<String>,
    oneshot_reasoning_effort: Option<ReasoningEffort>,
    /// The May daemon that runs this TUI's agents, when it is a client.
    daemon: Option<crate::client::DaemonClient>,
    /// The session's turns belong to this process, a one-shot run: the daemon
    /// stops a turn whose client disconnects.
    owns_turns: bool,
}

/// An agent worker under construction off the event loop.
///
/// Construction prepares the session's tool environments, which may ask the
/// user for SSH consent, so the event loop awaits [`Self::wait`] alongside its
/// ask-user queue instead of blocking on it. Dropping the build cancels it.
#[derive(Debug)]
pub struct PendingWorkerBuild {
    model: String,
    settings: crate::agent::AgentLiveSettings,
    task: std::pin::Pin<Box<executor_core::AnyExecutorTask<Result<AgentWorker, TuiRuntimeError>>>>,
}

impl PendingWorkerBuild {
    /// Model the worker is being built for.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Config values the worker is built from.
    #[must_use]
    pub const fn settings(&self) -> &crate::agent::AgentLiveSettings {
        &self.settings
    }

    /// Waits for the worker; cancellation-safe, so it can be raced in a select.
    ///
    /// # Errors
    /// Returns the agent construction failure.
    pub async fn wait(&mut self) -> Result<AgentWorker, TuiRuntimeError> {
        self.task.as_mut().await
    }
}

impl RuntimeDeps {
    /// Create the dependency bundle shared by every agent worker the runtime
    /// spawns.
    #[must_use]
    pub const fn new(tool_environments: crate::agent::ToolEnvironmentCache) -> Self {
        Self {
            tool_environments,
            oneshot_model: None,
            oneshot_reasoning_effort: None,
            daemon: None,
            owns_turns: false,
        }
    }

    /// Run every agent in the daemon behind `client` instead of in-process.
    #[must_use]
    pub fn with_daemon(mut self, client: crate::client::DaemonClient) -> Self {
        self.daemon = Some(client);
        self
    }

    /// The daemon this TUI is a client of, if any.
    #[must_use]
    pub const fn daemon(&self) -> Option<&crate::client::DaemonClient> {
        self.daemon.as_ref()
    }

    /// Forgets the prepared tool environment of a deleted session.
    pub fn evict_session(&self, session_id: &SessionId) {
        self.tool_environments.evict(session_id);
    }

    /// Return a copy that overrides the configured model and reasoning effort for
    /// the next spawned worker only.
    #[must_use]
    pub fn with_oneshot_overrides(
        mut self,
        model: Option<String>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        self.oneshot_model = model;
        self.oneshot_reasoning_effort = reasoning_effort;
        self.owns_turns = true;
        self
    }

    /// Creates and activates a new session in memory only.
    ///
    /// Nothing reaches the daemon yet: the session row, its directories and
    /// the agent worker are created by [`Self::materialize_session`] on the
    /// first message sent.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when no model can be resolved for display.
    pub async fn begin_new_session(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
    ) -> Result<(), TuiRuntimeError> {
        discard_ephemeral_session(app).await;
        let model = self
            .oneshot_model
            .clone()
            .or_else(|| app.config.last_model.as_ref().map(ToString::to_string));
        let name = app.session_manager.issue_session_name().await?;
        let (session_id, messages) = {
            let session = app
                .session_manager
                .create_session_with_model(name, model.clone())?;
            (session.id.clone(), session.history.messages().to_vec())
        };

        app.init_session(&session_id, &messages).await;
        *worker = None;

        let selected_model = SelectedModel::resolve(&app.config, model.as_deref())?;
        app.set_current_model(selected_model.as_str());
        Ok(())
    }

    /// Persist the current session and start building its worker — the
    /// transition a session's first message performs. `Ok(None)` when the
    /// current session was already materialized or none is active.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the session cannot be persisted or
    /// the worker build fails to start.
    pub async fn materialize_session(
        &self,
        app: &mut App,
    ) -> Result<Option<PendingWorkerBuild>, TuiRuntimeError> {
        if app
            .session_manager
            .current()
            .is_none_or(|session| session.persisted)
        {
            return Ok(None);
        }
        app.session_manager.ensure_persisted().await?;
        let session = app
            .session_manager
            .current()
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();
        let selected_model = SelectedModel::resolve(&app.config, session.model.as_deref())?;
        let reasoning_effort = self.oneshot_reasoning_effort.or_else(|| {
            app.current_effort
                .as_deref()
                .and_then(parse_reasoning_effort)
        });
        self.begin_worker_build(
            app,
            sandbox_dir,
            session_id,
            selected_model,
            reasoning_effort,
            None,
        )
        .map(Some)
    }

    /// Activates a stored session and starts building its worker.
    ///
    /// The previous session's worker is dropped at once; messages sent before
    /// the new worker is ready are queued for it.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the session cannot be loaded or the worker build fails.
    pub async fn begin_load_session(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        id: &SessionId,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        discard_ephemeral_session(app).await;
        let session = app.session_manager.load_session(id).await?;

        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();
        let session_model = session.model.clone();
        let events_cursor = session.events_cursor.clone();
        let messages: Vec<_> = session.history.messages().to_vec();
        let suspended = session.suspended;

        app.init_session(&session_id, &messages).await;
        if suspended {
            app.chat.push_notice(SUSPENDED_NOTICE);
        }
        *worker = None;

        let selected_model = SelectedModel::resolve(&app.config, session_model.as_deref())?;
        self.begin_worker_build(
            app,
            sandbox_dir,
            session_id,
            selected_model,
            None,
            events_cursor,
        )
    }

    /// Import a foreign agent's global rules and skills: the daemon copies
    /// the skills and opens the session that rewrites the rules into
    /// `~/.may/AGENTS.md`, which this switches to. `None` when the agent has
    /// no rules file, so only its skills were copied.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the daemon refuses the import or the
    /// import session cannot be loaded.
    pub async fn begin_rules_import(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        source: crate::session::import::ForeignAgent,
    ) -> Result<Option<PendingWorkerBuild>, TuiRuntimeError> {
        app.chat.push_notice(&format!(
            "Importing {}'s skills and global rules…",
            source.product_name()
        ));
        let response: crate::rules_import::RulesImportResponse = self
            .daemon()
            .ok_or(TuiRuntimeError::DaemonUnavailable)?
            .post(
                "/api/rules/import",
                &crate::rules_import::RulesImportRequest {
                    source: source.label().to_string(),
                    profile_id: app.current_profile_id.clone(),
                },
            )
            .await?;
        let skills = |names: &[String]| {
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        };
        let skills_notice = format!(
            "Skills imported: {}. Skipped, May already has one by that name: {}.",
            skills(&response.imported_skills),
            skills(&response.skipped_skills)
        );
        let Some(session_id) = response.session_id else {
            app.chat.push_notice(&skills_notice);
            app.chat.push_notice(&format!(
                "No global rules to import: {} does not exist.",
                response.rules_file
            ));
            return Ok(None);
        };
        let build = self
            .begin_load_session(app, worker, &SessionId::from(session_id))
            .await?;
        app.chat.push_notice(&skills_notice);
        Ok(Some(build))
    }

    /// Import a foreign agent transcript into a fresh session and start
    /// building its worker.
    ///
    /// The transcript is located and parsed off the event loop, then the new
    /// session is seeded with the converted messages and a provenance note.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the transcript cannot be found or
    /// parsed, or when session/worker setup fails.
    pub async fn begin_import_session(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        source: crate::session::import::ForeignAgent,
        query: &str,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        discard_ephemeral_session(app).await;
        let query_owned = query.to_string();
        let mut imported =
            blocking::unblock(move || crate::session::import::import(source, &query_owned)).await?;

        let model = imported.model.clone();
        let name = app.session_manager.issue_session_name().await?;
        let (sandbox_dir, _session_id, imported_count) = {
            let session = app
                .session_manager
                .create_session_with_model(name, model.clone())?;
            (
                session.sandbox_dir.clone(),
                session.id.clone(),
                imported.messages.len(),
            )
        };
        app.session_manager.ensure_persisted().await?;
        app.session_manager
            .set_current_title(imported.title.clone())
            .await?;

        // Persist image payloads the parser decoded into the session dir,
        // then fix the attachment URLs to their real paths.
        for (rel, bytes) in &imported.files {
            let path = sandbox_dir.join(rel);
            if let Some(parent) = path.parent() {
                async_fs::create_dir_all(parent).await?;
            }
            async_fs::write(&path, bytes).await?;
        }
        let provenance = serde_json::json!({
            "kind": "imported_session",
            "source": source.label(),
            "source_session_id": imported.source_id,
            "source_path": imported.source_path.display().to_string(),
            "source_cwd": imported.cwd,
            "note": "This session continues an imported foreign-agent transcript. Prior tool calls appear as `terminal` calls; reasoning state was preserved verbatim.",
        });
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        let mut injected = Vec::with_capacity(imported.messages.len() + 1);
        injected.push(ChatMessage::hidden_context(provenance.to_string()));
        injected.append(&mut imported.messages);
        let _: i64 = self
            .daemon()
            .ok_or(TuiRuntimeError::DaemonUnavailable)?
            .post(
                &format!("/api/sessions/{}/history", session_id.as_str()),
                &injected,
            )
            .await?;
        for message in injected {
            app.session_manager.record_unpersisted(message);
        }

        let messages: Vec<_> = app
            .session_manager
            .current()
            .map(|session| session.history.messages().to_vec())
            .unwrap_or_default();
        app.init_session(&session_id, &messages).await;
        app.chat.push_notice(&format!(
            "Imported {imported_count} messages from {} session {}",
            source.label(),
            imported.source_id
        ));
        *worker = None;

        let selected_model = SelectedModel::resolve(&app.config, model.as_deref())?;
        self.begin_worker_build(app, sandbox_dir, session_id, selected_model, None, None)
    }

    /// Starts building a worker for the current session on `model`; the
    /// current worker keeps serving until the new one is installed.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the worker build cannot start.
    pub fn begin_switch_model(
        &self,
        app: &App,
        model: &str,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        let session = app
            .session_manager
            .current()
            .ok_or(TuiRuntimeError::NoActiveSession)?;

        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();

        self.begin_worker_build(
            app,
            sandbox_dir,
            session_id,
            SelectedModel::new(model)?,
            None,
            None,
        )
    }

    /// Starts building a replacement worker for the current session from the
    /// saved config, on the session's model; the current worker keeps serving
    /// until the new one is installed.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the worker build cannot start.
    pub fn begin_reload_worker(&self, app: &App) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        let session = app
            .session_manager
            .current()
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        let sandbox_dir = session.sandbox_dir.clone();
        let session_id = session.id.clone();
        let selected_model = SelectedModel::resolve(&app.config, session.model.as_deref())?;
        self.begin_worker_build(app, sandbox_dir, session_id, selected_model, None, None)
    }

    /// Installs a worker finished by `build` as the current session's worker.
    pub fn install_worker(
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        build: &PendingWorkerBuild,
        new_worker: AgentWorker,
    ) {
        app.set_current_model(build.model());
        app.worker_settings = build.settings().clone();
        app.background = new_worker.background().cloned();
        app.refresh_jobs();
        *worker = Some(new_worker);
    }

    /// Start an agent turn for the current session.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when no session is active or the worker fails to start.
    pub async fn start_agent_task(
        &self,
        app: &mut App,
        worker: &AgentWorker,
        msg: PendingMessage,
        title_gen: &mut Option<TitleGenContext>,
    ) -> Result<AgentHandle, TuiRuntimeError> {
        let turn = Self::show_agent_turn(app, msg, title_gen).await?;
        Ok(worker.send(turn))
    }

    /// Shows `msg` as a turn in progress (user bubble, loading indicator) and
    /// returns the turn to hand to a worker, which may not exist yet.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when an attachment cannot be persisted.
    pub async fn show_agent_turn(
        app: &mut App,
        msg: PendingMessage,
        title_gen: &mut Option<TitleGenContext>,
    ) -> Result<crate::agent::RemoteTurn, TuiRuntimeError> {
        let needs_title = app
            .session_manager
            .current()
            .is_some_and(|s| s.title.is_none());

        if needs_title {
            *title_gen = Some(TitleGenContext {
                user_message: msg.content.clone(),
            });
        }

        Self::display_remote_turn(app, msg).await
    }

    /// Shows `msg` and builds the turn the daemon will run. The daemon writes
    /// the turn's rows; the in-memory history records the message now so the
    /// chat's history positions stay aligned until the turn's end reloads them.
    async fn display_remote_turn(
        app: &mut App,
        msg: PendingMessage,
    ) -> Result<crate::agent::RemoteTurn, TuiRuntimeError> {
        let client = app.session_manager.daemon_handle()?;
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.as_str().to_string())
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        let mut attachments = Vec::with_capacity(msg.attachments.len());
        for attachment in &msg.attachments {
            attachments
                .push(persisted_attachment_from_tui(&client, &session_id, attachment).await?);
        }
        if let Some(hidden_context_xml) = msg.hidden_context_xml.as_ref() {
            app.session_manager
                .record_unpersisted(ChatMessage::hidden_context(hidden_context_xml.to_string()));
        }
        app.session_manager
            .record_unpersisted(ChatMessage::user_with_attachments(
                msg.content.clone(),
                attachments.clone(),
            ));
        app.start_processing(&msg);
        Ok(crate::agent::RemoteTurn {
            content: msg.content,
            attachments,
            hidden_context_xml: msg.hidden_context_xml.as_ref().map(ToString::to_string),
        })
    }

    /// Settle the app after a run finishes: persist the assistant message, save
    /// the session, finish the run (which settles the session's runtime state
    /// and releases its lease), and kick off title generation.
    pub async fn finish_agent_task(
        &self,
        app: &mut App,
        agent_handle: &mut Option<AgentHandle>,
        title_gen: &mut Option<TitleGenContext>,
        _assistant_response: &AssistantReply,
        _outcome: crate::session::RunOutcome,
    ) {
        let _session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone());
        *agent_handle = None;
        app.finish_processing();
        app.clear_persistent_status();
        app.chat
            .fold_turn_work(app.turn_started_at.map_or(0, |at| at.elapsed().as_secs()));

        // The daemon persisted the turn and names the session.
        if let Err(error) = app.session_manager.refresh_history().await {
            tracing::warn!(%error, "failed to reload the session history");
        }
        title_gen.take();
    }

    /// Rewind the conversation to just before the user message at
    /// `history_index`, optionally restoring tracked files to that
    /// prompt's checkpoint, then start building a worker that replays the
    /// truncated history.
    ///
    /// The prompt's text and attachments are placed back in the input for
    /// editing; pending queued messages are dropped with the abandoned
    /// timeline; the current worker is dropped since its context still
    /// holds it.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when `history_index` does not identify
    /// a user prompt, the persisted history cannot be truncated, or the
    /// worker build cannot start.
    pub async fn rewind_conversation(
        &self,
        app: &mut App,
        worker: &mut Option<AgentWorker>,
        history_index: usize,
        restore_code: bool,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        let Some((content, attachments)) = app.chat.prompt_rewind_payload(history_index) else {
            return Err(crate::session::SessionError::NotFound(format!(
                "no prompt at history position {history_index}"
            ))
            .into());
        };
        let point = app.session_manager.rewind_to_prompt(history_index).await?;

        let session = app
            .session_manager
            .current()
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        let session_id = session.id.clone();
        let sandbox_dir = session.sandbox_dir.clone();
        let model = session.model.clone();

        if restore_code {
            app.request_prompt_rewind(point.user_message_id);
        }

        app.chat.truncate_prompt_at(history_index);
        app.input = content;
        app.cursor = app.input.len();
        app.attachments = attachments;
        // The next send carries a one-shot notice: the conversation rolled
        // back while the world — open and merged PRs — stands. The audit
        // log already recorded a `rewound` event per session PR.
        let mut notice =
            "The conversation was rewound to an earlier point. Nothing outside the conversation changed — repository state, open and merged pull requests, and issues still stand as recorded. Undo landed code through a revert PR, never by rewriting history.".to_string();
        let prs = &app.sidebar.prs;
        if !prs.is_empty() {
            let list = prs
                .iter()
                .map(|pr| format!("#{} {} ({})", pr.number, pr.title, pr.state))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = std::fmt::Write::write_fmt(
                &mut notice,
                format_args!(" Session pull requests: {list}."),
            );
        }
        app.rewind_notice = crate::xml::XmlBlock::parse(notice).ok();
        app.chat.push_notice(&format!(
            "⏪ rewound to before this prompt{}",
            if restore_code {
                " (conversation + code)"
            } else {
                " (conversation)"
            }
        ));
        app.scroll = usize::MAX;

        *worker = None;
        let selected_model = SelectedModel::resolve(&app.config, model.as_deref())?;
        self.begin_worker_build(app, sandbox_dir, session_id, selected_model, None, None)
    }

    /// A worker for `session_id` whose agent runs in the daemon. The model,
    /// reasoning effort and working directory the daemon runs the session
    /// with are recorded on the session first. `events_cursor` is the
    /// cursor `GET /api/sessions/{id}` reported when the session loaded —
    /// the point its event stream resumes from.
    fn begin_remote_worker(
        app: &App,
        client: crate::client::DaemonClient,
        session_id: SessionId,
        model: SelectedModel,
        reasoning_effort: Option<ReasoningEffort>,
        events_cursor: Option<String>,
        owns_turns: bool,
    ) -> PendingWorkerBuild {
        let cwd = app.cwd.display().to_string();
        let model_id = model.as_str().to_string();
        let app_events = app.event_tx.clone();
        let task = spawn(async move {
            let mut body = serde_json::json!({
                "model": model_id,
                "cwd": cwd,
            });
            // An absent reasoningEffort leaves the stored level untouched; a
            // serialized null would read as "clear" on the daemon's tri-state.
            if let Some(effort) = reasoning_effort {
                body["reasoningEffort"] = serde_json::json!(match effort {
                    ReasoningEffort::None => "none",
                    ReasoningEffort::Minimal => "minimal",
                    ReasoningEffort::Low => "low",
                    ReasoningEffort::Medium => "medium",
                    ReasoningEffort::High => "high",
                    ReasoningEffort::XHigh => "xhigh",
                    ReasoningEffort::Max => "max",
                });
            }
            let _: serde_json::Value = client
                .patch(&format!("/api/sessions/{}", session_id.as_str()), &body)
                .await
                .map_err(TuiRuntimeError::from)?;
            Ok(crate::tui::remote::spawn_remote_worker(
                client,
                session_id.as_str().to_string(),
                app_events,
                events_cursor,
                owns_turns,
            ))
        });
        PendingWorkerBuild {
            model: model.0,
            settings: crate::agent::AgentLiveSettings::of(&app.config),
            task: Box::pin(task),
        }
    }

    fn begin_worker_build(
        &self,
        app: &App,
        _sandbox_dir: std::path::PathBuf,
        session_id: SessionId,
        model: SelectedModel,
        reasoning_effort: Option<ReasoningEffort>,
        events_cursor: Option<String>,
    ) -> Result<PendingWorkerBuild, TuiRuntimeError> {
        if let Some(client) = self.daemon.clone() {
            return Ok(Self::begin_remote_worker(
                app,
                client,
                session_id,
                model,
                reasoning_effort,
                events_cursor,
                self.owns_turns,
            ));
        }
        Ok(Self::begin_remote_worker(
            app,
            self.daemon
                .clone()
                .ok_or(TuiRuntimeError::DaemonUnavailable)?,
            session_id,
            model,
            reasoning_effort,
            events_cursor,
            self.owns_turns,
        ))
    }

    /// Append a message to the pending queue and sync the app state.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn enqueue_pending_message(
        &self,
        app: &mut App,
        message: PendingMessage,
    ) -> Result<(), TuiRuntimeError> {
        // The local queue mirrors the message for display until the daemon
        // dispatches it, and the next `runtime.changed` frame makes it
        // authoritative.
        app.queue.enqueue(message.clone());
        self.post_enqueue(app, &message).await
    }

    /// Posts an already displayed queued message to the daemon, which
    /// dispatches queued and steering messages itself; the turns they start
    /// reach this TUI as foreign turns.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage or daemon failure.
    pub async fn post_enqueue(
        &self,
        app: &mut App,
        message: &PendingMessage,
    ) -> Result<(), TuiRuntimeError> {
        let session_id = app
            .session_manager
            .current()
            .map(|session| session.id.clone())
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        if let Some(client) = &self.daemon {
            let body = serde_json::json!({
                "sessionId": session_id.as_str(),
                "content": message.content,
                "attachments": futures_util::future::try_join_all(
                    message
                        .attachments
                        .iter()
                        .map(|attachment| {
                            persisted_attachment_from_tui(
                                client,
                                session_id.as_str(),
                                attachment,
                            )
                        }),
                )
                .await?,
                "hiddenContextXml": message.hidden_context_xml.as_ref().map(ToString::to_string),
                "mode": match message.delivery_mode.as_str() { "queue" => "queue", _ => "steer" },
            });
            if let Err(error) = client
                .post::<_, serde_json::Value>(&format!("/api/sessions/{session_id}/turns"), &body)
                .await
            {
                app.sync_queue_from_runtime();
                return Err(error.into());
            }
        }
        Ok(())
    }

    ///
    /// # Errors
    /// Always succeeds.
    pub const fn listen_for_agent_messages(&self) -> Result<(), TuiRuntimeError> {
        Ok(())
    }

    /// Start the next queued turn; the daemon dispatches the queue, so a
    /// client never has anything to start itself.
    ///
    /// # Errors
    /// Always succeeds.
    pub const fn begin_queued_run(
        &self,
        _app: &mut App,
    ) -> Result<QueuedRunStart, TuiRuntimeError> {
        Ok(QueuedRunStart::Nothing)
    }

    /// The daemon takes the run lease when the turn starts there.
    ///
    /// # Errors
    /// Always succeeds.
    pub const fn begin_run(&self, _app: &mut App) -> Result<bool, TuiRuntimeError> {
        Ok(true)
    }

    /// Commit the user's answers to an open question; the answer joins the
    /// session queue, which the event loop dispatches. Returns whether this
    /// call committed them: `false` when the question is no longer open.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError::Daemon`] carrying the daemon's message when
    /// the answers do not fit the question or could not be stored; the
    /// question is still open then.
    pub async fn answer_open_question(
        &self,
        request_id: &str,
        answers: &[QuestionAnswer],
    ) -> Result<bool, TuiRuntimeError> {
        if let Some(client) = &self.daemon {
            return committed_or_not_open(
                client
                    .post::<_, serde_json::Value>(
                        &format!("/api/interactions/{request_id}"),
                        &serde_json::json!({ "kind": "askUser", "answers": answers_json(answers) }),
                    )
                    .await,
            );
        }
        Ok(false)
    }

    /// Commit the user's answers to the prompt a run is blocked on; the
    /// caller wakes the run afterwards, only if this call committed.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] when the answers do not fit the prompt or
    /// on storage failure.
    pub async fn resolve_ask_user_response(
        &self,
        _session_id: &SessionId,
        request_id: &str,
        answers: &[QuestionAnswer],
    ) -> Result<ResponseCommit<Vec<QuestionAnswer>>, TuiRuntimeError> {
        let Some(client) = &self.daemon else {
            return Ok(ResponseCommit::Lost(answers.to_vec()));
        };
        let committed = committed_or_not_open(
            client
                .post::<_, serde_json::Value>(
                    &format!("/api/interactions/{request_id}"),
                    &serde_json::json!({ "kind": "askUser", "answers": answers_json(answers) }),
                )
                .await,
        )?;
        Ok(if committed {
            ResponseCommit::Committed
        } else {
            ResponseCommit::Lost(answers.to_vec())
        })
    }

    /// Commit the workspace decision; the caller wakes the run afterwards,
    /// only if this call committed.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_workspace_response(
        &self,
        _session_id: &SessionId,
        request_id: &str,
        approved: bool,
    ) -> Result<ResponseCommit<bool>, TuiRuntimeError> {
        if let Some(client) = &self.daemon {
            let _: serde_json::Value = client
                .post(
                    &format!("/api/interactions/{request_id}"),
                    &serde_json::json!({ "kind": "workspace", "approved": approved }),
                )
                .await?;
            return Ok(ResponseCommit::Committed);
        }
        Ok(ResponseCommit::Lost(approved))
    }

    /// Record the permission decision and clear the pending request.
    ///
    /// # Errors
    /// Returns [`TuiRuntimeError`] on storage failure.
    pub async fn resolve_permission_response(
        &self,
        app: &App,
        request_id: &str,
        approved: bool,
    ) -> Result<(), TuiRuntimeError> {
        app.session_manager
            .current()
            .ok_or(TuiRuntimeError::NoActiveSession)?;
        if let Some(client) = &self.daemon {
            let _: serde_json::Value = client
                .post(
                    &format!("/api/interactions/{request_id}"),
                    &serde_json::json!({ "kind": "permission", "approved": approved }),
                )
                .await?;
            return Ok(());
        }
        Ok(())
    }
}

/// The assistant text a run streamed, with the emission time of its newest chunk.
#[derive(Debug, Default)]
pub struct AssistantReply {
    text: String,
    at: Option<jiff::Timestamp>,
}

impl AssistantReply {
    /// Append a text chunk the agent emitted at `at`.
    pub fn push(&mut self, chunk: &str, at: jiff::Timestamp) {
        if chunk.is_empty() {
            return;
        }
        self.text.push_str(chunk);
        self.at = Some(at);
    }

    /// Forget the accumulated reply.
    pub fn clear(&mut self) {
        self.text.clear();
        self.at = None;
    }

    /// The accumulated text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Emission time of the newest chunk; `None` while the reply is empty.
    #[must_use]
    pub const fn at(&self) -> Option<jiff::Timestamp> {
        self.at
    }
}

/// Drop the current session when nothing ever persisted it, removing the
/// scratch dirs (pasted attachments, the file-diff base) it may have left.
pub async fn discard_ephemeral_session(app: &mut App) {
    let Some(session) = app.session_manager.current() else {
        return;
    };
    if session.persisted {
        return;
    }
    let sandbox_dir = session.sandbox_dir.clone();
    app.session_manager.discard_current();
    if let Err(error) = async_fs::remove_dir_all(&sandbox_dir).await {
        tracing::debug!(%error, "ephemeral session scratch dir cleanup failed");
    }
}

/// Reasoning-effort strings as the effort ladder carries them.
fn parse_reasoning_effort(value: &str) -> Option<ReasoningEffort> {
    Some(match value {
        "none" => ReasoningEffort::None,
        "minimal" => ReasoningEffort::Minimal,
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::XHigh,
        "max" => ReasoningEffort::Max,
        _ => return None,
    })
}

/// Context needed to generate a session title once a run finishes.
#[derive(Debug)]
pub struct TitleGenContext {
    /// The user message that opened the session.
    pub user_message: String,
}

#[derive(Debug, Clone)]
struct SelectedModel(String);

impl SelectedModel {
    fn new(model: &str) -> Result<Self, TuiRuntimeError> {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            return Err(TuiRuntimeError::EmptySelectedModel);
        }
        Ok(Self(trimmed.to_string()))
    }

    fn resolve(config: &MayConfig, preferred: Option<&str>) -> Result<Self, TuiRuntimeError> {
        if let Some(model) = preferred {
            return Self::new(model);
        }

        if let Some(model) = config.last_model.as_ref().map(ToString::to_string) {
            return Self::new(&model);
        }

        let model = config
            .default_model_for_tier(crate::config::ModelTier::Balanced)
            .ok_or(TuiRuntimeError::NoModelConfigured)?;

        Self::new(&model.to_string())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::urlencoding;

    #[test]
    fn an_upload_name_is_query_safe() {
        assert_eq!(urlencoding("my shot.png"), "my%20shot.png");
        assert_eq!(urlencoding("a&b=1"), "a%26b%3D1");
    }
}
