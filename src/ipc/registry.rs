//! The registry of May's commands and the per-launch view a script calls.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use aither_core::llm::ToolCallId;
use aither_core::llm::tool::ToolDefinition;
use aither_core::llm::{
    AgentIdentity, CallIdentity, IntoToolResult, Tool, ToolContext, ToolResult,
};
use aither_sandbox::{LaunchInfo, OutputStore, SandboxMode};
use askama::Template;
use async_channel::{Receiver, Sender};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::SessionShell;
use super::cli::{cli_to_json, detect_stdin_arg, schema_to_help, validate_positional_layout};
use super::command_set::{AnyCommandSet, CommandSet};
use super::payload::{
    CommandEnvelope, ReservedForMain, ToolUsageError, UnknownCommand, answer, tool_usage_error,
};

/// Which side of the authority boundary a launch is on: the main agent's, or
/// a subagent's at any nesting depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The main agent.
    Main,
    /// A subagent, or a subagent of one.
    Subagent,
}

impl Role {
    /// The role of the agent a launch runs for; a launch no tool call asked
    /// for (a scheduled script, `may exec`) is the main agent's.
    #[must_use]
    pub const fn of(launch: &LaunchInfo) -> Self {
        match &launch.call {
            Some(call) => match call.agent() {
                AgentIdentity::Main => Self::Main,
                AgentIdentity::Subagent(_) => Self::Subagent,
            },
            None => Self::Main,
        }
    }
}

/// What a command's handler is built from: the launch that calls it and the
/// shell the commands serve.
#[derive(Debug, Clone, Copy)]
pub struct Binding<'a> {
    /// The launch of the process that makes the call.
    pub launch: &'a LaunchInfo,
    /// The main agent's shell, from which `subagent` and `btw` launch other
    /// agents; `None` for a registry no shell serves.
    pub shell: Option<&'a SessionShell>,
}

impl Binding<'_> {
    /// The role of the agent the launch runs for.
    #[must_use]
    pub const fn role(&self) -> Role {
        Role::of(self.launch)
    }
}

/// Who invoked a command and from where.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    /// The `terminal` call whose script invoked the command.
    pub call: Option<CallIdentity>,
    /// The working directory of the process that made the request.
    pub working_dir: Option<String>,
}

impl Caller {
    fn into_context(self) -> ToolContext {
        let mut cx = ToolContext::new();
        if let Some(identity) = self.call {
            cx = cx.with_identity(identity);
        }
        if let Some(working_dir) = self.working_dir {
            cx = cx.with_working_dir(working_dir);
        }
        cx
    }
}

type HandlerFuture = Pin<Box<dyn Future<Output = Result<ToolResult, String>> + Send>>;
type Handler = Arc<dyn Fn(Caller, Vec<String>) -> HandlerFuture + Send + Sync>;
type Factory = Arc<dyn Fn(&Binding<'_>) -> Handler + Send + Sync>;
type UsageFn<T> = fn(Arc<T>) -> Pin<Box<dyn Future<Output = String> + Send>>;

/// A terminal command whose valid arguments depend on what exists when it is
/// called, such as the names a lookup can resolve.
///
/// A call that does not parse is answered with the parse error followed by
/// [`usage`](Self::usage), read at the moment of that call.
pub trait CommandUsage: Tool {
    /// What the command's arguments can be right now.
    fn usage(&self) -> impl Future<Output = String> + Send;
}

fn command_usage<T: CommandUsage + 'static>(
    tool: Arc<T>,
) -> Pin<Box<dyn Future<Output = String> + Send>> {
    Box::pin(async move { tool.usage().await })
}

/// The modes a command answers in.
#[derive(Debug, Clone, Copy)]
enum Modes {
    Every,
    /// Sandboxed alone: an unsafe script has no such command and resolves the
    /// name on the host's `PATH`.
    Sandboxed,
}

impl Modes {
    const fn admits(self, mode: SandboxMode) -> bool {
        matches!(
            (self, mode),
            (Self::Every, _) | (Self::Sandboxed, SandboxMode::Sandboxed)
        )
    }
}

