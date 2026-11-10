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
use super::cli::{flatten_args_to_cli, has_help_flag};
use super::payload::CommandEnvelope;
use super::registry::CommandRegistry;

/// The shell a bridge serves, set once the shell is built (the shell is
/// built with the bridge, so the bridge cannot hold it earlier). The slot
/// holds the shell weakly: the shell owns the bridge, so a strong reference
/// would keep both alive forever, and the shell lives as long as its owner
/// holds it.
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
        if has_help_flag(&cli_args) {
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
