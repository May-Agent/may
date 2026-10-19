//! `may <prompt>` (one-shot) subcommand.
//!
//! Runs a single agent turn against the most recent session in the
//! current profile, streams assistant text to stdout, and writes
//! progress events (tool calls, reasoning, usage) to stderr. Designed
//! for non-interactive CLI workflows where stdin is not a TTY — the
//! agent is automatically told to avoid `ask_user` unless the caller
//! explicitly allows interactive prompts.

use std::io::{self, Write};

use aither_agent::AgentEvent;
use jiff::Timestamp;

use may_agent::agent::{AgentWorker, TuiEvent};
use may_agent::ask_user;
use may_agent::tui::runtime::{LocalSessionRuntime, RuntimeDeps, TitleGenContext};
use may_agent::tui::{App, PendingMessage};
use may_agent::workspace_request;

use super::cli::OneShotArgs;

/// Records a question for the current session, prompts on stderr and answers it.
async fn answer_oneshot_question(
    runtime_deps: &RuntimeDeps,
    app: &mut App,
    request: may_agent::ask_user::AskUserRequest,
    stderr: &mut io::Stderr,
) -> anyhow::Result<()> {
    let session_id = app
        .session_manager
        .current()
        .map(|session| session.id.clone())
        .ok_or_else(|| anyhow::anyhow!("oneshot session missing"))?;
    let questions = request.args.request.clone().into_questions();
    let request_id = runtime_deps
        .register_ask_user_request(app, &session_id, &questions)
        .await?;
    let answers = prompt_oneshot_questions(&questions, stderr)?;
    runtime_deps
        .resolve_ask_user_response(app, &session_id, &request_id, &answers)
        .await?;
    request.respond(answers).map_err(anyhow::Error::from)?;
    Ok(())
}

