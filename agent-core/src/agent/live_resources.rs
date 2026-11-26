//! Live session resource recall across context compaction/reassembly and
//! turn-end unattended-resource checks.
//!
//! [`LiveResourcesContext`] implements [`aither_agent::ContextSource`] for the
//! main session:
//! - On [`Moment::Request`] with `reassembled == true`, it queries live
//!   background jobs, open `tui` sessions, any session-owned running browser,
//!   and optional [`LiveResourceProvider`] extensions, injecting a single
//!   [`Placement::Stable`](aither_agent::Placement::Stable) inventory block
//!   (and emitting nothing when empty or when `reassembled == false`, so warm
//!   prompt caches remain untouched).
//! - On [`Moment::FinalTurn`], it raises an at-most-once-per-resource
//!   objection when an interactive `tui` session (or an extended provider's
//!   unattended resource) remains open.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aither_agent::{ContextBlock, ContextSource, Moment, SourceId};
use aither_sandbox::{JobStatus, Jobs};
use askama::Template;

use super::tui_tool::TuiRegistry;

/// Maximum characters of a background job script shown in the live inventory.
const MAX_SCRIPT_CHARS: usize = 80;

/// Snapshot of a session-owned managed browser reported to context sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveBrowserStatus {
    /// Whether the browser is currently running and owned by this session.
    pub running: bool,
    /// CDP WebSocket endpoint, when available.
    pub cdp_endpoint: Option<String>,
    /// Number of connected WebRTC live-view peers.
    pub active_peers: usize,
    /// Whether user takeover is currently active.
    pub user_takeover: bool,
}

/// Reads live state of the session's owned managed browser.
pub trait BrowserLiveReader: Send + Sync + 'static {
    /// Returns the browser's status when this session currently owns a running browser.
    fn status(&self) -> Option<LiveBrowserStatus>;
}

/// Type-erased [`BrowserLiveReader`] handle.
#[derive(Clone)]
pub struct AnyBrowserLiveReader(Arc<dyn BrowserLiveReader>);

impl std::fmt::Debug for AnyBrowserLiveReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnyBrowserLiveReader")
            .finish_non_exhaustive()
    }
}

impl AnyBrowserLiveReader {
    /// Wraps a concrete [`BrowserLiveReader`].
    #[must_use]
    pub fn new(reader: impl BrowserLiveReader) -> Self {
        Self(Arc::new(reader))
    }

    /// Reads the current browser status for the owning session.
    #[must_use]
    pub fn status(&self) -> Option<LiveBrowserStatus> {
        self.0.status()
    }
}

/// One titled section of live resources rendered in `<live-resources>` on reassembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveResourceSection {
    /// Section heading (for example `"Interactive TUI sessions"` or `"Active debug sessions"`).
    pub heading: String,
    /// Concise natural-language bullet entries for each active resource.
    pub items: Vec<String>,
}

/// One unattended interactive resource eligible for an at-most-once `FinalTurn` objection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnattendedResource {
    /// Stable deduplication key (for example `"tui:12345"` or `"debug:1"`).
    pub key: String,
    /// Natural-language summary of the open resource.
    pub description: String,
    /// Actionable cleanup instruction appended after the summary.
    pub cleanup_hint: String,
}

/// Extension point for additional session-scoped live resources (such as active
/// debugger sessions) that participate in post-compaction recall and turn-end checks.
pub trait LiveResourceProvider: Send + Sync + 'static {
    /// Returns sections to include in the `<live-resources>` inventory on reassembly.
    fn snapshot(&self) -> impl Future<Output = Vec<LiveResourceSection>> + Send;

    /// Returns currently open resources that should trigger an at-most-once
    /// `Moment::FinalTurn` cleanup prompt.
    fn unattended_final_turn(&self) -> impl Future<Output = Vec<UnattendedResource>> + Send {
        async { Vec::new() }
    }
}

trait LiveResourceProviderErased: Send + Sync {
    fn snapshot<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Vec<LiveResourceSection>> + Send + 'a>>;

    fn unattended_final_turn<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Vec<UnattendedResource>> + Send + 'a>>;
}

impl<T: LiveResourceProvider> LiveResourceProviderErased for T {
    fn snapshot<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Vec<LiveResourceSection>> + Send + 'a>> {
        Box::pin(LiveResourceProvider::snapshot(self))
    }

    fn unattended_final_turn<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Vec<UnattendedResource>> + Send + 'a>> {
        Box::pin(LiveResourceProvider::unattended_final_turn(self))
    }
}

/// Type-erased [`LiveResourceProvider`] handle.
#[derive(Clone)]
pub struct AnyLiveResourceProvider(Arc<dyn LiveResourceProviderErased>);

impl std::fmt::Debug for AnyLiveResourceProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnyLiveResourceProvider")
            .finish_non_exhaustive()
    }
}