/// What the subagent role has of a command.
#[derive(Clone)]
enum SubagentSide {
    /// The same handler as the main agent's.
    Same,
    /// Its own handler.
    Serves(Factory),
    /// Reserved: a subagent's call gets the refusal naming it so.
    Reserved,
    /// Absent: neither listed nor callable.
    Absent,
}

#[derive(Clone)]
struct Entry {
    help: String,
    stdin_arg: Option<String>,
    main: Factory,
    subagent: SubagentSide,
    modes: Modes,
}

impl Entry {
    fn exists(&self, role: Role, mode: SandboxMode) -> bool {
        self.modes.admits(mode)
            && !(role == Role::Subagent && matches!(self.subagent, SubagentSide::Absent))
    }

    fn factory(&self, role: Role) -> Option<&Factory> {
        match (role, &self.subagent) {
            (Role::Main, _) | (Role::Subagent, SubagentSide::Same) => Some(&self.main),
            (Role::Subagent, SubagentSide::Serves(factory)) => Some(factory),
            (Role::Subagent, SubagentSide::Reserved | SubagentSide::Absent) => None,
        }
    }
}

/// One command call a registry served.
#[derive(Debug, Clone, Copy)]
pub struct CommandCall<'a> {
    /// The role of the launch that made the call.
    pub role: Role,
    /// The resolved command name.
    pub name: &'a str,
    /// The `terminal` call whose script invoked the command.
    pub call: Option<&'a CallIdentity>,
    /// The arguments the command was called with.
    pub args: &'a [String],
    /// What the call answered.
    pub outcome: &'a CommandEnvelope,
}

/// Watches the commands a registry serves; awaited after the command has
/// answered and before the answer returns, and unable to change it.
pub trait CommandObserver: Send + Sync + 'static {
    /// Called once for every served call of a resolved command.
    fn command_called(&self, call: &CommandCall<'_>) -> impl Future<Output = ()> + Send;
}

trait CommandObserverImpl: Send + Sync {
    fn command_called<'a>(
        &'a self,
        call: &'a CommandCall<'a>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

impl<T: CommandObserver> CommandObserverImpl for T {
    fn command_called<'a>(
        &'a self,
        call: &'a CommandCall<'a>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(CommandObserver::command_called(self, call))
    }
}

/// A type-erased [`CommandObserver`].
#[derive(Clone)]
pub struct AnyCommandObserver(Arc<dyn CommandObserverImpl>);

impl AnyCommandObserver {
    /// Erases `observer`.
    #[must_use]
    pub fn new(observer: impl CommandObserver) -> Self {
        Self(Arc::new(observer))
    }
}

impl std::fmt::Debug for AnyCommandObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AnyCommandObserver")
    }
}

/// Receiver for the `terminal` calls in which a command registered with
/// [`CommandRegistryBuilder::kept_through_compaction`] succeeded.
#[derive(Clone)]
pub struct KeptExchangeReceiver {
    rx: Receiver<ToolCallId>,
}

impl KeptExchangeReceiver {
    /// Takes the pending call ids without blocking.
    #[must_use]
    pub fn take_pending(&self) -> Vec<ToolCallId> {
        let mut calls = Vec::new();
        while let Ok(call) = self.rx.try_recv() {
            calls.push(call);
        }
        calls
    }
}

impl std::fmt::Debug for KeptExchangeReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeptExchangeReceiver")
            .field("pending", &self.rx.len())
            .finish()
    }
}

fn tool_schema<A: JsonSchema>() -> Value {
    checked_schema(
        serde_json::to_value(schemars::schema_for!(A)).expect("a tool schema serializes"),
    )
}

fn live_tool_schema<T: Tool>(tool: &T) -> Value {
    checked_schema(serde_json::to_value(tool.arguments_schema()).expect("a tool schema serializes"))
}

fn checked_schema(schema: Value) -> Value {
    if let Err(error) = validate_positional_layout(&schema) {
        panic!("invalid command schema: {error}");
    }
    schema
}

