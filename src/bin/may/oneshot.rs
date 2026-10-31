//! `may <prompt>` (one-shot) subcommand.
//!
//! Runs a single agent turn against the most recent session in the
//! current profile, streams assistant text to stdout, and writes
//! progress events (tool calls, reasoning, usage) to stderr. Designed
//! for non-interactive CLI workflows where stdin is not a TTY — the
//! agent is automatically told to avoid `ask_user` unless the caller
//! explicitly allows interactive prompts.

use std::io::{self, Write};

use aither_agent::{AgentEvent, TimedEvent};
use jiff::Timestamp;

use may_agent::agent::{AgentWorker, TuiEvent};
use may_agent::session::RunOutcome;
use may_agent::tui::runtime::{AssistantReply, ResponseCommit, RuntimeDeps, TitleGenContext};
use may_agent::tui::{App, PendingMessage};

use super::cli::OneShotArgs;

mod render;

use render::OneShotOutput;

/// The session's runtime snapshot as the daemon reports it; oneshot polls it
/// for the prompts a remote run waits on.
async fn session_runtime(
    runtime_deps: &RuntimeDeps,
    session_id: &may_agent::session::SessionId,
) -> anyhow::Result<may_agent::client::SessionRuntimeSnapshot> {
    Ok(runtime_deps
        .daemon()
        .ok_or_else(|| anyhow::anyhow!("the May daemon is not attached"))?
        .get::<may_agent::client::SessionRuntimeSnapshot>(&format!(
            "/api/sessions/{}/runtime",
            session_id.as_str()
        ))
        .await?)
}

/// Prompt for the questions the daemon's run waits on and commit the answers.
async fn answer_oneshot_questions(
    runtime_deps: &RuntimeDeps,
    session_id: &may_agent::session::SessionId,
    request: &may_agent::session::PersistedAskUserRequest,
    output: &mut OneShotOutput<io::Stdout, io::Stderr>,
) -> anyhow::Result<()> {
    let questions = request
        .questions
        .iter()
        .map(|question| may_agent::ask_user::Question {
            section: question.section.clone(),
            question: question.question.clone(),
            option: question.option.clone(),
            multi_select: question.multi_select,
        })
        .collect::<Vec<_>>();
    let answers = prompt_oneshot_questions(&questions, output.stderr()?)?;
    match runtime_deps
        .resolve_ask_user_response(session_id, &request.id, &answers)
        .await?
    {
        ResponseCommit::Committed => Ok(()),
        ResponseCommit::Lost(_) => {
            writeln!(
                output.stderr()?,
                "[notice] the question was already answered elsewhere; the run continues with that answer, not yours"
            )?;
            Ok(())
        }
        ResponseCommit::Withdrawn => Err(anyhow::anyhow!(
            "the question was withdrawn; this answer was not sent"
        )),
    }
}