pub async fn run_oneshot(args: OneShotArgs) -> anyhow::Result<()> {
    enum Event {
        AskUser(may_agent::ask_user::AskUserRequest),
        Workspace(may_agent::workspace_request::WorkspaceRequest),
        Agent(TuiEvent),
    }

    const ONESHOT_IDLE_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    let (event_tx, _event_rx) = async_channel::unbounded();
    let mut app = App::new(event_tx, None).await?;
    if !app.is_configured() {
        anyhow::bail!("May is not configured");
    }

    if let Some(model) = args.model.as_ref() {
        app.config.last_model = Some(
            model
                .parse::<may_agent::ModelSelector>()
                .expect("oneshot model must be an explicit provider::model selector"),
        );
    }

    let (workspace_broker, workspace_queue) = workspace_request::channel();
    let (ask_user_broker, ask_user_queue) = ask_user::channel();
    let mut local_runtime = LocalSessionRuntime::new(app.session_manager.db().clone());
    local_runtime.enable_oneshot_restricted_mode();
    let tool_distribution = std::sync::Arc::new(may_agent::tools::embedded_distribution()?);
    let tool_dispatchers = may_agent::tool_dispatch::ToolDispatchers::spawn(
        may_agent::tool_dispatch::ToolDispatchConfig {
            may_dir: app.session_manager.may_dir().to_path_buf(),
            cwd: app.cwd.clone(),
            db: app.session_manager.db().clone(),
            browser_config: app.config.browser.clone(),
            network_policy: app.config.preferences.network_policy.clone(),
            ask_user_broker: ask_user_broker.clone(),
            tool_distribution: tool_distribution.clone(),
        },
    )
    .await?;
    let runtime_deps = RuntimeDeps::new(
        local_runtime,
        workspace_broker,
        ask_user_broker,
        tool_dispatchers,
        std::sync::Arc::new(may_agent::location::LocationProvider::new()),
        tool_distribution,
        may_agent::agent::ToolEnvironmentCache::spawn(),
    )
    .with_oneshot_overrides(args.model.clone(), args.reasoning_effort);

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut worker: Option<AgentWorker> = None;
    let mut build = runtime_deps
        .begin_new_session(&mut app, &mut worker)
        .await?;
    // Preparing the session's tool environments can ask for SSH consent, so
    // questions are answered while the worker is built.
    let completed = loop {
        let next = futures_lite::future::or(async { Ok(build.wait().await) }, async {
            Err(ask_user_queue.next().await)
        })
        .await;
        match next {
            Ok(completed) => break completed?,
            Err(Some(request)) => {
                answer_oneshot_question(&runtime_deps, &mut app, request, &mut stderr).await?;
            }
            Err(None) => anyhow::bail!("ask-user channel closed while preparing the session"),
        }
    };
    RuntimeDeps::install_worker(&mut app, &mut worker, &build, completed);
    let worker = worker
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("oneshot worker was not initialized"))?;

    let mut pending_title_gen: Option<TitleGenContext> = None;
    let mut assistant_response = String::new();
    let mut active_tool_calls = std::collections::HashSet::<String>::new();
    runtime_deps.mark_running(&app).await?;
    let prompt = build_oneshot_prompt_with_mentions(&app, &args.prompt).await?;
    let handle = runtime_deps
        .start_agent_task(
            &mut app,
            worker,
            PendingMessage {
                content: prompt.content,
                hidden_context_xml: prompt.hidden_context_xml,
                attachments: Vec::new(),
                queued_at: Timestamp::now(),
                delivery_mode: may_agent::session::PendingUserMessageMode::Steer,
            },
            &mut pending_title_gen,
        )
        .await?;
    let mut agent_handle = Some(handle);

    loop {
        if let Some(request) = runtime_deps.pending_permission(&app).await? {
            let approved = prompt_oneshot_permission(&request.request, &mut stderr)?;
            runtime_deps
                .resolve_permission_response(&mut app, &request.id, approved)
                .await?;
            continue;
        }

        let Some(handle) = agent_handle.as_ref() else {
            break;
        };
        let event_future = futures_lite::future::or(
            async { ask_user_queue.next().await.map(Event::AskUser) },
            futures_lite::future::or(
                async { workspace_queue.next().await.map(Event::Workspace) },
                async { handle.recv().await.map(Event::Agent) },
            ),
        );
        let next = if assistant_response.is_empty() || !active_tool_calls.is_empty() {
            event_future.await
        } else if let Ok(event) =
            tokio::time::timeout(ONESHOT_IDLE_FINISH_TIMEOUT, event_future).await
        {
            event
        } else {
            if let Some(handle) = agent_handle.take() {
                handle.cancel();
            }
            runtime_deps
                .finish_agent_task(
                    &mut app,
                    &mut agent_handle,
                    &mut pending_title_gen,
                    &assistant_response,
                )
                .await;
            if !assistant_response.ends_with('\n') {
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
            return Ok(());
        };
        let Some(event) = next else {
            break;
        };
        match event {
            Event::AskUser(request) => {
                answer_oneshot_question(&runtime_deps, &mut app, request, &mut stderr).await?;
            }
            Event::Workspace(request) => {
                let session_id = app
                    .session_manager
                    .current()
                    .map(|session| session.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("oneshot session missing"))?;
                let request_id = runtime_deps
                    .register_workspace_request(&mut app, &session_id, &request.args.request)
                    .await?;
                let approved = prompt_oneshot_workspace(&request.args.request, &mut stderr)?;
                runtime_deps
                    .resolve_workspace_response(&mut app, &session_id, &request_id, approved)
                    .await?;
                request.respond(approved).map_err(anyhow::Error::from)?;
            }
            Event::Agent(TuiEvent::Agent(result)) => {
                if let Ok(AgentEvent::ToolCallStart { id, .. }) = &result {
                    active_tool_calls.insert(id.clone());
                }
                if let Ok(AgentEvent::ToolCallEnd { id, .. }) = &result {
                    active_tool_calls.remove(id);
                }
                if let Ok(AgentEvent::Text(ref text)) = result {
                    assistant_response.push_str(text);
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                }
                render_oneshot_event(&mut stderr, &result)?;
                let run_failed = result.is_err();
                let ended = app.handle_agent_event(result).await;
                if ended {
                    runtime_deps
                        .finish_agent_task(
                            &mut app,
                            &mut agent_handle,
                            &mut pending_title_gen,
                            &assistant_response,
                        )
                        .await;
                    if run_failed {
                        anyhow::bail!("oneshot agent run failed");
                    }
                    if !assistant_response.ends_with('\n') {
                        stdout.write_all(b"\n")?;
                        stdout.flush()?;
                    }
                    return Ok(());
                }
            }
            Event::Agent(TuiEvent::Complete) => {
                runtime_deps
                    .finish_agent_task(
                        &mut app,
                        &mut None,
                        &mut pending_title_gen,
                        &assistant_response,
                    )
                    .await;
                if !assistant_response.ends_with('\n') {
                    stdout.write_all(b"\n")?;
                    stdout.flush()?;
                }
                return Ok(());
            }
            Event::Agent(TuiEvent::Compacted(_) | TuiEvent::ContextReport(_)) => {}
            Event::Agent(TuiEvent::Error(error)) => {
                let _ = writeln!(stderr, "[error] {error}");
                runtime_deps
                    .finish_agent_task(
                        &mut app,
                        &mut None,
                        &mut pending_title_gen,
                        &assistant_response,
                    )
                    .await;
                anyhow::bail!(error);
            }
        }
    }

    Ok(())
}

