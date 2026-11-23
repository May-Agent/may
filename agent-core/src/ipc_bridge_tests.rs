//! The `IpcProvider` bridge tests that need `may-agent`'s `TestTerminal`:
//! a real session shell serving the commands a `CommandRegistry` holds.
#![cfg(all(test, unix))]

mod tests {
    use std::borrow::Cow;

    use aither_core::llm::{Tool, ToolContext, ToolResult};
    use schemars::JsonSchema;
    use serde::Deserialize;

    use crate::ipc::CommandRegistryBuilder;
    use crate::test_sandbox::TestTerminal;

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

        fn call(
            &self,
            args: Self::Arguments,
            _cx: ToolContext,
        ) -> impl Future<Output = aither_core::Result<Self::Res>> + Send {
            std::future::ready(Ok(ToolResult::text(format!(
                "ran prompt={} input={} body={}",
                args.prompt,
                args.input,
                args.body.unwrap_or_default()
            ))))
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
    /// command; it is not a help request. `note` declares a stdin field, so
    /// it reads its stdin to the end: the script gives it an empty one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dash_led_option_value_runs_the_command() {
        let output = run_note("note first --body \"- fixed the crash\" </dev/null").await;
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
