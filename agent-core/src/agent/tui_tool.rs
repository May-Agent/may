//! The `tui` IPC command for driving interactive terminal applications in a PTY.
//!
//! Pairs virtual terminal emulation and structured screen observation with
//! `computer`-aligned input encoding and compact PNG cell rasterization that
//! bypasses redundant OCR.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use aither_core::llm::{IntoToolResult, Tool, ToolContext, ToolResult};
use aither_sandbox::{Interpreter, Job, LaunchInfo, LiveDuration, PtyArgs, PtyJob};
use askama::Template;
use executor_core::{DefaultExecutor, Executor as _};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ipc::{IpcImageTool, RecordedCallImage, SessionShell};
use crate::tui_driver::{
    CellRasterizer, DEFAULT_COLS, DEFAULT_ROWS, DEFAULT_SETTLE_QUIET_MS, DEFAULT_SETTLE_TIMEOUT_MS,
    ElementTable, FrameWaiter, RasterizeError, ScreenSnapshot, TerminalEmulator, TerminalModes,
    TuiElementRef, TuiInputError, encode_key, encode_mouse_click, encode_mouse_double_click,
    encode_mouse_drag, encode_mouse_right_click, encode_mouse_scroll, encode_type,
    find_unique_text, observe_screen, render_observation,
};

/// Default lifetime in seconds for a `tui open` job when `--timeout` is omitted.
const DEFAULT_TUI_TIMEOUT_SECS: u64 = 1_800;

/// Maximum exited TUI sessions retained in memory for post-exit observation.
const MAX_RETAINED_EXITED: usize = 16;

/// Summary of an open TUI session reported to context sources and turn guards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiSessionSummary {
    /// Real OS PID inside the execution environment.
    pub pid: u32,
    /// Script or command that launched the TUI.
    pub command: String,
    /// Current terminal width in cells.
    pub cols: u16,
    /// Current terminal height in cells.
    pub rows: u16,
    /// Window title set via OSC 0/2, if any.
    pub title: Option<String>,
}

/// One live or recently exited TUI session owned by a [`TuiRegistry`].
pub struct TuiSession {
    pid: u32,
    command: String,
    emulator: Arc<Mutex<TerminalEmulator>>,
    waiter: FrameWaiter,
    last_table: Mutex<ElementTable>,
    job: Job,
    exited: Arc<AtomicBool>,
}

impl std::fmt::Debug for TuiSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiSession")
            .field("pid", &self.pid)
            .field("command", &self.command)
            .field("exited", &self.is_exited())
            .finish_non_exhaustive()
    }
}

impl TuiSession {
    /// Real OS PID of the TUI process.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the underlying PTY output stream has reached EOF.
    #[must_use]
    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Current terminal modes tracked from the VT stream.
    #[must_use]
    pub fn modes(&self) -> TerminalModes {
        self.emulator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .modes()
    }

    /// Current visible screen generation counter.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.emulator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation()
    }

    /// Captures an immutable snapshot of the current visible screen.
    #[must_use]
    pub fn snapshot(&self) -> ScreenSnapshot {
        self.emulator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot()
    }

    /// Summary of this session's dimensions, command, and window title.
    #[must_use]
    pub fn summary(&self) -> TuiSessionSummary {
        let guard = self
            .emulator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let title = guard.title();
        TuiSessionSummary {
            pid: self.pid,
            command: self.command.clone(),
            cols: guard.cols(),
            rows: guard.rows(),
            title: (!title.is_empty()).then(|| title.to_owned()),
        }
    }

    fn store_table(&self, table: ElementTable) {
        *self
            .last_table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = table;
    }

    fn resolve_ref(&self, reference: TuiElementRef) -> Result<(u16, u16), String> {
        let current_generation = self.generation();
        let table = self
            .last_table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        table
            .resolve(reference, current_generation)
            .map(|element| element.center)
            .map_err(|error| error.render_for_model())
    }

    async fn settle_current(&self) {
        let _ = self
            .waiter
            .wait_for_settle(
                Duration::from_millis(DEFAULT_SETTLE_QUIET_MS),
                Duration::from_millis(DEFAULT_SETTLE_TIMEOUT_MS),
            )
            .await;
    }

    async fn settle_after_action(&self) {
        let _ = self
            .waiter
            .wait_for_action_settle(
                Duration::from_millis(DEFAULT_SETTLE_QUIET_MS),
                Duration::from_millis(DEFAULT_SETTLE_TIMEOUT_MS),
            )
            .await;
    }
}

/// Registry of TUI sessions scoped to one agent session (or one writable
/// subagent run). Cheap to clone; clones share the same underlying state.
#[derive(Debug, Clone, Default)]
pub struct TuiRegistry {
    sessions: Arc<Mutex<HashMap<u32, Arc<TuiSession>>>>,
    rasterizer: Arc<OnceLock<Arc<CellRasterizer>>>,
}