fn prompt_oneshot_questions(
    questions: &[may_agent::ask_user::Question],
    stderr: &mut impl Write,
) -> anyhow::Result<Vec<may_agent::ask_user::QuestionAnswer>> {
    let mut answers = Vec::with_capacity(questions.len());
    for question in questions {
        writeln!(stderr, "[ask_user] {}", question.question)?;
        if !question.section.is_empty() {
            writeln!(stderr, "section: {}", question.section)?;
        }
        for (index, option) in question.option.iter().enumerate() {
            writeln!(stderr, "{}. {}", index + 1, option)?;
        }
        if question.multi_select {
            writeln!(
                stderr,
                "Enter comma-separated numbers, `s` to skip, or any other text for a custom answer:"
            )?;
        } else {
            writeln!(
                stderr,
                "Enter a number, `s` to skip, or any other text for a custom answer:"
            )?;
        }
        stderr.flush()?;
        let answer = loop {
            let input = read_oneshot_line()?;
            let trimmed = input.trim();
            if trimmed.eq_ignore_ascii_case("s") || trimmed.eq_ignore_ascii_case("skip") {
                break may_agent::ask_user::AnswerValue::Skipped;
            }
            if question.option.is_empty() {
                if trimmed.is_empty() {
                    writeln!(stderr, "answer must not be empty")?;
                    stderr.flush()?;
                    continue;
                }
                break may_agent::ask_user::AnswerValue::Custom(trimmed.to_string());
            }
            if let Some(selected) =
                parse_oneshot_selection(trimmed, &question.option, question.multi_select)
            {
                break may_agent::ask_user::AnswerValue::Selected(selected);
            }
            if trimmed.is_empty() {
                writeln!(stderr, "answer must not be empty")?;
                stderr.flush()?;
                continue;
            }
            break may_agent::ask_user::AnswerValue::Custom(trimmed.to_string());
        };
        answers.push(may_agent::ask_user::QuestionAnswer {
            section: question.section.clone(),
            answer,
        });
    }
    Ok(answers)
}