impl AnyLiveResourceProvider {
    /// Wraps a concrete [`LiveResourceProvider`].
    #[must_use]
    pub fn new(provider: impl LiveResourceProvider) -> Self {
        Self(Arc::new(provider))
    }

    async fn snapshot(&self) -> Vec<LiveResourceSection> {
        self.0.snapshot().await
    }

    async fn unattended_final_turn(&self) -> Vec<UnattendedResource> {
        self.0.unattended_final_turn().await
    }
}

#[derive(Template)]
#[template(path = "live_resources.txt", escape = "none")]
struct LiveResourcesTemplate<'a> {
    sections: &'a [LiveResourceSection],
}

#[derive(Template)]
#[template(path = "live_resources_final_turn.txt", escape = "none")]
struct LiveResourcesFinalTurnTemplate<'a> {
    items: &'a [UnattendedResource],
}

/// [`ContextSource`] for live session resource recall and turn-end checks.
///
/// Reports active background jobs, open `tui` sessions, owned running browsers,
/// and registered [`LiveResourceProvider`] entries on context reassembly, and
/// raises at-most-once-per-resource `FinalTurn` objections for unattended items.
pub struct LiveResourcesContext {
    jobs: Jobs,
    tui_registry: TuiRegistry,
    browser: Option<AnyBrowserLiveReader>,
    providers: Vec<AnyLiveResourceProvider>,
    objected_final_turn: HashSet<String>,
}

impl std::fmt::Debug for LiveResourcesContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveResourcesContext")
            .field("providers", &self.providers.len())
            .field("objected_final_turn", &self.objected_final_turn.len())
            .finish_non_exhaustive()
    }
}

impl LiveResourcesContext {
    /// Builds a new live-resource context source for a session.
    #[must_use]
    pub fn new(
        jobs: Jobs,
        tui_registry: TuiRegistry,
        browser: Option<AnyBrowserLiveReader>,
        providers: Vec<AnyLiveResourceProvider>,
    ) -> Self {
        Self {
            jobs,
            tui_registry,
            browser,
            providers,
            objected_final_turn: HashSet::new(),
        }
    }

    async fn collect_sections(&self) -> Vec<LiveResourceSection> {
        let mut sections = Vec::new();

        let open_tuis = self.tui_registry.open_sessions();
        let tui_pids: HashSet<u32> = open_tuis.iter().map(|summary| summary.pid).collect();

        let mut running_jobs: Vec<_> = self
            .jobs
            .list()
            .await
            .into_iter()
            .filter(|job| job.status == JobStatus::Running && !tui_pids.contains(&job.pid))
            .collect();
        running_jobs.sort_by_key(|job| job.pid);
        if !running_jobs.is_empty() {
            let items = running_jobs
                .into_iter()
                .map(|job| {
                    let preview = compact_preview(&job.script, MAX_SCRIPT_CHARS);
                    let elapsed = format_elapsed(job.started_at.elapsed());
                    format!(
                        "PID {} (running {elapsed}): `{preview}` — inspect with `terminal_read`, write with `terminal_input`, or stop with `kill {}`",
                        job.pid, job.pid
                    )
                })
                .collect();
            sections.push(LiveResourceSection {
                heading: "Running background jobs".to_string(),
                items,
            });
        }

        if !open_tuis.is_empty() {
            let items = open_tuis
                .into_iter()
                .map(|tui| {
                    let preview = compact_preview(&tui.command, MAX_SCRIPT_CHARS);
                    let title = tui
                        .title
                        .as_deref()
                        .map_or_else(String::new, |t| format!(", title \"{t}\""));
                    format!(
                        "PID {} ({}x{}{title}): `{preview}` — inspect with `tui observe {}` / `tui text {}`, or close with quit key / `kill {}`",
                        tui.pid, tui.cols, tui.rows, tui.pid, tui.pid, tui.pid
                    )
                })
                .collect();
            sections.push(LiveResourceSection {
                heading: "Interactive TUI sessions".to_string(),
                items,
            });
        }

        if let Some(status) = self.browser.as_ref().and_then(AnyBrowserLiveReader::status)
            && status.running
        {
            let mut details = Vec::new();
            if let Some(cdp) = status.cdp_endpoint {
                details.push(format!("CDP {cdp}"));
            }
            if status.active_peers > 0 {
                details.push(format!("{} live viewer(s)", status.active_peers));
            }
            if status.user_takeover {
                details.push("user takeover active".to_string());
            }
            let suffix = if details.is_empty() {
                String::new()
            } else {
                format!(" ({})", details.join(", "))
            };
            sections.push(LiveResourceSection {
                heading: "Managed browser".to_string(),
                items: vec![format!(
                    "Running and owned by this session{suffix} — inspect with `browser status` / `browser read` or close with `browser stop`"
                )],
            });
        }

        if !self.providers.is_empty() {
            let extra = futures_util::future::join_all(
                self.providers.iter().map(AnyLiveResourceProvider::snapshot),
            )
            .await;
            for provider_sections in extra {
                for section in provider_sections {
                    if !section.items.is_empty() {
                        sections.push(section);
                    }
                }
            }
        }

        sections
    }