fn tool_handler<T>(tool: Arc<T>, schema: Value, usage: Option<UsageFn<T>>) -> Handler
where
    T: Tool + Send + Sync + 'static,
    T::Arguments: DeserializeOwned + JsonSchema + Send + 'static,
{
    let name = tool.name().to_string();
    Arc::new(move |caller: Caller, args: Vec<String>| {
        let tool = tool.clone();
        let schema = schema.clone();
        let name = name.clone();
        Box::pin(async move {
            tracing::debug!(tool = %name, ?args, "command: raw CLI args");
            let json_args = match cli_to_json(&schema, &args) {
                Ok(value) => value,
                Err(error) => {
                    let message = error.to_string();
                    let usage = match usage {
                        Some(usage) => Some(usage(tool.clone()).await),
                        None => None,
                    };
                    return Err(ToolUsageError {
                        message: &message,
                        usage: usage.as_deref(),
                        tool_name: &name,
                    }
                    .render()
                    .expect("the tool usage error template renders typed input"));
                }
            };
            let parsed = match serde_json::from_value(json_args) {
                Ok(parsed) => parsed,
                Err(error) => {
                    return Err(tool_usage_error(
                        &format!("invalid arguments: {error}"),
                        &name,
                    ));
                }
            };
            let output = tool
                .call(parsed, caller.into_context())
                .await
                .map_err(|error| error.to_string())?;
            output
                .into_tool_result()
                .map_err(|error| format!("failed to convert tool result: {error}"))
        }) as HandlerFuture
    })
}

fn tool_entry<T>(tool: T) -> (String, Entry)
where
    T: Tool + Send + Sync + 'static,
    T::Arguments: DeserializeOwned + JsonSchema + Send + 'static,
{
    let schema = live_tool_schema(&tool);
    let name = tool.name().to_string();
    let help = schema_to_help(&schema);
    let stdin_arg = detect_stdin_arg(&schema);
    let handler = tool_handler(Arc::new(tool), schema, None);
    (
        name,
        Entry {
            help,
            stdin_arg,
            main: Arc::new(move |_| handler.clone()),
            subagent: SubagentSide::Same,
            modes: Modes::Every,
        },
    )
}

fn definition_entry(definition: &ToolDefinition, handler: Handler) -> (String, Entry) {
    let name = definition.name().to_string();
    let description = definition.description().to_string();
    let schema = checked_schema(definition.arguments_openai_schema());
    let stdin_arg = detect_stdin_arg(&schema);
    let usage = schema_to_help(&schema);
    let help = if description.is_empty() {
        usage
    } else {
        format!("{description}\n\n{usage}")
    };
    (
        name,
        Entry {
            help,
            stdin_arg,
            main: Arc::new(move |_| handler.clone()),
            subagent: SubagentSide::Same,
            modes: Modes::Every,
        },
    )
}

/// Builds a [`CommandRegistry`].
#[derive(Default)]
pub struct CommandRegistryBuilder {
    entries: HashMap<String, Entry>,
    sets: Vec<AnyCommandSet>,
    observer: Option<AnyCommandObserver>,
    kept: HashSet<String>,
}

impl CommandRegistryBuilder {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool both roles call in every mode.
    ///
    /// # Panics
    /// Panics if the tool's argument schema cannot be serialized or lays out
    /// its positional fields invalidly.
    pub fn configure_tool<T>(&mut self, tool: T)
    where
        T: Tool + Send + Sync + 'static,
        T::Arguments: DeserializeOwned + JsonSchema + Send + 'static,
    {
        let (name, entry) = tool_entry(tool);
        self.entries.insert(name, entry);
    }

    /// Registers one command with a handler per role.
    ///
    /// # Panics
    /// Panics if the tools do not share a name.
    pub fn configure_role_tools<M, S>(&mut self, main: M, subagent: S)
    where
        M: Tool + Send + Sync + 'static,
        M::Arguments: DeserializeOwned + JsonSchema + Send + 'static,
        S: Tool + Send + Sync + 'static,
        S::Arguments: DeserializeOwned + JsonSchema + Send + 'static,
    {
        let (name, mut entry) = tool_entry(main);
        let (subagent_name, subagent) = tool_entry(subagent);
        assert_eq!(
            name, subagent_name,
            "a command's role handlers must share its name"
        );
        entry.subagent = SubagentSide::Serves(subagent.main);
        self.entries.insert(name, entry);
    }