fn prompt_oneshot_workspace(
    request: &may_agent::workspace_request::RequestWorkspaceArgs,
    stderr: &mut impl Write,
) -> anyhow::Result<bool> {
    writeln!(stderr, "[workspace_request] path={}", request.path)?;
    writeln!(stderr, "reason={}", request.reason)?;
    writeln!(stderr, "Approve? [y/N]")?;
    stderr.flush()?;
    loop {
        let input = read_oneshot_line()?;
        let trimmed = input.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("n")
            || trimmed.eq_ignore_ascii_case("no")
        {
            return Ok(false);
        }
        if trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes") {
            return Ok(true);
        }
        writeln!(stderr, "Please answer `y` or `n`.")?;
        stderr.flush()?;
    }
}

#[derive(serde::Serialize)]
struct OneShotNonInteractiveAlert<'a> {
    #[serde(rename = "@type")]
    alert_type: &'a str,
    message: &'a str,
}

#[derive(Debug, Clone)]
struct OneShotPromptInput {
    content: String,
    hidden_context_xml: Option<may_agent::XmlBlock>,
}

async fn build_oneshot_prompt_with_mentions(
    app: &App,
    prompt: &str,
) -> anyhow::Result<OneShotPromptInput> {
    let mut hidden_context_parts = vec![may_agent::xml::serialize_xml_block(
        "system-reminder",
        &OneShotNonInteractiveAlert {
            alert_type: "oneshot_noninteractive",
            message: "Non-interactive oneshot run. Do not use ask_user or wait for stdin. Make a reasonable assumption when possible; if interaction is truly required, explain the blocker and stop.",
        },
    )?];
    let Some(session) = app.session_manager.current() else {
        return Ok(OneShotPromptInput {
            content: prompt.to_string(),
            hidden_context_xml: Some(may_agent::XmlBlock::parse(
                hidden_context_parts.join("\n\n"),
            )?),
        });
    };

    let db = app.session_manager.db();
    if let Ok(Some(session_mentions_xml)) =
        may_agent::prompt_mentions::build_session_mentions_xml(db, prompt).await
    {
        hidden_context_parts.push(session_mentions_xml);
    }

    let session_paths = may_agent::session::SessionPaths::new(session.sandbox_dir.clone());
    let files_dir = session_paths.files_dir();
    if let Ok(Some(file_mentions_xml)) =
        may_agent::prompt_mentions::build_file_mentions_xml(&files_dir, prompt, 400).await
    {
        hidden_context_parts.push(file_mentions_xml);
    }

    Ok(OneShotPromptInput {
        content: prompt.to_string(),
        hidden_context_xml: Some(may_agent::XmlBlock::parse(
            hidden_context_parts.join("\n\n"),
        )?),
    })
}

fn prompt_oneshot_permission(
    request: &may_agent::session::PersistedPermissionRequestPayload,
    stderr: &mut impl Write,
) -> anyhow::Result<bool> {
    writeln!(stderr, "[permission_request] mode={}", request.mode)?;
    writeln!(stderr, "script={}", request.script)?;
    writeln!(stderr, "Approve? [y/N]")?;
    stderr.flush()?;
    loop {
        let input = read_oneshot_line()?;
        let trimmed = input.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("n")
            || trimmed.eq_ignore_ascii_case("no")
        {
            return Ok(false);
        }
        if trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes") {
            return Ok(true);
        }
        writeln!(stderr, "Please answer `y` or `n`.")?;
        stderr.flush()?;
    }
}

fn read_oneshot_line() -> anyhow::Result<String> {
    let mut line = String::new();
    let bytes = io::stdin().read_line(&mut line)?;
    if bytes == 0 {
        anyhow::bail!("oneshot requires interactive stdin for this prompt, but stdin is closed");
    }
    Ok(line)
}