    async fn collect_unattended(&self) -> Vec<UnattendedResource> {
        let mut items: Vec<UnattendedResource> = self
            .tui_registry
            .open_sessions()
            .into_iter()
            .map(|tui| {
                let preview = compact_preview(&tui.command, MAX_SCRIPT_CHARS);
                let title = tui
                    .title
                    .as_deref()
                    .map_or_else(String::new, |t| format!(", title \"{t}\""));
                UnattendedResource {
                    key: format!("tui:{}", tui.pid),
                    description: format!(
                        "Interactive TUI PID {} ({}x{}{title}): `{preview}`.",
                        tui.pid, tui.cols, tui.rows
                    ),
                    cleanup_hint: format!(
                        "Close it via its quit key or `kill {}` if no longer needed.",
                        tui.pid
                    ),
                }
            })
            .collect();

        if !self.providers.is_empty() {
            let extra = futures_util::future::join_all(
                self.providers
                    .iter()
                    .map(AnyLiveResourceProvider::unattended_final_turn),
            )
            .await;
            for provider_items in extra {
                items.extend(provider_items);
            }
        }

        items
    }

    async fn render_inventory_block(&self) -> Vec<ContextBlock> {
        let sections = self.collect_sections().await;
        if sections.is_empty() {
            return Vec::new();
        }
        let rendered = LiveResourcesTemplate {
            sections: &sections,
        }
        .render()
        .expect("live resources template renders typed input");
        vec![ContextBlock::stable("live-resources", rendered)]
    }
}

impl ContextSource for LiveResourcesContext {
    fn id(&self) -> SourceId {
        SourceId::new("live-resources")
    }

    async fn blocks(&mut self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { reassembled: true } => self.render_inventory_block().await,
            Moment::FinalTurn => {
                let unattended = self.collect_unattended().await;
                let active_keys: HashSet<String> =
                    unattended.iter().map(|item| item.key.clone()).collect();
                self.objected_final_turn
                    .retain(|key| active_keys.contains(key));

                let fresh: Vec<UnattendedResource> = unattended
                    .into_iter()
                    .filter(|item| !self.objected_final_turn.contains(&item.key))
                    .collect();
                if fresh.is_empty() {
                    return Vec::new();
                }
                for item in &fresh {
                    self.objected_final_turn.insert(item.key.clone());
                }
                let rendered = LiveResourcesFinalTurnTemplate { items: &fresh }
                    .render()
                    .expect("live resources final turn template renders typed input");
                vec![ContextBlock::stable("system-reminder", rendered)]
            }
            _ => Vec::new(),
        }
    }

    async fn peek(&self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { reassembled: true } => self.render_inventory_block().await,
            _ => Vec::new(),
        }
    }
}

