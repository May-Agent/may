//! The [`IpcProvider`] that serves May's commands to a shell's scripts.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

use aither_sandbox::{IpcProvider, LaunchInfo, OutputStore};
use arc_swap::ArcSwapWeak;
use heel::ipc::{IpcContext, IpcError};
use heel::{IpcCommand, IpcRouter};
use serde_json::Value;

use super::SessionShell;
use super::cli::flatten_args_to_cli;
use super::payload::CommandEnvelope;
use super::registry::CommandRegistry;

/// The shell a bridge serves, set once the shell is built.
///
/// The shell is built with the bridge, so the bridge cannot hold it earlier.
/// The slot holds the shell weakly: the shell owns the bridge, so a strong
/// reference would keep both alive forever, and the shell lives as long as
/// its owner holds it.
#[derive(Debug, Clone)]
pub struct ShellSlot(Arc<ArcSwapWeak<SessionShell>>);

impl Default for ShellSlot {
    fn default() -> Self {
        Self(Arc::new(ArcSwapWeak::new(Weak::new())))
    }
}

impl ShellSlot {
    /// An empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds the slot to `shell`, which stays owned by the caller.
    pub fn bind(&self, shell: &Arc<SessionShell>) {
        self.0.store(Arc::downgrade(shell));
    }

    fn get(&self) -> Option<Arc<SessionShell>> {
        self.0.load().upgrade()
    }
}

/// Serves a [`CommandRegistry`] to the scripts of one shell, and of the
/// subagent shells launched from it.
#[derive(Debug, Clone)]
pub struct CommandBridge {
    registry: CommandRegistry,
    slot: ShellSlot,
}

impl CommandBridge {
    /// A bridge serving `registry` to the shell `slot` is bound to.
    #[must_use]
    pub const fn new(registry: CommandRegistry, slot: ShellSlot) -> Self {
        Self { registry, slot }
    }
}

impl IpcProvider for CommandBridge {
    fn router(&self, launch: &LaunchInfo) -> Option<IpcRouter> {
        let shell = self.slot.get()?;
        let view = Arc::new(LaunchView {
            registry: self.registry.clone(),
            launch: launch.clone(),
            outputs: shell.output_store().clone(),
            shell,
        });
        let router =
            self.registry
                .names(launch)
                .into_iter()
                .fold(IpcRouter::new(), |router, name| {
                    router.register(ToolCallCommand {
                        name,
                        view: view.clone(),
                    })
                });
        Some(router)
    }
}

/// What one launch's commands are called with.
struct LaunchView {
    registry: CommandRegistry,
    launch: LaunchInfo,
    outputs: OutputStore,
    shell: Arc<SessionShell>,
}

/// One command of the router a launch is served.
struct ToolCallCommand {
    name: String,
    view: Arc<LaunchView>,
}

/// Renders a call's outcome the way the terminal caller expects it: content
/// on stdout for success, a transport error for failure so `heel ipc` exits
/// non-zero.
fn envelope_to_cli(envelope: CommandEnvelope) -> Result<String, IpcError> {
    if !envelope.ok {
        return Err(IpcError::remote(
            envelope
                .error
                .unwrap_or_else(|| "command failed".to_string()),
        ));
    }
    Ok(envelope
        .payload
        .map_or_else(String::new, |payload| payload.render_for_cli()))
}