fn parse_oneshot_selection(
    input: &str,
    options: &[String],
    multi_select: bool,
) -> Option<Vec<String>> {
    let mut selected = Vec::new();
    for raw_part in input.split(',') {
        let trimmed = raw_part.trim();
        if trimmed.is_empty() {
            return None;
        }
        let index = trimmed.parse::<usize>().ok()?;
        let option = options.get(index.checked_sub(1)?).cloned()?;
        if !selected.iter().any(|existing| existing == &option) {
            selected.push(option);
        }
    }
    if selected.is_empty() {
        return None;
    }
    if !multi_select && selected.len() > 1 {
        return None;
    }
    Some(selected)
}

fn render_oneshot_event(
    stderr: &mut impl Write,
    result: &Result<AgentEvent, aither_agent::AgentError>,
) -> anyhow::Result<()> {
    match result {
        Ok(AgentEvent::RunStart { .. }) => {
            writeln!(stderr, "Running...")?;
        }
        Ok(AgentEvent::Reasoning(text)) => {
            if !text.trim().is_empty() {
                writeln!(stderr, "Thinking: {text}")?;
            }
        }
        // UI progress — oneshot mode ignores deltas.
        Ok(
            AgentEvent::ToolCallDelta { .. }
            | AgentEvent::TurnComplete { .. }
            | AgentEvent::Checkpoint { .. }
            | AgentEvent::Complete { .. }
            | AgentEvent::Text(_),
        ) => {}
        Ok(AgentEvent::ToolCallStart { name, .. }) => {
            writeln!(stderr, "Tool: {name}")?;
        }
        Ok(AgentEvent::ToolCallEnd { name, result, .. }) => {
            if result.is_error() {
                let error = result.error_message().unwrap_or("tool failed");
                writeln!(stderr, "Tool failed: {name}: {error}")?;
            } else {
                writeln!(stderr, "Tool finished: {name}")?;
            }
        }
        Ok(AgentEvent::BackgroundTaskStarted {
            task_id,
            output_file,
            ..
        }) => {
            writeln!(
                stderr,
                "Background task started: {task_id} (output: {output_file})"
            )?;
        }
        Ok(AgentEvent::BackgroundTaskCompleted { task_id, .. }) => {
            writeln!(stderr, "Background task completed: {task_id}")?;
        }
        Ok(AgentEvent::TerminalInputNeeded { task_id, notice }) => {
            writeln!(
                stderr,
                "Terminal input needed{}: {}",
                task_id
                    .as_deref()
                    .map(|id| {
                        let mut text = String::from(" for ");
                        text.push_str(id);
                        text
                    })
                    .unwrap_or_default(),
                notice,
            )?;
        }
        Ok(AgentEvent::RunPaused { reason, .. }) => {
            writeln!(stderr, "Paused: {reason:?}")?;
        }
        Ok(AgentEvent::RunResumed { reason, .. }) => {
            writeln!(stderr, "Resumed: {reason:?}")?;
        }
        Ok(AgentEvent::SkillActivated { name, .. }) => {
            writeln!(stderr, "Skill activated: {name}")?;
        }
        Ok(AgentEvent::Usage(usage)) => {
            writeln!(
                stderr,
                "Usage: prompt={:?} completion={:?} total={:?}",
                usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
            )?;
        }
        Ok(AgentEvent::Error(error)) | Err(error) => {
            writeln!(stderr, "Error: {error}")?;
        }
    }
    stderr.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_oneshot_selection;

    #[test]
    fn parse_oneshot_selection_accepts_single_choice() {
        let options = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let selected =
            parse_oneshot_selection("2", &options, false).expect("single selection must parse");
        assert_eq!(selected, vec!["b".to_string()]);
    }

    #[test]
    fn parse_oneshot_selection_rejects_multiple_when_not_allowed() {
        let options = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert!(parse_oneshot_selection("1,2", &options, false).is_none());
    }

    #[test]
    fn parse_oneshot_selection_accepts_multi_select() {
        let options = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let selected =
            parse_oneshot_selection("1,3", &options, true).expect("multi selection must parse");
        assert_eq!(selected, vec!["a".to_string(), "c".to_string()]);
    }
}
