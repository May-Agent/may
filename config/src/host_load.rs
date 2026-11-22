//! The host-load monitor's settings: what a machine load is and when it is
//! called high.
//!
//! The monitor, the platform readers and the alerts live in `may-agent`'s
//! harness; the thresholds a `[monitor]` config section tunes are here.

use std::time::Duration;

/// The machine's load, `1.0` at capacity.
///
/// `1.0` is a machine whose cores are all busy. What exceeds it is platform
/// specific: where the host reports a load average (macOS) it is that load
/// divided by the number of cores, so a queue shows as more than `1.0`;
/// on Linux it is the share of the last ten seconds during which runnable
/// tasks waited for a CPU (PSI `cpu`), and on Windows the share of CPU time
/// not spent idle since the previous reading.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct MachineLoad(pub f64);

impl MachineLoad {
    /// The load as a whole percentage of the machine's capacity.
    #[must_use]
    pub fn percent(self) -> u64 {
        // Truncation is the point: the percentage is for a human to read.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let percent = (self.0 * 100.0).round().max(0.0) as u64;
        percent
    }
}

/// The machine load this platform's reading is high at by default; see
/// [`MachineLoad`] for what it measures.
#[cfg(target_vendor = "apple")]
pub const DEFAULT_LOAD_THRESHOLD: f64 = 1.0;

/// Stall time of the last ten seconds at which tasks waiting for a CPU
/// are a load worth telling the agent about.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub const DEFAULT_LOAD_THRESHOLD: f64 = 0.3;

/// The share of CPU time not spent idle at which load is worth telling the
/// agent about.
#[cfg(windows)]
pub const DEFAULT_LOAD_THRESHOLD: f64 = 0.9;

/// The machine load a platform with no reading treats as its threshold.
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    windows
)))]
pub const DEFAULT_LOAD_THRESHOLD: f64 = 1.0;

/// When the monitor calls a condition high, and how long it must hold.
#[derive(Debug, Clone, PartialEq)]
pub struct HostLoadConfig {
    /// Machine load at or above this is high; see [`MachineLoad`] for what
    /// the platform measures.
    pub load_threshold: MachineLoad,
    /// How long a condition must stay above its threshold to start.
    pub start_after: Duration,
    /// How long a condition must stay below its threshold to end.
    pub end_after: Duration,
    /// How often the host is read.
    pub poll_interval: Duration,
    /// The terminal's foreground window while high machine load lasts.
    pub high_load_foreground_wait: Duration,
}

impl HostLoadConfig {
    /// The defaults for the platform the agent runs on.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            load_threshold: MachineLoad(DEFAULT_LOAD_THRESHOLD),
            start_after: Duration::from_secs(30),
            end_after: Duration::from_secs(60),
            poll_interval: Duration::from_secs(5),
            high_load_foreground_wait: Duration::from_secs(60),
        }
    }

    /// Sets the machine load at which the condition is high.
    #[must_use]
    pub const fn load_threshold(mut self, threshold: MachineLoad) -> Self {
        self.load_threshold = threshold;
        self
    }

    /// Sets how long a condition must stay above its threshold to start.
    #[must_use]
    pub const fn start_after(mut self, window: Duration) -> Self {
        self.start_after = window;
        self
    }

    /// Sets how long a condition must stay below its threshold to end.
    #[must_use]
    pub const fn end_after(mut self, window: Duration) -> Self {
        self.end_after = window;
        self
    }

    /// Sets how often the host is read.
    #[must_use]
    pub const fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Sets the terminal's foreground window while high load lasts.
    #[must_use]
    pub const fn high_load_foreground_wait(mut self, window: Duration) -> Self {
        self.high_load_foreground_wait = window;
        self
    }
}

impl Default for HostLoadConfig {
    fn default() -> Self {
        Self::new()
    }
}