impl TuiRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a newly spawned [`PtyJob`] and starts its background VT reader task.
    #[must_use]
    pub fn register(
        &self,
        command: String,
        cols: u16,
        rows: u16,
        pty_job: PtyJob,
    ) -> Arc<TuiSession> {
        let pid = pty_job.job.pid();
        let emulator = Arc::new(Mutex::new(TerminalEmulator::new(cols, rows)));
        let waiter = FrameWaiter::new();
        let exited = Arc::new(AtomicBool::new(false));
        let session = Arc::new(TuiSession {
            pid,
            command,
            emulator: Arc::clone(&emulator),
            waiter: waiter.clone(),
            last_table: Mutex::new(ElementTable::default()),
            job: pty_job.job,
            exited: Arc::clone(&exited),
        });

        let output = pty_job.output;
        DefaultExecutor
            .spawn(async move {
                while let Ok(chunk) = output.recv().await {
                    let (generation, sync_active) = {
                        let mut guard = emulator
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.feed(&chunk);
                        (guard.generation(), guard.modes().sync_output_active)
                    };
                    waiter.record_feed(generation, sync_active);
                }
                exited.store(true, Ordering::Release);
                waiter.mark_closed();
            })
            .detach();

        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        prune_exited(&mut sessions);
        sessions.insert(pid, Arc::clone(&session));
        session
    }

    /// Looks up a registered TUI session by its real OS PID.
    #[must_use]
    pub fn get(&self, pid: u32) -> Option<Arc<TuiSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&pid)
            .cloned()
    }

    /// Returns summaries of all currently running (non-exited) TUI sessions,
    /// ordered by PID.
    #[must_use]
    pub fn open_sessions(&self) -> Vec<TuiSessionSummary> {
        let mut sessions: Vec<TuiSessionSummary> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|session| !session.is_exited())
            .map(|session| session.summary())
            .collect();
        sessions.sort_by_key(|summary| summary.pid);
        sessions
    }

    /// Kills every still-running TUI session in this registry and clears all entries.
    pub async fn close_all(&self) -> usize {
        let drained: Vec<Arc<TuiSession>> = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sessions.drain().map(|(_, session)| session).collect()
        };
        let mut killed = 0;
        for session in drained {
            if !session.is_exited() {
                if session.job.kill().await {
                    killed += 1;
                }
                session.exited.store(true, Ordering::Release);
                session.waiter.mark_closed();
            }
        }
        killed
    }

    /// Rasterizes `snapshot` into compact PNG bytes off the async event loop.
    ///
    /// # Errors
    /// Returns [`RasterizeError`] if PNG encoding fails.
    pub async fn rasterize(&self, snapshot: ScreenSnapshot) -> Result<Vec<u8>, RasterizeError> {
        let slot = Arc::clone(&self.rasterizer);
        blocking::unblock(move || {
            let rasterizer = slot.get_or_init(|| Arc::new(CellRasterizer::new()));
            rasterizer.render_png(&snapshot)
        })
        .await
    }
}

fn prune_exited(sessions: &mut HashMap<u32, Arc<TuiSession>>) {
    let exited_pids: Vec<u32> = sessions
        .iter()
        .filter_map(|(&pid, session)| session.is_exited().then_some(pid))
        .collect();
    if exited_pids.len() > MAX_RETAINED_EXITED {
        for pid in exited_pids {
            sessions.remove(&pid);
        }
    }
}

/// `tui <action> ...` — open and drive interactive terminal/TUI applications in a PTY.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TuiArgs {
    /// Action to perform.
    ///
    /// - `open [--cols N] [--rows N] [--timeout SECS] SCRIPT`: start a program in a PTY (default 120x40)
    /// - `observe PID`: capture a rasterized PNG plus structured panes, cursor, modes, and `<gen>:<idx>` refs
    /// - `text PID`: return clean trimmed plain text of the visible screen
    /// - `key PID KEY` or `key PID MOD+KEY`: send a W3C `KeyboardEvent.code` keystroke (`Enter`, `Tab`, `Escape`, `ArrowUp`, `ctrl+c`, etc.)
    /// - `type PID TEXT`: send literal text (wrapped in bracketed paste when `?2004` is active)
    /// - `click PID REF | TEXT | COL ROW`: click a `<gen>:<idx>` ref, unique visible substring, or 0-based cell coordinate (requires mouse tracking)
    /// - `double-click PID COL ROW` or `double-click PID REF|TEXT`
    /// - `right-click PID COL ROW` or `right-click PID REF|TEXT`
    /// - `drag PID COL1 ROW1 COL2 ROW2`
    /// - `scroll PID COL ROW DX DY`
    /// - `resize PID COLS ROWS`: resize the virtual terminal and forward `TIOCSWINSZ`/`SIGWINCH`
    pub action: String,
    /// Terminal width in columns for `open` (default 120).
    #[serde(default)]
    pub cols: Option<u16>,
    /// Terminal height in rows for `open` (default 40).
    #[serde(default)]
    pub rows: Option<u16>,
    /// Maximum lifetime in seconds for `open` (default 1800, capped by the shell timeout limit).
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Action operands (`PID`, `SCRIPT`, `KEY`, `TEXT`, `REF`, coordinates, etc.).
    #[serde(default)]
    #[schemars(extend("x-positional" = true))]
    pub operands: Vec<String>,
}

