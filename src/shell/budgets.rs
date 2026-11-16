//! The shell's time bounds and output budgets, as live handles a saved
//! config change moves while sessions run.

use aither_sandbox::{
    ForegroundWait, ImportBudget, LiveDuration, MediaBounds, OutputLimits, OutputPolicy,
    ResponseWindow, ShellLimits,
};

use crate::config::ShellSettings;

/// The handles every shell of the daemon is built with.
///
/// A clone shares the handles, so the daemon keeps one, builds every shell
/// from it and [`apply`](Self::apply)s each config change to it.
#[derive(Debug, Clone)]
pub struct ShellBudgets {
    limits: ShellLimits,
    output: OutputPolicy,
    monitor_max_timeout: LiveDuration,
    monitor_default_interval: LiveDuration,
}

impl ShellBudgets {
    /// Handles holding the values of `settings`.
    #[must_use]
    pub fn of(settings: &ShellSettings) -> Self {
        Self {
            limits: ShellLimits {
                foreground_wait: ForegroundWait::new(settings.foreground_wait),
                timeout_cap: LiveDuration::new(settings.timeout_cap),
                long_job_alert: LiveDuration::new(settings.long_job_alert),
                subagent_import: ImportBudget::new(settings.subagent_import_bytes),
                response_window: ResponseWindow::new(settings.response_window),
            },
            output: OutputPolicy::new(Self::output_limits(settings)),
            monitor_max_timeout: LiveDuration::new(settings.monitor_max_timeout),
            monitor_default_interval: LiveDuration::new(settings.monitor_default_interval),
        }
    }

    const fn output_limits(settings: &ShellSettings) -> OutputLimits {
        OutputLimits {
            inline_bytes: settings.inline_output_bytes,
            preview_bytes: settings.preview_bytes,
            default_lines: settings.default_lines,
            ceiling_lines: settings.ceiling_lines,
            read_bytes: settings.read_bytes,
            media: MediaBounds {
                low: settings.media_low,
                medium: settings.media_medium,
                high: settings.media_high,
            },
            job_preview_bytes: settings.job_preview_bytes,
        }
    }

    /// Moves every handle to the values of `settings`.
    pub fn apply(&self, settings: &ShellSettings) {
        self.limits
            .foreground_wait
            .set_normal(settings.foreground_wait);
        self.limits.timeout_cap.set(settings.timeout_cap);
        self.limits.long_job_alert.set(settings.long_job_alert);
        self.limits
            .subagent_import
            .set(settings.subagent_import_bytes);
        self.limits.response_window.set(settings.response_window);
        self.output.set(Self::output_limits(settings));
        self.monitor_max_timeout.set(settings.monitor_max_timeout);
        self.monitor_default_interval
            .set(settings.monitor_default_interval);
    }

    /// The limits a shell is built with.
    #[must_use]
    pub fn limits(&self) -> ShellLimits {
        self.limits.clone()
    }

    /// The output policy a shell is built with.
    #[must_use]
    pub fn output(&self) -> OutputPolicy {
        self.output.clone()
    }

    /// The longest a `monitor` may run.
    #[must_use]
    pub fn monitor_max_timeout(&self) -> LiveDuration {
        self.monitor_max_timeout.clone()
    }

    /// The interval between a `monitor`'s deliveries when its call states
    /// none.
    #[must_use]
    pub fn monitor_default_interval(&self) -> LiveDuration {
        self.monitor_default_interval.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_config_change_reaches_the_handles_a_shell_holds() {
        let budgets = ShellBudgets::of(&ShellSettings::default());
        let held = budgets.limits();
        let output = budgets.output();
        assert_eq!(held.timeout_cap.get(), Duration::from_secs(7200));
        assert_eq!(output.lines(None), 200);
        let changed = ShellSettings {
            timeout_cap: Duration::from_secs(60),
            default_lines: 50,
            ..ShellSettings::default()
        };
        budgets.apply(&changed);
        assert_eq!(held.timeout_cap.get(), Duration::from_secs(60));
        assert_eq!(output.lines(None), 50);
    }
}