fn compact_preview(script: &str, max_chars: usize) -> String {
    let single_line: String = script.split_whitespace().collect::<Vec<_>>().join(" ");
    match single_line.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}...", single_line.split_at(end).0),
        None => single_line,
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 3_600 {
        format!("{}h{}m", secs / 3_600, (secs % 3_600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use aither_agent::{ContextSource, Moment, Placement};
    use aither_sandbox::{Interpreter, PtyArgs};

    use super::*;
    use crate::test_sandbox::TestTerminal;

    struct TestBrowserReader {
        running: Arc<AtomicBool>,
        cdp: Option<String>,
    }

    impl BrowserLiveReader for TestBrowserReader {
        fn status(&self) -> Option<LiveBrowserStatus> {
            self.running
                .load(Ordering::Acquire)
                .then(|| LiveBrowserStatus {
                    running: true,
                    cdp_endpoint: self.cdp.clone(),
                    active_peers: 1,
                    user_takeover: false,
                })
        }
    }

    struct TestExtraProvider {
        active: Arc<AtomicBool>,
    }

    impl LiveResourceProvider for TestExtraProvider {
        fn snapshot(&self) -> impl Future<Output = Vec<LiveResourceSection>> + Send {
            let sections = if self.active.load(Ordering::Acquire) {
                vec![LiveResourceSection {
                    heading: "Active debug sessions".to_string(),
                    items: vec!["Session #1 (lldb-dap) stopped at main.rs:42".to_string()],
                }]
            } else {
                Vec::new()
            };
            std::future::ready(sections)
        }

        fn unattended_final_turn(&self) -> impl Future<Output = Vec<UnattendedResource>> + Send {
            let items = if self.active.load(Ordering::Acquire) {
                vec![UnattendedResource {
                    key: "debug:1".to_string(),
                    description: "Debug session #1 (lldb-dap) is still attached.".to_string(),
                    cleanup_hint: "Run `debug terminate` or `debug detach`.".to_string(),
                }]
            } else {
                Vec::new()
            };
            std::future::ready(items)
        }
    }

    #[tokio::test]
    async fn emits_nothing_when_empty_or_not_reassembled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let terminal = TestTerminal::new(temp.path()).await;
        let browser_running = Arc::new(AtomicBool::new(false));
        let mut source = LiveResourcesContext::new(
            terminal.shell().jobs(),
            TuiRegistry::new(),
            Some(AnyBrowserLiveReader::new(TestBrowserReader {
                running: Arc::clone(&browser_running),
                cdp: Some("ws://127.0.0.1:39222/devtools/browser/test".to_string()),
            })),
            Vec::new(),
        );

        assert!(
            source
                .blocks(Moment::Request { reassembled: true })
                .await
                .is_empty(),
            "empty session should emit no live-resources block"
        );

        browser_running.store(true, Ordering::Release);
        assert!(
            source
                .blocks(Moment::Request { reassembled: false })
                .await
                .is_empty(),
            "non-reassembled request must leave warm prompt cache untouched"
        );

        let blocks = source.blocks(Moment::Request { reassembled: true }).await;
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].placement(), Placement::Stable);
        assert_eq!(blocks[0].block().tag(), "live-resources");
        assert!(blocks[0].block().content().contains("Managed browser"));
        assert!(blocks[0].block().content().contains("ws://127.0.0.1:39222"));
    }

    #[tokio::test]
    async fn reassembly_reports_jobs_tuis_browser_and_extra_providers_and_final_turn_fires_once_per_pid()
     {
        let temp = tempfile::tempdir().expect("tempdir");
        let terminal = TestTerminal::new(temp.path()).await;
        let tui_registry = TuiRegistry::new();

        let pty_job = terminal
            .shell()
            .run_pty(PtyArgs {
                description: "test tui".to_string(),
                script: "sleep 30".to_string(),
                interpreter: Interpreter::Shell,
                sandbox: aither_sandbox::SandboxMode::Sandboxed,
                cols: 80,
                rows: 24,
                timeout: 60,
                working_dir: None,
                inherited_launch: None,
            })
            .await
            .expect("spawn pty job");
        let tui_pid = pty_job.job.pid();
        let _tui_session = tui_registry.register("sleep 30".to_string(), 80, 24, pty_job);

        let (_bg_job, bg_pid) = terminal.sleeping_job().await;

        let browser_running = Arc::new(AtomicBool::new(true));
        let extra_active = Arc::new(AtomicBool::new(true));
        let mut source = LiveResourcesContext::new(
            terminal.shell().jobs(),
            tui_registry.clone(),
            Some(AnyBrowserLiveReader::new(TestBrowserReader {
                running: Arc::clone(&browser_running),
                cdp: Some("ws://127.0.0.1:39222/devtools/browser/live".to_string()),
            })),
            vec![AnyLiveResourceProvider::new(TestExtraProvider {
                active: Arc::clone(&extra_active),
            })],
        );

        let blocks = source.blocks(Moment::Request { reassembled: true }).await;
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].placement(), Placement::Stable);
        let body = blocks[0].block().content();
        assert!(body.contains("Running background jobs"), "body: {body}");
        assert!(body.contains(&format!("PID {bg_pid}")), "body: {body}");
        assert!(body.contains("Interactive TUI sessions"), "body: {body}");
        assert!(body.contains(&format!("PID {tui_pid}")), "body: {body}");
        assert!(body.contains("Managed browser"), "body: {body}");
        assert!(body.contains("Active debug sessions"), "body: {body}");
        assert_eq!(
            body.matches(&format!("PID {tui_pid}")).count(),
            1,
            "TUI PID should not be duplicated under plain background jobs: {body}"
        );

        // FinalTurn objects once per open TUI PID + extra unattended resource.
        let first_final = source.blocks(Moment::FinalTurn).await;
        assert_eq!(first_final.len(), 1);
        assert_eq!(first_final[0].placement(), Placement::Stable);
        assert!(
            first_final[0]
                .block()
                .content()
                .contains(&format!("PID {tui_pid}"))
        );
        assert!(
            first_final[0]
                .block()
                .content()
                .contains("Debug session #1")
        );

        // Second FinalTurn with the same open resources emits nothing (at-most-once).
        let second_final = source.blocks(Moment::FinalTurn).await;
        assert!(
            second_final.is_empty(),
            "FinalTurn must not repeat objection for the same open PID"
        );

        // Cleanup spawned test processes.
        tui_registry.close_all().await;
        let _ = terminal.shell().jobs().kill_all().await;
    }
}