/// Output of a [`TuiTool`] call pairing natural-language text with optional
/// OCR-bypassing PNG attachments.
#[derive(Debug, Clone)]
pub struct TuiCommandOutput {
    result: ToolResult,
    images: Vec<RecordedCallImage>,
}

impl IntoToolResult for TuiCommandOutput {
    fn into_tool_result(self) -> ToolResult {
        self.result
    }
}

/// Errors raised by the `tui` command.
#[derive(Debug, thiserror::Error)]
pub enum TuiToolError {
    /// Runtime has no shell attached.
    #[error("tui needs a session shell, which this runtime has none")]
    NoShell,
    /// Invalid action or operands.
    #[error("{0}")]
    Usage(String),
    /// Unknown TUI PID.
    #[error("no TUI session with PID {pid}; open one first with `tui open <script>`")]
    UnknownPid {
        /// Requested PID.
        pid: u32,
    },
    /// Key or mouse encoding refusal (e.g. mouse tracking not enabled).
    #[error(transparent)]
    Input(#[from] TuiInputError),
    /// Refusal rendered from an Askama template (stale ref, missing/ambiguous text, etc.).
    #[error("{0}")]
    Refused(String),
    /// Underlying PTY spawn, input, resize, or rasterization error.
    #[error("{0}")]
    Runtime(String),
}

#[derive(Template)]
#[template(path = "tui_opened.txt", escape = "none")]
struct TuiOpenedText<'a> {
    pid: u32,
    cols: u16,
    rows: u16,
    observation: &'a str,
}

#[derive(Template)]
#[template(path = "tui_action_ok.txt", escape = "none")]
struct TuiActionOkText<'a> {
    summary: &'a str,
    pid: u32,
    generation: u64,
    exited: bool,
}

#[derive(Template)]
#[template(path = "tui_read_only_refused.txt", escape = "none")]
struct TuiReadOnlyRefused;

/// Renders the refusal returned when a read-only subagent or `btw` fork calls `tui`.
///
/// # Panics
/// Panics if the compiled-in `tui_read_only_refused.txt` template fails to render.
#[must_use]
pub fn read_only_tui_refusal() -> String {
    TuiReadOnlyRefused
        .render()
        .expect("tui_read_only_refused template renders")
        .trim_end_matches('\n')
        .to_owned()
}

/// The `tui` IPC command bound to one calling launch and [`TuiRegistry`].
#[derive(Clone)]
pub struct TuiTool {
    shell: Option<SessionShell>,
    launch: LaunchInfo,
    registry: TuiRegistry,
    timeout_cap: LiveDuration,
}

impl std::fmt::Debug for TuiTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiTool")
            .field("mode", &self.launch.mode)
            .finish_non_exhaustive()
    }
}

impl TuiTool {
    /// Creates a `tui` command bound to `shell`, `launch`, `registry`, and `timeout_cap`.
    #[must_use]
    pub const fn new(
        shell: Option<SessionShell>,
        launch: LaunchInfo,
        registry: TuiRegistry,
        timeout_cap: LiveDuration,
    ) -> Self {
        Self {
            shell,
            launch,
            registry,
            timeout_cap,
        }
    }

    fn require_shell(&self) -> Result<&SessionShell, TuiToolError> {
        self.shell.as_ref().ok_or(TuiToolError::NoShell)
    }

    fn require_session(&self, raw_pid: &str) -> Result<Arc<TuiSession>, TuiToolError> {
        let pid = raw_pid
            .parse::<u32>()
            .map_err(|_| TuiToolError::Usage(format!("expected numeric PID, got `{raw_pid}`")))?;
        self.registry
            .get(pid)
            .ok_or(TuiToolError::UnknownPid { pid })
    }

    async fn render_image_for_snapshot(
        &self,
        shell: &SessionShell,
        snapshot: ScreenSnapshot,
        source: &str,
    ) -> Result<RecordedCallImage, TuiToolError> {
        let png = self
            .registry
            .rasterize(snapshot)
            .await
            .map_err(|error| TuiToolError::Runtime(error.to_string()))?;
        let (_url, path) = shell
            .output_store()
            .save_binary(&png, "image/png")
            .await
            .map_err(|error| {
                TuiToolError::Runtime(format!("failed to store tui screen image: {error}"))
            })?;
        Ok(RecordedCallImage {
            path,
            media_type: "image/png".to_owned(),
            source: source.to_owned(),
            skip_ocr: true,
        })
    }