pub async fn run_oneshot(args: OneShotArgs) -> anyhow::Result<()> {
    enum Event {
        Agent(TuiEvent),
        /// Time to look for prompts the daemon's run waits on.
        Poll,
    }

    /// How often prompts the daemon holds are looked for.
    const DAEMON_PROMPT_POLL: std::time::Duration = std::time::Duration::from_millis(300);

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

    let runtime_deps = RuntimeDeps::new(may_agent::agent::ToolEnvironmentCache::spawn())
        .with_oneshot_overrides(args.model.clone(), args.reasoning_effort);
    // The run happens in the May daemon, like every TUI turn.
    let paths = may_agent::daemon::DaemonPaths::user()?;
    let runtime_deps = runtime_deps.with_daemon(
        may_agent::daemon::lifecycle::ensure_running(&paths, |line| {
            let _ = writeln!(io::stderr(), "[may] {line}");
        })
        .await?,
    );

    let mut output = OneShotOutput::new(io::stdout(), io::stderr());
    let mut worker: Option<AgentWorker> = None;
    let mut build = runtime_deps
        .begin_new_session(&mut app, &mut worker)
        .await?;
    let session_id = app
        .session_manager
        .current()
        .map(|session| session.id.clone())
        .ok_or_else(|| anyhow::anyhow!("oneshot session missing"))?;
    // Nobody answers permission prompts of a one-shot run; the daemon
    // refuses them instead of asking.
    runtime_deps
        .daemon()
        .ok_or_else(|| anyhow::anyhow!("the May daemon is not attached"))?
        .put::<_, serde_json::Value>(
            &format!("/api/sessions/{}", session_id.as_str()),
            &may_agent::client::UpdateSessionRequest {
                non_interactive: Some(true),
                ..Default::default()
            },
        )
        .await?;
    let mut answered_prompts = std::collections::HashSet::<String>::new();
    // Preparing the session's tool environments can ask for SSH consent; the
    // daemon registers the prompt and oneshot answers through the runtime
    // snapshot while the worker is built.
    let completed = loop {
        let next = futures_lite::future::or(async { Ok(build.wait().await) }, async {
            async_io::Timer::after(DAEMON_PROMPT_POLL).await;
            Err(())
        })
        .await;
        if let Ok(completed) = next {
            break completed?;
        }
        let snapshot = session_runtime(&runtime_deps, &session_id).await?;
        let pending = snapshot
            .waiting_ask_user
            .clone()
            .or_else(|| snapshot.open_questions.first().cloned());
        if let Some(request) = pending
            && answered_prompts.insert(request.id.clone())
        {
            answer_oneshot_questions(&runtime_deps, &session_id, &request, &mut output).await?;
        }
    };
    RuntimeDeps::install_worker(&mut app, &mut worker, &build, completed);
    let worker = worker
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("oneshot worker was not initialized"))?;

    let mut pending_title_gen: Option<TitleGenContext> = None;
    let mut assistant_response = AssistantReply::default();
    let mut active_tool_calls = std::collections::HashSet::<String>::new();
    if !runtime_deps.begin_run(&mut app)? {
        anyhow::bail!("the session is already running in another May process");
    }
    let prompt = build_oneshot_prompt_with_mentions(&app, &args.prompt).await?;
    let handle = runtime_deps.start_agent_task(
        &mut app,
        worker,
        PendingMessage {
            content: prompt.content,
            hidden_context_xml: prompt.hidden_context_xml,
            attachments: Vec::new(),
            queued_at: Timestamp::now(),
            delivery_mode: may_agent::session::PendingUserMessageMode::Steer,
            display: None,
        },
        &mut pending_title_gen,
    )?;
    let mut agent_handle = Some(handle);
    // Prompt polls do not count as activity for the idle finish.
    let mut last_agent_event = std::time::Instant::now();

    while let Some(handle) = agent_handle.as_ref() {
        let snapshot = session_runtime(&runtime_deps, &session_id).await?;
        if let Some(request) = snapshot.waiting_permission
            && answered_prompts.insert(request.id.clone())
        {
            let approved = prompt_oneshot_permission(&request.request, output.stderr()?)?;
            runtime_deps
                .resolve_permission_response(&app, &request.id, approved)
                .await?;
            continue;
        }
        let pending_ask = snapshot
            .waiting_ask_user
            .clone()
            .or_else(|| snapshot.open_questions.first().cloned());
        if let Some(request) = pending_ask
            && answered_prompts.insert(request.id.clone())
        {
            if snapshot
                .waiting_ask_user
                .as_ref()
                .is_some_and(|w| w.id == request.id)
            {
                answer_oneshot_questions(&runtime_deps, &session_id, &request, &mut output).await?;
            } else {
                let questions = request
                    .questions
                    .iter()
                    .map(|question| may_agent::ask_user::Question {
                        section: question.section.clone(),
                        question: question.question.clone(),
                        option: question.option.clone(),
                        multi_select: question.multi_select,
                    })
                    .collect::<Vec<_>>();
                let answers = prompt_oneshot_questions(&questions, output.stderr()?)?;
                if !runtime_deps
                    .answer_open_question(&request.id, &answers)
                    .await?
                {
                    writeln!(
                        output.stderr()?,
                        "[notice] the question is no longer open; this answer was not sent"
                    )?;
                }
            }
            continue;
        }
        if let Some(request) = snapshot.waiting_workspace
            && answered_prompts.insert(request.id.clone())
        {
            let approved = prompt_oneshot_workspace(
                &may_agent::workspace_request::RequestWorkspaceArgs {
                    path: request.path.clone(),
                    reason: request.reason.clone(),
                },
                output.stderr()?,
            )?;
            if !matches!(
                runtime_deps
                    .resolve_workspace_response(&session_id, &request.id, approved)
                    .await?,
                ResponseCommit::Committed
            ) {
                writeln!(
                    output.stderr()?,
                    "[notice] the workspace request is no longer open; this answer was not sent"
                )?;
            }
            continue;
        }

        let event_future =
            futures_lite::future::or(async { handle.recv().await.map(Event::Agent) }, async {
                async_io::Timer::after(DAEMON_PROMPT_POLL).await;
                Some(Event::Poll)
            });
        let next = if assistant_response.text().is_empty() || !active_tool_calls.is_empty() {
            event_future.await
        } else if let Ok(event) = tokio::time::timeout(
            ONESHOT_IDLE_FINISH_TIMEOUT.saturating_sub(last_agent_event.elapsed()),
            event_future,
        )
        .await
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
                    RunOutcome::Completed,
                )
                .await;
            output.end_text_line()?;
            return Ok(());
        };
        let Some(event) = next else {
            break;
        };
        if matches!(event, Event::Agent(_)) {
            last_agent_event = std::time::Instant::now();
        }
        match event {
            Event::Agent(TuiEvent::Agent(result)) => {
                if let Ok(TimedEvent {
                    event: AgentEvent::ToolCallStart { id, .. },
                    ..
                }) = &result
                {
                    active_tool_calls.insert(id.clone());
                }
                if let Ok(TimedEvent {
                    event: AgentEvent::ToolCallEnd { id, .. },
                    ..
                }) = &result
                {
                    active_tool_calls.remove(id);
                }
                if let Ok(TimedEvent {
                    at,
                    event: AgentEvent::Text(ref text),
                }) = result
                {
                    assistant_response.push(text, at);
                    output.text(text)?;
                }
                output.event(&result)?;
                let run_failed = result.is_err();
                let ended = app.handle_agent_event(result).await;
                if ended {
                    runtime_deps
                        .finish_agent_task(
                            &mut app,
                            &mut agent_handle,
                            &mut pending_title_gen,
                            &assistant_response,
                            if run_failed {
                                RunOutcome::Failed
                            } else {
                                RunOutcome::Completed
                            },
                        )
                        .await;
                    if run_failed {
                        anyhow::bail!("oneshot agent run failed");
                    }
                    output.end_text_line()?;
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
                        RunOutcome::Completed,
                    )
                    .await;
                output.end_text_line()?;
                return Ok(());
            }
            Event::Poll
            | Event::Agent(
                TuiEvent::Compacted(_)
                | TuiEvent::ContextReport(_)
                | TuiEvent::ContextDump(_)
                | TuiEvent::ContextDumpSaved { .. },
            ) => {}
            Event::Agent(TuiEvent::Error(error)) => {
                writeln!(output.stderr()?, "[error] {error}")?;
                runtime_deps
                    .finish_agent_task(
                        &mut app,
                        &mut None,
                        &mut pending_title_gen,
                        &assistant_response,
                        RunOutcome::Failed,
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
                "Enter comma-separated numbers, or any other text for a custom answer:"
            )?;
        } else {
            writeln!(
                stderr,
                "Enter a number, or any other text for a custom answer:"
            )?;
        }
        stderr.flush()?;
        let answer = loop {
            let input = read_oneshot_line()?;
            let trimmed = input.trim();
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

#[derive(askama::Template)]
#[template(path = "reminder_oneshot_noninteractive.txt", escape = "none")]
struct OneShotNonInteractiveReminder;

#[derive(Debug, Clone)]
struct OneShotPromptInput {
    content: String,
    hidden_context_xml: Option<may_agent::XmlBlock>,
}

async fn build_oneshot_prompt_with_mentions(
    app: &App,
    prompt: &str,
) -> anyhow::Result<OneShotPromptInput> {
    let mut hidden_context_parts = vec![askama::Template::render(&OneShotNonInteractiveReminder)?];
    let Some(session) = app.session_manager.current() else {
        return Ok(OneShotPromptInput {
            content: prompt.to_string(),
            hidden_context_xml: Some(may_agent::XmlBlock::parse(
                hidden_context_parts.join("\n\n"),
            )?),
        });
    };

    if let Some(client) = app.btw_daemon.as_ref()
        && let Ok(Some(session_mentions_xml)) =
            may_agent::prompt_mentions::build_session_mentions_remote(client, prompt).await
    {
        hidden_context_parts.push(session_mentions_xml);
    }

    let session_paths = may_agent::session::SessionPaths::new(session.sandbox_dir.clone());
    let files_dir = session_paths.files_dir();
    if let Ok(Some(file_mentions_xml)) =
        may_agent::prompt_mentions::build_file_mentions(&files_dir, prompt, 400).await
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

#[cfg(test)]
mod tests {
    use aither_agent::{AgentEvent, TimedEvent};
    use aither_core::llm::tool::{
        CommandOutcome, CommandResult, CommandStream, StoredFile, ToolResult,
    };
    use jiff::Timestamp;

    use super::parse_oneshot_selection;
    use super::render::OneShotOutput;

    type Output = OneShotOutput<Vec<u8>, Vec<u8>>;

    fn output() -> Output {
        OneShotOutput::new(Vec::new(), Vec::new())
    }

    fn timed(at: &str, event: AgentEvent) -> TimedEvent {
        TimedEvent {
            at: at.parse().expect("valid time"),
            event,
        }
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8(output.stderr_bytes().to_vec()).expect("event lines are utf-8")
    }

    fn command(outcome: CommandOutcome) -> ToolResult {
        ToolResult::Command(Box::new(CommandResult {
            stdout: CommandStream::Inline {
                text: String::new(),
            },
            stderr: None,
            outcome,
        }))
    }

    fn terminal_call(output: &mut Output, arguments: &str, result: ToolResult) {
        output
            .event(&Ok(timed(
                "2024-03-01T10:00:01Z",
                AgentEvent::tool_start("c1", "terminal", arguments),
            )))
            .expect("start renders");
        output
            .event(&Ok(timed(
                "2024-03-01T10:00:02.250Z",
                AgentEvent::ToolCallEnd {
                    id: "c1".to_string(),
                    name: "terminal".to_string(),
                    result,
                },
            )))
            .expect("end renders");
    }

    #[test]
    fn event_lines_carry_the_emission_time_as_rfc3339_utc() {
        let mut output = output();
        let at: Timestamp = "2024-03-01T10:00:01.250Z".parse().expect("valid time");
        output
            .event(&Ok(TimedEvent {
                at,
                event: AgentEvent::run_start("run", 1),
            }))
            .expect("render must succeed");
        let line = stderr(&output);
        let (stamp, rest) = line.split_once(' ').expect("line has a timestamp prefix");
        assert_eq!(stamp, "2024-03-01T10:00:01.25Z");
        assert!(stamp.ends_with('Z'));
        assert_eq!(stamp.parse::<Timestamp>().expect("valid RFC 3339"), at);
        assert_eq!(rest, "Running...\n");
    }

    #[test]
    fn a_run_error_is_stamped_between_markers_taken_around_its_receipt() {
        let mut output = output();
        let before = Timestamp::now();
        output
            .event(&Err(aither_agent::AgentError::Llm("boom".to_string())))
            .expect("render must succeed");
        let after = Timestamp::now();
        let line = stderr(&output);
        let (stamp, rest) = line.split_once(' ').expect("line has a timestamp prefix");
        let at = stamp.parse::<Timestamp>().expect("valid RFC 3339");
        assert!(stamp.ends_with('Z'));
        assert!(before <= at && at <= after);
        assert!(rest.starts_with("Error:"));
    }

    fn usage_line(usage: aither_core::llm::Usage) -> String {
        let mut output = output();
        output
            .event(&Ok(TimedEvent {
                at: Timestamp::UNIX_EPOCH,
                event: AgentEvent::Usage(usage),
            }))
            .expect("usage renders");
        stderr(&output)
            .split_once(' ')
            .expect("line has a timestamp prefix")
            .1
            .to_string()
    }

    #[test]
    fn the_usage_line_names_cache_reads_and_writes() {
        let usage = aither_core::llm::Usage {
            prompt_tokens: Some(1200),
            completion_tokens: Some(80),
            total_tokens: Some(1280),
            cache_read_tokens: Some(1000),
            cache_write_tokens: Some(150),
            ..Default::default()
        };
        assert_eq!(
            usage_line(usage),
            "Usage: prompt=1200 cache_read=1000 cache_write=150 completion=80 total=1280\n"
        );
    }

    #[test]
    fn the_usage_line_omits_fields_the_provider_did_not_report() {
        let usage = aither_core::llm::Usage {
            prompt_tokens: Some(1200),
            completion_tokens: Some(80),
            total_tokens: Some(1280),
            ..Default::default()
        };
        assert_eq!(
            usage_line(usage),
            "Usage: prompt=1200 completion=80 total=1280\n"
        );
    }

    #[test]
    fn a_terminal_call_that_exits_zero_shows_its_description_duration_and_exit_code() {
        let mut output = output();
        terminal_call(
            &mut output,
            r#"{"description":"list sources","script":"ls src"}"#,
            command(CommandOutcome::Exited { code: 0 }),
        );
        assert_eq!(
            stderr(&output),
            "2024-03-01T10:00:01Z Tool: terminal: list sources\n\
             2024-03-01T10:00:02.25Z Tool finished: terminal: list sources in 1.2s, exit code 0\n"
        );
    }

    #[test]
    fn a_terminal_call_without_a_description_shows_a_short_form_of_its_script() {
        let mut output = output();
        terminal_call(
            &mut output,
            r#"{"script":"cargo test\n  --all"}"#,
            command(CommandOutcome::Exited { code: 101 }),
        );
        assert_eq!(
            stderr(&output),
            "2024-03-01T10:00:01Z Tool: terminal: cargo test …\n\
             2024-03-01T10:00:02.25Z Tool finished: terminal: cargo test … in 1.2s, exit code 101\n"
        );
    }

    #[test]
    fn a_background_terminal_call_names_its_task() {
        let mut output = output();
        terminal_call(
            &mut output,
            r#"{"description":"serve","script":"serve"}"#,
            command(CommandOutcome::Running {
                task_id: "t7".to_string(),
                output: StoredFile {
                    url: "outputs/t7.txt".to_string(),
                    path: "/sandbox/outputs/t7.txt".to_string(),
                },
            }),
        );
        assert!(stderr(&output).ends_with(
            "Tool finished: terminal: serve in 1.2s, running in background as task t7\n"
        ));
    }

    #[test]
    fn a_killed_terminal_call_and_a_failed_tool_state_their_outcome() {
        let mut killed = output();
        terminal_call(
            &mut killed,
            r#"{"description":"wait","script":"sleep 99"}"#,
            command(CommandOutcome::Killed {
                task_id: "t9".to_string(),
                notice: "Killed at the time bound.".to_string(),
            }),
        );
        assert!(stderr(&killed).ends_with("in 1.2s, killed (task t9)\n"));

        let mut failed = output();
        terminal_call(
            &mut failed,
            r#"{"description":"wait","script":"sleep 99"}"#,
            ToolResult::error("permission denied"),
        );
        assert!(
            stderr(&failed).ends_with("Tool failed: terminal: wait in 1.2s: permission denied\n")
        );
    }

    #[test]
    fn another_tool_start_line_names_the_tool_and_summarises_its_arguments() {
        let mut output = output();
        output
            .event(&Ok(timed(
                "2024-03-01T10:00:01Z",
                AgentEvent::tool_start("c2", "read_page", r#"{"url":"https://example.com"}"#),
            )))
            .expect("start renders");
        assert_eq!(
            stderr(&output),
            "2024-03-01T10:00:01Z Tool: read_page: {\"url\":\"https://example.com\"}\n"
        );
    }

    #[test]
    fn assistant_text_ends_its_line_before_the_next_event_line() {
        let mut output = output();
        output.text("for arrays and strings:").expect("text writes");
        output
            .event(&Ok(timed(
                "2024-03-01T10:00:01Z",
                AgentEvent::run_start("run", 1),
            )))
            .expect("event renders");
        output.text("done.\n").expect("text writes");
        output
            .event(&Ok(timed(
                "2024-03-01T10:00:02Z",
                AgentEvent::run_start("run", 2),
            )))
            .expect("event renders");
        output.end_text_line().expect("nothing is open");
        assert_eq!(
            String::from_utf8(output.stdout_bytes().to_vec()).expect("utf-8"),
            "for arrays and strings:\ndone.\n"
        );
    }

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