impl IpcCommand for ToolCallCommand {
    type Args = HashMap<String, Value>;
    type Response = String;

    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.name.clone())
    }

    /// The schema parser on this side binds positionals itself, so every
    /// word goes through.
    fn raw_args(&self) -> bool {
        true
    }

    fn stdin_arg(&self) -> Option<Cow<'static, str>> {
        self.view
            .registry
            .stdin_arg(&self.view.launch, &self.name)
            .map(Cow::Owned)
    }

    async fn handle(&self, args: Self::Args, context: IpcContext) -> Result<String, IpcError> {
        let cli_args = flatten_args_to_cli(&args);
        let view = &self.view;
        if view
            .registry
            .requests_help(&view.launch, &self.name, &cli_args)
        {
            let help = view
                .registry
                .help(&view.launch, &self.name)
                .unwrap_or_else(|| format!("no help available for '{}'", self.name));
            return Ok(help);
        }
        // A command that panics fails its own call and nothing else.
        let call = view.registry.call(
            &view.launch,
            Some(&view.shell),
            &view.outputs,
            &self.name,
            &cli_args,
            Some(context.working_dir()),
        );
        let envelope =
            futures_lite::future::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(call))
                .await
                .unwrap_or_else(|_| {
                    tracing::error!(command = %self.name, "a command panicked");
                    CommandEnvelope::failure(format!(
                        "the `{}` command failed unexpectedly",
                        self.name
                    ))
                });
        envelope_to_cli(envelope)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::borrow::Cow;

    use aither_core::llm::{Tool, ToolContext, ToolResult};
    use schemars::JsonSchema;
    use serde::Deserialize;

    use crate::ipc::CommandRegistryBuilder;
    use crate::repo_clone::test_sandbox::TestTerminal;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct NoteArgs {
        /// The first word.
        prompt: String,
        /// Text piped on stdin.
        #[serde(default)]
        input: String,
        /// A body.
        #[serde(default)]
        body: Option<String>,
    }

    /// A command that reports the arguments it parsed.
    struct NoteTool;

    impl Tool for NoteTool {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("note")
        }

        type Arguments = NoteArgs;
        type Res = ToolResult;

        async fn call(
            &self,
            args: Self::Arguments,
            _cx: ToolContext,
        ) -> aither_core::Result<Self::Res> {
            Ok(ToolResult::text(format!(
                "ran prompt={} input={} body={}",
                args.prompt,
                args.input,
                args.body.unwrap_or_default()
            )))
        }
    }

    /// Runs `script` against a terminal that serves `note`.
    async fn run_note(script: &str) -> String {
        let mut commands = CommandRegistryBuilder::new();
        commands.configure_tool(NoteTool);
        let session = tempfile::tempdir().expect("session dir");
        let terminal = TestTerminal::with_commands(session.path(), commands).await;
        terminal
            .sandbox()
            .run(session.path(), script)
            .await
            .expect("the script runs to its end")
    }

    /// An option's value that starts with a dash and holds an `h` runs the
    /// command; it is not a help request.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dash_led_option_value_runs_the_command() {
        let output = run_note("note first --body \"- fixed the crash\"").await;
        assert_eq!(
            output.trim(),
            "ran prompt=first input= body=- fixed the crash"
        );
    }

    /// Text piped on stdin that starts with a list item reaches the command
    /// as its input and does not print usage.
    #[tokio::test(flavor = "multi_thread")]
    async fn piped_text_is_never_read_as_a_flag() {
        let output = run_note("printf -- '- the notes' | note translate").await;
        assert_eq!(
            output.trim(),
            "ran prompt=translate input=- the notes body="
        );
    }

    /// `-h` and `--help` still print usage without running the command.
    #[tokio::test(flavor = "multi_thread")]
    async fn help_flags_print_usage() {
        for flag in ["-h", "--help"] {
            let output = run_note(&format!("note {flag}")).await;
            assert!(output.contains("Options:"), "{flag}: {output}");
            assert!(!output.contains("ran prompt="), "{flag}: {output}");
        }
    }

    /// A command that panics fails its own call with a message, and the
    /// session's shell goes on serving the commands that do not.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_command_fails_its_own_call_and_the_session_continues() {
        let mut commands = CommandRegistryBuilder::new();
        commands.configure_raw_handler("boom", "Panics", |_call, _args| {
            Box::pin(async { panic!("the command's own bug") })
        });
        commands.configure_raw_handler("fine", "Succeeds", |_call, _args| {
            Box::pin(async { Ok(ToolResult::text("still serving")) })
        });
        let session = tempfile::tempdir().expect("session dir");
        let terminal = TestTerminal::with_commands(session.path(), commands).await;
        let sandbox = terminal.sandbox();

        let output = sandbox
            .run(
                session.path(),
                "boom 2>&1 && echo boom=succeeded || echo boom=failed\nboom 2>&1 || true\nfine",
            )
            .await
            .expect("the script runs to its end");
        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            [
                "error: IPC error: the `boom` command failed unexpectedly",
                "boom=failed",
                "error: IPC error: the `boom` command failed unexpectedly",
                "still serving"
            ],
            "{output}"
        );
    }
}