    async fn handle_open(
        &self,
        args: &TuiArgs,
        working_dir: Option<PathBuf>,
    ) -> Result<TuiCommandOutput, TuiToolError> {
        if args.operands.is_empty() {
            return Err(TuiToolError::Usage(
                "usage: tui open [--cols N] [--rows N] [--timeout SECS] SCRIPT".to_owned(),
            ));
        }
        let cols = args.cols.unwrap_or(DEFAULT_COLS);
        let rows = args.rows.unwrap_or(DEFAULT_ROWS);
        if cols == 0 || rows == 0 {
            return Err(TuiToolError::Usage(format!(
                "invalid terminal dimensions {cols}x{rows}: cols and rows must be at least 1"
            )));
        }
        let shell = self.require_shell()?;
        let script = if args.operands.len() == 1 {
            args.operands[0].clone()
        } else {
            args.operands
                .iter()
                .map(|word| shell_escape::escape(Cow::Borrowed(word.as_str())))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let description = format!("tui: {}", args.operands.join(" "));
        let cap_secs = self.timeout_cap.get().as_secs().max(1);
        let timeout = args
            .timeout
            .unwrap_or_else(|| DEFAULT_TUI_TIMEOUT_SECS.min(cap_secs));
        let pty_job = shell
            .run_pty(PtyArgs {
                description,
                script: script.clone(),
                interpreter: Interpreter::Shell,
                sandbox: self.launch.mode,
                cols,
                rows,
                timeout,
                working_dir,
                inherited_launch: Some(self.launch.clone()),
            })
            .await
            .map_err(|error| TuiToolError::Runtime(error.to_string()))?;

        let session = self.registry.register(script, cols, rows, pty_job);
        session.settle_after_action().await;
        let snapshot = session.snapshot();
        let obs = observe_screen(&snapshot);
        session.store_table(obs.table.clone());
        let obs_text = render_observation(&obs).expect("tui_observe template renders");
        let image = self
            .render_image_for_snapshot(shell, snapshot, "tui open")
            .await?;
        let text = TuiOpenedText {
            pid: session.pid(),
            cols,
            rows,
            observation: &obs_text,
        }
        .render()
        .expect("tui_opened template renders");
        Ok(TuiCommandOutput {
            result: ToolResult::text(text),
            images: vec![image],
        })
    }

    async fn handle_observe(&self, operands: &[String]) -> Result<TuiCommandOutput, TuiToolError> {
        let [raw_pid] = operands else {
            return Err(TuiToolError::Usage("usage: tui observe PID".to_owned()));
        };
        let shell = self.require_shell()?;
        let session = self.require_session(raw_pid)?;
        session.settle_current().await;
        let snapshot = session.snapshot();
        let obs = observe_screen(&snapshot);
        session.store_table(obs.table.clone());
        let text = render_observation(&obs).expect("tui_observe template renders");
        let image = self
            .render_image_for_snapshot(shell, snapshot, "tui observe")
            .await?;
        Ok(TuiCommandOutput {
            result: ToolResult::text(text),
            images: vec![image],
        })
    }

    async fn handle_text(&self, operands: &[String]) -> Result<TuiCommandOutput, TuiToolError> {
        let [raw_pid] = operands else {
            return Err(TuiToolError::Usage("usage: tui text PID".to_owned()));
        };
        let session = self.require_session(raw_pid)?;
        session.settle_current().await;
        let text = session.snapshot().plain_text();
        Ok(TuiCommandOutput {
            result: ToolResult::text(text),
            images: Vec::new(),
        })
    }

    async fn handle_key(&self, operands: &[String]) -> Result<TuiCommandOutput, TuiToolError> {
        let [raw_pid, spec] = operands else {
            return Err(TuiToolError::Usage(
                "usage: tui key PID KEY|MOD+KEY".to_owned(),
            ));
        };
        let session = self.require_session(raw_pid)?;
        let bytes = encode_key(spec, &session.modes())?;
        session
            .job
            .pty_input(bytes)
            .await
            .map_err(|error| TuiToolError::Runtime(error.to_string()))?;
        session.settle_after_action().await;
        Ok(action_ok(&format!("Sent key `{spec}`"), &session))
    }

    async fn handle_type(&self, operands: &[String]) -> Result<TuiCommandOutput, TuiToolError> {
        if operands.len() < 2 {
            return Err(TuiToolError::Usage("usage: tui type PID TEXT".to_owned()));
        }
        let session = self.require_session(&operands[0])?;
        let text = operands[1..].join(" ");
        let bytes = encode_type(&text, session.modes().bracketed_paste);
        session
            .job
            .pty_input(bytes)
            .await
            .map_err(|error| TuiToolError::Runtime(error.to_string()))?;
        session.settle_after_action().await;
        Ok(action_ok("Typed text", &session))
    }

    fn resolve_click_target(
        session: &TuiSession,
        target: &[String],
        usage: &str,
    ) -> Result<(u16, u16), TuiToolError> {
        match target {
            [col_str, row_str] => {
                let col = parse_u16(col_str, "COL")?;
                let row = parse_u16(row_str, "ROW")?;
                Ok((col, row))
            }
            [single] => single.parse::<TuiElementRef>().map_or_else(
                |_| {
                    let snapshot = session.snapshot();
                    find_unique_text(&snapshot, single)
                        .map_err(|error| TuiToolError::Refused(error.render_for_model()))
                },
                |elem_ref| session.resolve_ref(elem_ref).map_err(TuiToolError::Refused),
            ),
            _ => Err(TuiToolError::Usage(usage.to_owned())),
        }
    }

    async fn handle_click(
        &self,
        action: &str,
        operands: &[String],
    ) -> Result<TuiCommandOutput, TuiToolError> {
        let Some((raw_pid, rest)) = operands.split_first() else {
            return Err(TuiToolError::Usage(format!(
                "usage: tui {action} PID REF|TEXT | COL ROW"
            )));
        };
        let session = self.require_session(raw_pid)?;
        let (col, row) = Self::resolve_click_target(
            &session,
            rest,
            &format!("usage: tui {action} PID REF|TEXT | COL ROW"),
        )?;
        let modes = session.modes();
        let bytes = match action {
            "click" => encode_mouse_click(col, row, &modes)?,
            "double-click" => encode_mouse_double_click(col, row, &modes)?,
            "right-click" => encode_mouse_right_click(col, row, &modes)?,
            _ => unreachable!("checked click action"),
        };
        session
            .job
            .pty_input(bytes)
            .await
            .map_err(|error| TuiToolError::Runtime(error.to_string()))?;
        session.settle_after_action().await;
        Ok(action_ok(
            &format!("Sent {action} at ({col}, {row})"),
            &session,
        ))
    }

    async fn handle_drag(&self, operands: &[String]) -> Result<TuiCommandOutput, TuiToolError> {
        let [raw_pid, c1, r1, c2, r2] = operands else {
            return Err(TuiToolError::Usage(
                "usage: tui drag PID COL1 ROW1 COL2 ROW2".to_owned(),
            ));
        };
        let session = self.require_session(raw_pid)?;
        let from = (parse_u16(c1, "COL1")?, parse_u16(r1, "ROW1")?);
        let to = (parse_u16(c2, "COL2")?, parse_u16(r2, "ROW2")?);
        let bytes = encode_mouse_drag(from, to, &session.modes())?;
        session
            .job
            .pty_input(bytes)
            .await
            .map_err(|error| TuiToolError::Runtime(error.to_string()))?;
        session.settle_after_action().await;
        Ok(action_ok(
            &format!(
                "Dragged from ({}, {}) to ({}, {})",
                from.0, from.1, to.0, to.1
            ),
            &session,
        ))
    }

    async fn handle_scroll(&self, operands: &[String]) -> Result<TuiCommandOutput, TuiToolError> {
        let [raw_pid, col_s, row_s, horiz_s, vert_s] = operands else {
            return Err(TuiToolError::Usage(
                "usage: tui scroll PID COL ROW DX DY".to_owned(),
            ));
        };
        let session = self.require_session(raw_pid)?;
        let col = parse_u16(col_s, "COL")?;
        let row = parse_u16(row_s, "ROW")?;
        let dx = parse_i32(horiz_s, "DX")?;
        let dy = parse_i32(vert_s, "DY")?;
        let bytes = encode_mouse_scroll(col, row, dx, dy, &session.modes())?;
        session
            .job
            .pty_input(bytes)
            .await
            .map_err(|error| TuiToolError::Runtime(error.to_string()))?;
        session.settle_after_action().await;
        Ok(action_ok(
            &format!("Scrolled ({dx}, {dy}) at ({col}, {row})"),
            &session,
        ))
    }

    async fn handle_resize(&self, operands: &[String]) -> Result<TuiCommandOutput, TuiToolError> {
        let [raw_pid, cols_s, rows_s] = operands else {
            return Err(TuiToolError::Usage(
                "usage: tui resize PID COLS ROWS".to_owned(),
            ));
        };
        let session = self.require_session(raw_pid)?;
        let cols = parse_u16(cols_s, "COLS")?;
        let rows = parse_u16(rows_s, "ROWS")?;
        if cols == 0 || rows == 0 {
            return Err(TuiToolError::Usage(format!(
                "invalid terminal dimensions {cols}x{rows}: cols and rows must be at least 1"
            )));
        }
        session
            .job
            .resize(cols, rows)
            .await
            .map_err(|error| TuiToolError::Runtime(error.to_string()))?;
        {
            let mut guard = session
                .emulator
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.resize(cols, rows);
        }
        session.settle_after_action().await;
        Ok(action_ok(&format!("Resized to {cols}x{rows}"), &session))
    }
}

fn action_ok(summary: &str, session: &TuiSession) -> TuiCommandOutput {
    let text = TuiActionOkText {
        summary,
        pid: session.pid(),
        generation: session.generation(),
        exited: session.is_exited(),
    }
    .render()
    .expect("tui_action_ok template renders");
    TuiCommandOutput {
        result: ToolResult::text(text),
        images: Vec::new(),
    }
}

fn parse_u16(raw: &str, label: &str) -> Result<u16, TuiToolError> {
    raw.parse::<u16>().map_err(|_| {
        TuiToolError::Usage(format!(
            "expected non-negative integer for {label}, got `{raw}`"
        ))
    })
}

fn parse_i32(raw: &str, label: &str) -> Result<i32, TuiToolError> {
    raw.parse::<i32>()
        .map_err(|_| TuiToolError::Usage(format!("expected integer for {label}, got `{raw}`")))
}

impl Tool for TuiTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("tui")
    }

    type Arguments = TuiArgs;
    type Res = TuiCommandOutput;
    type Error = TuiToolError;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> Result<Self::Res, Self::Error> {
        let working_dir = cx.working_dir().map(PathBuf::from);
        drop(cx);
        match args.action.as_str() {
            "open" => self.handle_open(&args, working_dir).await,
            "observe" => self.handle_observe(&args.operands).await,
            "text" => self.handle_text(&args.operands).await,
            "key" => self.handle_key(&args.operands).await,
            "type" => self.handle_type(&args.operands).await,
            action @ ("click" | "double-click" | "right-click") => {
                self.handle_click(action, &args.operands).await
            }
            "drag" => self.handle_drag(&args.operands).await,
            "scroll" => self.handle_scroll(&args.operands).await,
            "resize" => self.handle_resize(&args.operands).await,
            other => Err(TuiToolError::Usage(format!(
                "unknown tui action `{other}`; expected one of: open, observe, text, key, type, click, double-click, right-click, drag, scroll, resize"
            ))),
        }
    }
}

