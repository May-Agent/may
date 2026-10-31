//! Jank instrumentation for the TUI event loop.
//!
//! Rendering and event handling timings are collected per frame/iteration
//! and reported through `tracing` on the `jank` target whenever one busy
//! iteration exceeds the threshold (`MAY_JANK_MS`, default
//! [`DEFAULT_JANK_THRESHOLD_MS`]). The log line carries enough render state
//! (document size, scroll position, per-phase timings, event backlog) to
//! reproduce a stall from `~/.may/logs/may.log` alone.

use std::time::Duration;

/// Default busy-time threshold for a jank report, in milliseconds.
pub const DEFAULT_JANK_THRESHOLD_MS: u64 = 20;

/// Render-time breakdown for the chat paragraph, filled by
/// [`crate::tui::ui::chat::render_unified`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ChatRenderStats {
    /// Layout sync: re-laying-out changed elements (markdown parse + wrap)
    /// and building the tail (queue, input, status).
    pub lines_build: Duration,
    /// Wrapping the tail and computing the document height.
    pub wrap_count: Duration,
    /// `Clear` + `Paragraph` buffer write.
    pub widget: Duration,
    /// Elements re-laid-out this frame (0 while only scrolling).
    pub elements: usize,
    /// Wrapped visual height the render exported.
    pub content_height: usize,
}

/// Timing breakdown of one `ui::render` pass plus the rendered sizes the
/// jank reporter logs alongside it.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameStats {
    /// Whole `ui::render` call.
    pub total: Duration,
    /// `scan_and_fetch_images`.
    pub image_scan: Duration,
    /// The `chat::render_unified` breakdown.
    pub chat: ChatRenderStats,
    /// Plain-text mirror build (now read back lazily from the layout cache,
    /// so this stays zero).
    pub plain_lines: Duration,
    /// Selection overlay, inline images, popups and completion overlay.
    pub overlay: Duration,
}

impl FrameStats {
    /// Approximate backend diff+flush cost: `draw` minus the render pass.
    #[must_use]
    pub const fn diff_flush(self, draw: Duration) -> Duration {
        draw.saturating_sub(self.total)
    }
}

/// Per-iteration timings of `main_event_loop`, excluding the `select` wait.
#[derive(Debug, Default)]
pub struct LoopPhases {
    /// `terminal.draw` (render + diff + backend flush).
    pub draw: Duration,
    /// All non-blocking bookkeeping between draw and select.
    pub housekeeping: Duration,
    /// Slowest single housekeeping step, for attribution.
    pub slowest_step: (&'static str, Duration),
    /// `app.handle_event` + send-action dispatch.
    pub event: Duration,
}

impl LoopPhases {
    /// Record one housekeeping step; keeps the slowest for reporting.
    pub fn step(&mut self, name: &'static str, elapsed: Duration) {
        self.housekeeping += elapsed;
        if elapsed > self.slowest_step.1 {
            self.slowest_step = (name, elapsed);
        }
    }

    /// Time one housekeeping step inline.
    pub fn timed<T>(&mut self, name: &'static str, run: impl FnOnce() -> T) -> T {
        let start = std::time::Instant::now();
        let out = run();
        self.step(name, start.elapsed());
        out
    }

    /// Total busy time this iteration (everything but the select wait).
    #[must_use]
    pub fn busy(&self) -> Duration {
        self.draw + self.housekeeping + self.event
    }
}

/// Jank-report threshold from `MAY_JANK_MS`; defaults to
/// [`DEFAULT_JANK_THRESHOLD_MS`]. `0` disables reporting.
pub fn jank_threshold() -> Duration {
    static THRESHOLD: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        std::env::var("MAY_JANK_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or_else(
                || Duration::from_millis(DEFAULT_JANK_THRESHOLD_MS),
                Duration::from_millis,
            )
    })
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX) / 1_000
}

/// Emit a `jank` tracing event when one event-loop iteration's busy time
/// exceeds the threshold. The report carries every render phase plus the
/// document/scroll state needed to reproduce the stall later.
#[allow(clippy::too_many_arguments)]
pub fn report(
    phases: &LoopPhases,
    stats: &FrameStats,
    scroll: usize,
    plain_lines: usize,
    backlog: usize,
    event: &'static str,
) {
    let busy = phases.busy();
    let threshold = jank_threshold();
    if threshold.is_zero() || busy <= threshold {
        return;
    }
    let (step, step_ms) = (phases.slowest_step.0, millis(phases.slowest_step.1));
    tracing::info!(
        target: "jank",
        busy_ms = millis(busy),
        draw_ms = millis(phases.draw),
        render_ms = millis(stats.total),
        diff_flush_ms = millis(stats.diff_flush(phases.draw)),
        image_scan_ms = millis(stats.image_scan),
        lines_build_ms = millis(stats.chat.lines_build),
        wrap_count_ms = millis(stats.chat.wrap_count),
        widget_ms = millis(stats.chat.widget),
        plain_ms = millis(stats.plain_lines),
        overlay_ms = millis(stats.overlay),
        house_ms = millis(phases.housekeeping),
        house_step = step,
        house_step_ms = step_ms,
        event_ms = millis(phases.event),
        event,
        scroll = if scroll == usize::MAX {
            -1
        } else {
            i64::try_from(scroll).unwrap_or(i64::MAX)
        },
        elements = stats.chat.elements,
        content_height = stats.chat.content_height,
        plain_lines,
        backlog,
        "slow iteration: {}ms busy ({event})", millis(busy),
    );
}