    /// Registers a tool built for each call from the launch's [`Binding`];
    /// the built tool is named `name`.
    ///
    /// # Panics
    /// Panics if the argument schema cannot be serialized, and, when a call
    /// builds the tool, if it is not named `name`.
    pub fn configure_bound_tool<T, F>(&mut self, name: impl Into<String>, bind: F)
    where
        F: Fn(&Binding<'_>) -> T + Send + Sync + 'static,
        T: Tool + Send + Sync + 'static,
        T::Arguments: DeserializeOwned + JsonSchema + Send + 'static,
    {
        self.insert_bound(name.into(), bind, None);
    }

    /// Like [`configure_bound_tool`](Self::configure_bound_tool), for a tool
    /// whose parse failures also print its [`CommandUsage::usage`].
    pub fn configure_bound_usage_tool<T, F>(&mut self, name: impl Into<String>, bind: F)
    where
        F: Fn(&Binding<'_>) -> T + Send + Sync + 'static,
        T: CommandUsage + Send + Sync + 'static,
        T::Arguments: DeserializeOwned + JsonSchema + Send + 'static,
    {
        self.insert_bound(name.into(), bind, Some(command_usage::<T>));
    }

    fn insert_bound<T, F>(&mut self, name: String, bind: F, usage: Option<UsageFn<T>>)
    where
        F: Fn(&Binding<'_>) -> T + Send + Sync + 'static,
        T: Tool + Send + Sync + 'static,
        T::Arguments: DeserializeOwned + JsonSchema + Send + 'static,
    {
        let schema = tool_schema::<T::Arguments>();
        let help = schema_to_help(&schema);
        let stdin_arg = detect_stdin_arg(&schema);
        let bound_name = name.clone();
        let factory: Factory = Arc::new(move |binding| {
            let tool = bind(binding);
            assert_eq!(
                tool.name(),
                bound_name,
                "a bound command keeps the name it was registered under"
            );
            tool_handler(Arc::new(tool), schema.clone(), usage)
        });
        self.entries.insert(
            name,
            Entry {
                help,
                stdin_arg,
                main: factory,
                subagent: SubagentSide::Same,
                modes: Modes::Every,
            },
        );
    }

    /// Registers a handler over the raw command line, for both roles.
    pub fn configure_raw_handler<F>(
        &mut self,
        name: impl Into<String>,
        help: impl Into<String>,
        handler: F,
    ) where
        F: Fn(Option<CallIdentity>, Vec<String>) -> HandlerFuture + Send + Sync + 'static,
    {
        let handler: Handler = Arc::new(move |caller, args| handler(caller.call, args));
        self.entries.insert(
            name.into(),
            Entry {
                help: help.into(),
                stdin_arg: None,
                main: Arc::new(move |_| handler.clone()),
                subagent: SubagentSide::Same,
                modes: Modes::Every,
            },
        );
    }

    /// Registers a command whose arguments are the schema of `definition`.
    pub fn configure_definition_handler<F>(&mut self, definition: &ToolDefinition, handler: F)
    where
        F: Fn(Option<CallIdentity>, Value) -> HandlerFuture + Send + Sync + 'static,
    {
        let schema = checked_schema(definition.arguments_openai_schema());
        let name = definition.name().to_string();
        let handler = Arc::new(handler);
        let raw: Handler = Arc::new(move |caller, args| {
            let schema = schema.clone();
            let name = name.clone();
            let handler = handler.clone();
            Box::pin(async move {
                match cli_to_json(&schema, &args) {
                    Ok(value) => handler(caller.call, value).await,
                    Err(error) => Err(tool_usage_error(&error.to_string(), &name)),
                }
            })
        });
        let (name, entry) = definition_entry(definition, raw);
        self.entries.insert(name, entry);
    }

    /// Registers every command `set` holds, for both roles in every mode.
    /// A name a command registered any other way has stays with that command.
    pub fn command_set(&mut self, set: impl CommandSet) {
        self.sets.push(AnyCommandSet::new(set));
    }

    /// Reserves for the main agent every command `configure` registers: a
    /// subagent's call is refused with a message naming the command.
    pub fn main_only(&mut self, configure: impl FnOnce(&mut Self)) {
        self.restrict(configure, |entry| entry.subagent = SubagentSide::Reserved);
    }

    /// Registers for the main agent alone what `configure` registers: a
    /// subagent has no such command.
    pub fn main_exclusive(&mut self, configure: impl FnOnce(&mut Self)) {
        self.restrict(configure, |entry| entry.subagent = SubagentSide::Absent);
    }

    /// Registers what `configure` registers for sandboxed scripts alone.
    pub fn sandboxed_only(&mut self, configure: impl FnOnce(&mut Self)) {
        self.restrict(configure, |entry| entry.modes = Modes::Sandboxed);
    }

    /// Marks every command `configure` registers as one whose exchanges are
    /// kept verbatim through compaction.
    pub fn kept_through_compaction(&mut self, configure: impl FnOnce(&mut Self)) {
        let mut registered = Self::new();
        configure(&mut registered);
        assert!(
            registered.sets.is_empty(),
            "a command set cannot be kept through compaction"
        );
        self.kept.extend(registered.entries.keys().cloned());
        self.entries.extend(registered.entries);
    }

    fn restrict(&mut self, configure: impl FnOnce(&mut Self), apply: impl Fn(&mut Entry)) {
        let mut scoped = Self::new();
        configure(&mut scoped);
        assert!(
            scoped.sets.is_empty(),
            "a command set serves every role and mode"
        );
        self.kept.extend(scoped.kept);
        for (name, mut entry) in scoped.entries {
            apply(&mut entry);
            self.entries.insert(name, entry);
        }
    }

    /// Has `observer` told about every served call of a resolved command.
    pub fn observe_commands(&mut self, observer: AnyCommandObserver) {
        self.observer = Some(observer);
    }

    /// Builds the registry.
    #[must_use]
    pub fn build(self) -> CommandRegistry {
        let (kept_tx, kept_rx) = async_channel::unbounded();
        CommandRegistry {
            entries: Arc::new(self.entries),
            sets: self.sets.into(),
            observer: self.observer,
            kept: Arc::new(self.kept),
            kept_tx,
            kept_rx,
        }
    }
}

/// The commands May serves to its scripts. Cheap to clone; a clone shares the
/// kept-exchange channel.
#[derive(Clone)]
pub struct CommandRegistry {
    entries: Arc<HashMap<String, Entry>>,
    sets: Arc<[AnyCommandSet]>,
    observer: Option<AnyCommandObserver>,
    kept: Arc<HashSet<String>>,
    kept_tx: Sender<ToolCallId>,
    kept_rx: Receiver<ToolCallId>,
}

impl std::fmt::Debug for CommandRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandRegistry")
            .field("commands", &self.entries.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl CommandRegistry {
    /// The receiver of the `terminal` calls in which a kept command
    /// succeeded.
    #[must_use]
    pub fn kept_receiver(&self) -> KeptExchangeReceiver {
        KeptExchangeReceiver {
            rx: self.kept_rx.clone(),
        }
    }

    /// The command sets, in registration order.
    #[must_use]
    pub fn command_sets(&self) -> &[AnyCommandSet] {
        &self.sets
    }

    fn set_entry(&self, name: &str) -> Option<Entry> {
        if self.entries.contains_key(name) {
            return None;
        }
        self.sets.iter().find_map(|set| {
            let definition = set
                .definitions()
                .into_iter()
                .find(|definition| definition.name() == name)?;
            let set = set.clone();
            let command = name.to_string();
            let handler = Arc::new(move |call, arguments| {
                let set = set.clone();
                let command = command.clone();
                Box::pin(async move { set.call(&command, call, arguments).await }) as HandlerFuture
            });
            let schema = checked_schema(definition.arguments_openai_schema());
            let tool = name.to_string();
            let raw: Handler = Arc::new(move |caller, args| {
                let schema = schema.clone();
                let tool = tool.clone();
                let handler = handler.clone();
                Box::pin(async move {
                    match cli_to_json(&schema, &args) {
                        Ok(value) => handler(caller.call, value).await,
                        Err(error) => Err(tool_usage_error(&error.to_string(), &tool)),
                    }
                })
            });
            Some(definition_entry(&definition, raw).1)
        })
    }

    fn set_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for set in self.sets.iter() {
            for definition in set.definitions() {
                let name = definition.name();
                if !self.entries.contains_key(name) && !names.iter().any(|known| known == name) {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    /// The names the commands of `launch` are called by, reserved ones
    /// included.
    #[must_use]
    pub fn names(&self, launch: &LaunchInfo) -> Vec<String> {
        let (role, mode) = (Role::of(launch), launch.mode);
        let mut names: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.exists(role, mode))
            .map(|(name, _)| name.clone())
            .collect();
        names.extend(self.set_names());
        names
    }

    /// Whether `name` is a command `launch` can call.
    #[must_use]
    pub fn is_configured(&self, launch: &LaunchInfo, name: &str) -> bool {
        self.entries.get(name).map_or_else(
            || self.set_names().iter().any(|set| set == name),
            |entry| entry.exists(Role::of(launch), launch.mode),
        )
    }

    /// Whether `name` is reserved for the main agent in `launch`'s role.
    #[must_use]
    pub fn is_reserved(&self, launch: &LaunchInfo, name: &str) -> bool {
        self.entries
            .get(name)
            .is_some_and(|entry| entry.factory(Role::of(launch)).is_none())
    }

    /// The help text of `name` as `launch` sees it.
    #[must_use]
    pub fn help(&self, launch: &LaunchInfo, name: &str) -> Option<String> {
        match self.entries.get(name) {
            Some(entry) => entry
                .exists(Role::of(launch), launch.mode)
                .then(|| entry.help.clone()),
            None => self.set_entry(name).map(|entry| entry.help),
        }
    }

    /// The stdin argument of `name` as `launch` sees it.
    #[must_use]
    pub fn stdin_arg(&self, launch: &LaunchInfo, name: &str) -> Option<String> {
        match self.entries.get(name) {
            Some(entry) => entry
                .exists(Role::of(launch), launch.mode)
                .then(|| entry.stdin_arg.clone())
                .flatten(),
            None => self.set_entry(name).and_then(|entry| entry.stdin_arg),
        }
    }

    /// Runs the command `name` for the process `launch` describes, which was
    /// in `working_dir` when it made the request.
    pub async fn call(
        &self,
        launch: &LaunchInfo,
        shell: Option<&SessionShell>,
        outputs: &OutputStore,
        name: &str,
        args: &[String],
        working_dir: Option<&Path>,
    ) -> CommandEnvelope {
        let (role, mode) = (Role::of(launch), launch.mode);
        let binding = Binding { launch, shell };
        let handler = match self
            .entries
            .get(name)
            .filter(|entry| entry.exists(role, mode))
        {
            Some(entry) => match entry.factory(role) {
                Some(factory) => factory(&binding),
                None => {
                    return CommandEnvelope::failure(
                        ReservedForMain { name }
                            .render()
                            .expect("the reserved-for-main template renders typed input"),
                    );
                }
            },
            None => match self.set_entry(name) {
                Some(entry) => (entry.main)(&binding),
                None => {
                    return CommandEnvelope::failure(
                        UnknownCommand { name }
                            .render()
                            .expect("the unknown-command template renders typed input"),
                    );
                }
            },
        };
        let caller = Caller {
            call: launch.call.clone(),
            working_dir: working_dir.map(|dir| dir.to_string_lossy().into_owned()),
        };
        let outcome = match handler(caller, args.to_vec()).await {
            Ok(output) => answer(output, outputs).await,
            Err(error) => {
                tracing::warn!(tool = %name, %error, "command failed");
                CommandEnvelope::failure(error)
            }
        };
        if outcome.ok
            && self.kept.contains(name)
            && let Some(call) = &launch.call
        {
            self.kept_tx
                .try_send(call.call().clone())
                .expect("the registry holds its kept channel open and unbounded");
        }
        if let Some(observer) = &self.observer {
            observer
                .0
                .command_called(&CommandCall {
                    role,
                    name,
                    call: launch.call.as_ref(),
                    args,
                    outcome: &outcome,
                })
                .await;
        }
        outcome
    }
}