impl IpcImageTool for TuiTool {
    fn images(result: &Self::Res) -> &[RecordedCallImage] {
        &result.images
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aither_core::llm::{AgentIdentity, CallIdentity, SubagentIdentity, ToolCallId};
    use aither_sandbox::SandboxMode;

    use super::*;
    use crate::ipc::{CallImageStore, CommandRegistryBuilder, ShellSlot};
    use crate::test_sandbox::TestTerminal;
    use crate::vision_ocr::{VisionOcr, augment_tool_outcome};

    fn main_launch(mode: SandboxMode) -> LaunchInfo {
        LaunchInfo::new(
            aither_sandbox::LaunchLease::fresh().id(),
            Some(CallIdentity::new(
                ToolCallId::new("call-tui-test"),
                AgentIdentity::Main,
            )),
            mode,
        )
    }

    fn subagent_launch(mode: SandboxMode) -> LaunchInfo {
        LaunchInfo::new(
            aither_sandbox::LaunchLease::fresh().id(),
            Some(CallIdentity::new(
                ToolCallId::new("call-sub-tui-test"),
                AgentIdentity::Subagent(SubagentIdentity::new("worker", None)),
            )),
            mode,
        )
    }

    fn args(action: &str, operands: &[&str]) -> TuiArgs {
        TuiArgs {
            action: action.to_owned(),
            cols: None,
            rows: None,
            timeout: Some(60),
            operands: operands.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    async fn wait_until_screen_contains(session: &TuiSession, needle: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if session.snapshot().plain_text().contains(needle) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {needle:?}; screen was:\n{}",
                session.snapshot().plain_text()
            );
            async_io::Timer::after(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn tui_open_observe_text_key_type_resize_skip_ocr_and_kill_roundtrip() {
        let root = tempfile::tempdir().expect("tempdir");
        let terminal = TestTerminal::new(root.path()).await;
        let registry = TuiRegistry::new();
        let tool = TuiTool::new(
            Some(terminal.shell().clone()),
            main_launch(SandboxMode::Sandboxed),
            registry.clone(),
            LiveDuration::new(Duration::from_secs(600)),
        );

        let script = r#"stty -echo
printf "┌──────────────┐\n│ 中文 日本語 한국어 │\n└──────────────┘\n"
printf "INIT:%s\n" "$(stty size)"
while IFS= read -r line; do
  if [ "$line" = "CHECK_SIZE" ]; then
    printf "RESIZED:%s\n" "$(stty size)"
  else
    printf "TYPED:%s\n" "$line"
  fi
done"#;

        let opened = tool
            .call(
                TuiArgs {
                    action: "open".to_owned(),
                    cols: Some(80),
                    rows: Some(24),
                    timeout: Some(60),
                    operands: vec![script.to_owned()],
                },
                ToolContext::new(),
            )
            .await
            .expect("tui open succeeds");

        assert_eq!(opened.images.len(), 1, "open attaches a rasterized PNG");
        assert!(
            opened.images[0].skip_ocr,
            "tui rasterized images must bypass VisionOcr"
        );
        assert!(
            opened.images[0].path.exists(),
            "rasterized PNG was persisted"
        );

        // Verify VisionOcr augmentation attaches the PNG without `<recognized_text>`.
        let store = CallImageStore::default();
        let ocr_call_id = ToolCallId::new("call-ocr-check");
        store.record(&ocr_call_id, opened.images.clone());
        let (augmented, attachments) = augment_tool_outcome(
            &VisionOcr::new(),
            Some(&store),
            Some(terminal.shell().output_store().dir()),
            &ocr_call_id,
            "terminal",
            opened.result.clone(),
        )
        .await
        .expect("image attachment is produced");
        assert_eq!(attachments.len(), 1);
        assert!(
            !augmented
                .as_text()
                .unwrap_or_default()
                .contains("<recognized_text>"),
            "skip_ocr must suppress VisionOcr text block: {augmented:?}"
        );

        let open_sessions = registry.open_sessions();
        assert_eq!(open_sessions.len(), 1);
        let pid = open_sessions[0].pid;
        let pid_str = pid.to_string();
        let session = registry.get(pid).expect("registered session");
        wait_until_screen_contains(&session, "INIT:24 80").await;

        let text_out = tool
            .call(args("text", &[&pid_str]), ToolContext::new())
            .await
            .expect("tui text succeeds");
        let plain = text_out.result.as_text().unwrap_or_default();
        assert!(plain.contains("中文 日本語 한국어"), "plain text: {plain}");

        // Mouse click must be refused because the program never enabled mouse tracking.
        let click_err = tool
            .call(args("click", &[&pid_str, "1", "1"]), ToolContext::new())
            .await
            .expect_err("click without mouse tracking must be refused");
        assert!(
            click_err.to_string().contains("mouse tracking"),
            "unexpected click refusal: {click_err}"
        );

        // Type literal text and press Enter.
        tool.call(args("type", &[&pid_str, "hello-pty"]), ToolContext::new())
            .await
            .expect("tui type succeeds");
        tool.call(args("key", &[&pid_str, "Enter"]), ToolContext::new())
            .await
            .expect("tui key Enter succeeds");
        wait_until_screen_contains(&session, "TYPED:hello-pty").await;

        // Resize updates PTY window dimensions (`TIOCSWINSZ`).
        tool.call(args("resize", &[&pid_str, "100", "32"]), ToolContext::new())
            .await
            .expect("tui resize succeeds");
        tool.call(args("type", &[&pid_str, "CHECK_SIZE"]), ToolContext::new())
            .await
            .expect("tui type CHECK_SIZE succeeds");
        tool.call(args("key", &[&pid_str, "Enter"]), ToolContext::new())
            .await
            .expect("tui key Enter succeeds");
        wait_until_screen_contains(&session, "RESIZED:32 100").await;

        let observed = tool
            .call(args("observe", &[&pid_str]), ToolContext::new())
            .await
            .expect("tui observe succeeds");
        assert_eq!(observed.images.len(), 1);
        assert!(observed.images[0].skip_ocr);
        let obs_text = observed.result.as_text().unwrap_or_default();
        assert!(
            obs_text.contains("100x32"),
            "observed dimensions: {obs_text}"
        );

        // Native signal kill terminates the PTY process and marks the session exited.
        assert!(
            terminal.shell().jobs().kill(pid).await,
            "kill terminates PTY job"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !session.is_exited() {
            assert!(
                std::time::Instant::now() < deadline,
                "session did not observe EOF after kill"
            );
            async_io::Timer::after(Duration::from_millis(25)).await;
        }
        assert_eq!(registry.open_sessions(), Vec::new());
    }

    #[tokio::test]
    async fn tui_click_supports_refs_and_unique_text_and_rejects_stale_refs() {
        let root = tempfile::tempdir().expect("tempdir");
        let terminal = TestTerminal::new(root.path()).await;
        let registry = TuiRegistry::new();
        let tool = TuiTool::new(
            Some(terminal.shell().clone()),
            main_launch(SandboxMode::Sandboxed),
            registry.clone(),
            LiveDuration::new(Duration::from_secs(600)),
        );

        let script = r"stty -echo -icanon min 1 time 0
printf '\033[?1000h\033[?1006h\033[2J\033[H\033[7m[Apply]\033[0m\nREADY\n'
dd bs=1 count=32 2>/dev/null | od -An -tx1 | tr -d ' \n' | sed 's/^/MOUSE:/'
printf '\nDONE\n'
while :; do sleep 0.2; done";

        let opened = tool
            .call(
                TuiArgs {
                    action: "open".to_owned(),
                    cols: Some(60),
                    rows: Some(15),
                    timeout: Some(60),
                    operands: vec![script.to_owned()],
                },
                ToolContext::new(),
            )
            .await
            .expect("tui open succeeds");
        let pid = registry.open_sessions()[0].pid;
        let pid_str = pid.to_string();
        let session = registry.get(pid).expect("registered session");
        wait_until_screen_contains(&session, "READY").await;

        let observed = tool
            .call(args("observe", &[&pid_str]), ToolContext::new())
            .await
            .expect("observe succeeds");
        let obs_text = observed.result.as_text().unwrap_or_default();
        assert!(
            obs_text.contains("[Apply]"),
            "observation lists reversed button: {obs_text}"
        );
        // Extract the `<gen>:<idx>` ref printed next to `[Apply]`.
        let apply_ref = obs_text
            .lines()
            .find(|line| line.contains("[Apply]"))
            .and_then(|line| {
                line.split_whitespace()
                    .find(|tok| tok.parse::<TuiElementRef>().is_ok())
            })
            .expect("observation contains a `<gen>:<idx>` ref for [Apply]")
            .to_owned();

        // Click by unique visible substring first — sends SGR press + release.
        tool.call(args("click", &[&pid_str, "Apply"]), ToolContext::new())
            .await
            .expect("unique text click succeeds");
        // Advance screen generation with a second click using the ref (now stale after DONE prints)
        // or verify the mouse bytes arrived and generation advanced.
        tool.call(args("click", &[&pid_str, "Apply"]), ToolContext::new())
            .await
            .expect("second click fills dd read");
        wait_until_screen_contains(&session, "MOUSE:").await;

        // Now screen generation has advanced past `apply_ref`, so reusing it must fail as stale!
        let stale_err = tool
            .call(args("click", &[&pid_str, &apply_ref]), ToolContext::new())
            .await
            .expect_err("stale element ref must be refused");
        assert!(
            stale_err.to_string().contains("stale") || stale_err.to_string().contains("generation"),
            "unexpected stale ref error: {stale_err}"
        );
        let _ = opened;
        let _ = registry.close_all().await;
    }

    #[tokio::test]
    async fn tui_open_inherits_unsandboxed_launch_without_reprompting_approver() {
        let root = tempfile::tempdir().expect("tempdir");
        // `TestTerminal` uses `SandboxedOnly`, which denies every direct `Approver` prompt.
        let terminal = TestTerminal::new(root.path()).await;
        let registry = TuiRegistry::new();
        let tool = TuiTool::new(
            Some(terminal.shell().clone()),
            main_launch(SandboxMode::Unsandboxed),
            registry.clone(),
            LiveDuration::new(Duration::from_secs(600)),
        );

        let opened = tool
            .call(
                TuiArgs {
                    action: "open".to_owned(),
                    cols: Some(80),
                    rows: Some(24),
                    timeout: Some(60),
                    operands: vec!["printf 'UNSANDBOXED-OK\\n'; sleep 30".to_owned()],
                },
                ToolContext::new(),
            )
            .await
            .expect("inherited unsandboxed launch skips duplicate Approver prompt");
        assert_eq!(opened.images.len(), 1);
        let pid = registry.open_sessions()[0].pid;
        let session = registry.get(pid).expect("session");
        wait_until_screen_contains(&session, "UNSANDBOXED-OK").await;
        let _ = registry.close_all().await;
    }

    #[tokio::test]
    async fn read_only_subagent_refuses_tui_and_writable_subagent_is_isolated() {
        let root = tempfile::tempdir().expect("tempdir");
        let main_tui = TuiRegistry::new();
        let main_tui_clone = main_tui.clone();
        let cap = LiveDuration::new(Duration::from_secs(600));
        let mut builder = CommandRegistryBuilder::new();
        builder.configure_bound_image_tool("tui", move |binding| {
            TuiTool::new(
                binding.shell.cloned(),
                binding.launch.clone(),
                main_tui_clone.clone(),
                cap.clone(),
            )
        });
        let base_registry = builder.build();
        let terminal = TestTerminal::new(root.path()).await;

        // 1. Read-only subagent registry refuses `tui`.
        let ro_registry = base_registry.refuse_command(
            "tui",
            "Unavailable in read-only subagents",
            read_only_tui_refusal(),
        );
        let ro_outcome = ro_registry
            .call(
                &subagent_launch(SandboxMode::Sandboxed),
                Some(terminal.shell()),
                terminal.shell().output_store(),
                "tui",
                &["open".to_owned(), "sleep 30".to_owned()],
                Some(root.path()),
            )
            .await;
        assert!(
            !ro_outcome.ok,
            "read-only subagent must be refused: {ro_outcome:?}"
        );
        assert!(
            ro_outcome
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("read-only"),
            "refusal should explain read-only policy: {ro_outcome:?}"
        );

        // 2. Writable subagent gets its own `TuiRegistry` and `SessionShell` that tears down cleanly.
        let sub_tui = TuiRegistry::new();
        let sub_tui_clone = sub_tui.clone();
        let sub_cap = LiveDuration::new(Duration::from_secs(600));
        let sub_registry = base_registry.replace_bound_image_tool("tui", move |binding| {
            TuiTool::new(
                binding.shell.cloned(),
                binding.launch.clone(),
                sub_tui_clone.clone(),
                sub_cap.clone(),
            )
        });
        let sub_slot = ShellSlot::new();
        let sub_shell = terminal
            .shell()
            .subagent(aither_sandbox::SubagentGrant::new(), None, "writable")
            .await
            .expect("subagent shell")
            .with_ipc(crate::ipc::CommandBridge::new(
                sub_registry.clone(),
                sub_slot.clone(),
            ));
        let sub_shell = Arc::new(sub_shell);
        sub_slot.bind(&sub_shell);

        let sub_outcome = sub_registry
            .call(
                &subagent_launch(SandboxMode::Sandboxed),
                Some(&sub_shell),
                sub_shell.output_store(),
                "tui",
                &[
                    "open".to_owned(),
                    "printf 'SUB-TUI\\n'; sleep 30".to_owned(),
                ],
                Some(sub_shell.working_dir()),
            )
            .await;
        assert!(
            sub_outcome.ok,
            "writable subagent tui open: {sub_outcome:?}"
        );
        assert_eq!(
            main_tui.open_sessions(),
            Vec::new(),
            "writable subagent TUI must never enter main session registry"
        );
        assert_eq!(sub_tui.open_sessions().len(), 1);
        let killed = sub_tui.close_all().await;
        assert_eq!(killed, 1);
        assert_eq!(sub_tui.open_sessions(), Vec::new());
    }
}
