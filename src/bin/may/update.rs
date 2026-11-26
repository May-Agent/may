//! `may update` subcommand and its terminal progress indicator.

use std::io::{self, IsTerminal as _, Write as _};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use may_agent::update::{Channel, Outcome, Progress, running_executable, update};

/// Braille spinner frames plus a trailing space as the finished glyph.
const SPINNER_TICKS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";
/// Steady tick period for spinner steps (80 ms gives smooth 12.5 Hz motion).
const SPINNER_TICK_INTERVAL: Duration = Duration::from_millis(80);
const SPINNER_TEMPLATE: &str = "{spinner} {msg}";
const DOWNLOAD_KNOWN_TEMPLATE: &str = "  {wide_bar} {decimal_bytes}/{decimal_total_bytes}";
const DOWNLOAD_UNKNOWN_TEMPLATE: &str = "  {decimal_bytes} downloaded";

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template(SPINNER_TEMPLATE)
        .expect("a literal template parses")
        .tick_chars(SPINNER_TICKS)
}

fn download_style(known_total: bool) -> ProgressStyle {
    ProgressStyle::with_template(if known_total {
        DOWNLOAD_KNOWN_TEMPLATE
    } else {
        DOWNLOAD_UNKNOWN_TEMPLATE
    })
    .expect("a literal template parses")
}

/// Active terminal widget owned by [`UpdateProgress`].
enum ActiveDisplay {
    Idle,
    Spinner {
        bar: ProgressBar,
        text: String,
    },
    Download {
        bar: ProgressBar,
        /// The `total` last configured on `bar`, so unchanged totals do not
        /// trigger redundant `set_length` redraws.
        total: Option<u64>,
        /// Whether `bar.finish()` has already forced the 100% frame.
        completed: bool,
    },
}

enum ProgressOutput {
    /// Interactive terminal: animated spinners for step waits and a live bar
    /// for byte downloads.
    Interactive {
        make_target: Box<dyn Fn() -> ProgressDrawTarget + Send>,
        write_line: Box<dyn FnMut(&str) + Send>,
        steady_tick: bool,
        active: ActiveDisplay,
    },
    /// Non-interactive stderr (redirected or piped): plain step lines with no
    /// terminal control sequences.
    Plain {
        write_line: Box<dyn FnMut(&str) + Send>,
    },
}

/// Terminal progress renderer for `may update`.
///
/// Clears any active spinner or progress bar on [`finish`](Self::finish) and
/// on [`Drop`] so errors or early returns never leave stale control output on
/// stderr.
struct UpdateProgress {
    output: ProgressOutput,
}

impl UpdateProgress {
    fn stderr() -> Self {
        let write_line = Box::new(|line: &str| {
            let _ = writeln!(io::stderr(), "{line}");
        });
        if io::stderr().is_terminal() {
            Self {
                output: ProgressOutput::Interactive {
                    make_target: Box::new(ProgressDrawTarget::stderr),
                    write_line,
                    steady_tick: true,
                    active: ActiveDisplay::Idle,
                },
            }
        } else {
            Self {
                output: ProgressOutput::Plain { write_line },
            }
        }
    }

    #[cfg(test)]
    fn for_test(
        make_target: impl Fn() -> ProgressDrawTarget + Send + 'static,
        write_line: impl FnMut(&str) + Send + 'static,
    ) -> Self {
        Self {
            output: ProgressOutput::Interactive {
                make_target: Box::new(make_target),
                write_line: Box::new(write_line),
                steady_tick: false,
                active: ActiveDisplay::Idle,
            },
        }
    }

    #[cfg(test)]
    fn plain_for_test(write_line: impl FnMut(&str) + Send + 'static) -> Self {
        Self {
            output: ProgressOutput::Plain {
                write_line: Box::new(write_line),
            },
        }
    }

    fn handle(&mut self, progress: Progress) {
        match progress {
            Progress::Step(text) => self.on_step(text),
            Progress::Download { downloaded, total } => self.on_download(downloaded, total),
        }
    }

    fn on_step(&mut self, text: String) {
        match &mut self.output {
            ProgressOutput::Plain { write_line } => write_line(&text),
            ProgressOutput::Interactive {
                make_target,
                steady_tick,
                active,
                ..
            } => {
                clear_active(active);
                let bar = ProgressBar::with_draw_target(None, make_target())
                    .with_style(spinner_style())
                    .with_message(text.clone());
                bar.tick();
                if *steady_tick {
                    bar.enable_steady_tick(SPINNER_TICK_INTERVAL);
                }
                *active = ActiveDisplay::Spinner { bar, text };
            }
        }
    }

    fn on_download(&mut self, downloaded: u64, total: Option<u64>) {
        let ProgressOutput::Interactive {
            make_target,
            write_line,
            active,
            ..
        } = &mut self.output
        else {
            return;
        };

        if let ActiveDisplay::Download {
            bar,
            total: known_total,
            completed,
        } = active
        {
            if *known_total == total {
                bar.set_position(downloaded);
            } else {
                match (*known_total, total) {
                    (None, Some(new_total)) => {
                        bar.set_style(download_style(true));
                        bar.update(|state| {
                            state.set_len(new_total);
                            state.set_pos(downloaded);
                        });
                    }
                    (Some(_), Some(new_total)) => {
                        bar.update(|state| {
                            state.set_len(new_total);
                            state.set_pos(downloaded);
                        });
                    }
                    (Some(_), None) => {
                        bar.set_style(download_style(false));
                        bar.unset_length();
                        bar.set_position(downloaded);
                    }
                    (None, None) => unreachable!("checked known_total != total"),
                }
                *known_total = total;
            }

            if !*completed && total.is_some_and(|len| downloaded >= len) {
                bar.finish();
                *completed = true;
            }
            return;
        }

        // Transition from a step spinner (e.g. `Downloading …`) to the live
        // progress bar: clear the spinner and print its heading cleanly above
        // the bar.
        let heading = match std::mem::replace(active, ActiveDisplay::Idle) {
            ActiveDisplay::Spinner { bar, text } => {
                bar.disable_steady_tick();
                bar.finish_and_clear();
                Some(text)
            }
            ActiveDisplay::Idle | ActiveDisplay::Download { .. } => None,
        };
        if let Some(heading) = heading {
            write_line(&heading);
        }

        let bar = ProgressBar::with_draw_target(total, make_target())
            .with_style(download_style(total.is_some()))
            .with_position(downloaded);
        let completed = total.is_some_and(|len| downloaded >= len);
        if completed {
            bar.finish();
        } else {
            bar.tick();
        }
        *active = ActiveDisplay::Download {
            bar,
            total,
            completed,
        };
    }

    fn finish(&mut self) {
        if let ProgressOutput::Interactive { active, .. } = &mut self.output {
            clear_active(active);
        }
    }
}

impl Drop for UpdateProgress {
    fn drop(&mut self) {
        self.finish();
    }
}

fn clear_active(active: &mut ActiveDisplay) {
    match std::mem::replace(active, ActiveDisplay::Idle) {
        ActiveDisplay::Idle => {}
        ActiveDisplay::Spinner { bar, .. } => {
            bar.disable_steady_tick();
            bar.finish_and_clear();
        }
        ActiveDisplay::Download { bar, .. } => {
            bar.finish_and_clear();
        }
    }
}

/// `may update`: replaces this binary on its channel, then restarts a
/// running daemon onto the new one, gracefully, at once. A daemon that is
/// not running is not started. The restart is the new binary's own
/// `daemon restart`: this process is still the old build.
///
/// `channel` switches the channel first and keeps the choice in the
/// configuration, through the daemon when one runs, which applies it live.
pub async fn run_update(channel: Option<Channel>) -> anyhow::Result<()> {
    let paths = may_agent::daemon::DaemonPaths::user()?;
    let daemon = may_agent::client::DaemonClient::new(paths.socket());
    let channel = match channel {
        Some(channel) => {
            if daemon.health().await.is_ok() {
                let _: serde_json::Value = daemon
                    .put(
                        "/api/config/update-channel",
                        &serde_json::json!({ "channel": channel }),
                    )
                    .await?;
            } else {
                let mut config = may_agent::config::MayConfig::load().await?;
                config.update.channel = Some(channel);
                config.save().await?;
            }
            eprintln!("Following the {channel} channel.");
            channel
        }
        None => may_agent::config::MayConfig::load().await?.update.channel(),
    };
    let target = running_executable()?;
    let mut progress_ui = UpdateProgress::stderr();
    let outcome = update(channel, &target, |progress| progress_ui.handle(progress)).await?;
    progress_ui.finish();
    match outcome {
        Outcome::UpToDate => eprintln!("May is up to date."),
        Outcome::Updated { executable, to } => {
            eprintln!("Updated May to {to}.");
            if daemon.health().await.is_ok() {
                eprintln!("Restarting the May daemon…");
                let status = async_process::Command::new(&executable)
                    .args(["daemon", "restart"])
                    .status()
                    .await?;
                anyhow::ensure!(status.success(), "the new May could not restart the daemon");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use indicatif::TermLike;

    use super::*;

    #[derive(Debug, Default)]
    struct RecordedTermState {
        current: String,
        flushed_frames: Vec<String>,
        clear_count: usize,
    }

    /// Minimal `TermLike` recording every flushed frame written by `indicatif`.
    #[derive(Debug, Clone)]
    struct RecordingTerm {
        width: u16,
        state: Arc<Mutex<RecordedTermState>>,
    }

    impl RecordingTerm {
        fn new(width: u16) -> Self {
            Self {
                width,
                state: Arc::new(Mutex::new(RecordedTermState::default())),
            }
        }

        fn frames(&self) -> Vec<String> {
            self.state
                .lock()
                .expect("term state")
                .flushed_frames
                .clone()
        }

        fn non_empty_frames(&self) -> Vec<String> {
            self.frames()
                .into_iter()
                .filter(|frame| !frame.is_empty())
                .collect()
        }
    }

    impl TermLike for RecordingTerm {
        fn width(&self) -> u16 {
            self.width
        }

        fn move_cursor_up(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_down(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_right(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn write_line(&self, s: &str) -> io::Result<()> {
            let mut state = self.state.lock().expect("term state");
            state.current.push_str(s);
            state.current.push('\n');
            drop(state);
            Ok(())
        }

        fn write_str(&self, s: &str) -> io::Result<()> {
            self.state.lock().expect("term state").current.push_str(s);
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            let mut state = self.state.lock().expect("term state");
            state.current.clear();
            state.clear_count += 1;
            drop(state);
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            let mut state = self.state.lock().expect("term state");
            let frame = state.current.trim_end().to_owned();
            state.flushed_frames.push(frame);
            drop(state);
            Ok(())
        }
    }

    fn make_ui(
        term: &RecordingTerm,
        refresh_hz: Option<u8>,
    ) -> (UpdateProgress, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let lines_out = Arc::clone(&lines);
        let term = term.clone();
        let ui = UpdateProgress::for_test(
            move || {
                refresh_hz.map_or_else(
                    || ProgressDrawTarget::term_like(Box::new(term.clone())),
                    |hz| ProgressDrawTarget::term_like_with_hz(Box::new(term.clone()), hz),
                )
            },
            move |line| {
                lines_out.lock().expect("lines").push(line.to_owned());
            },
        );
        (ui, lines)
    }

    #[test]
    fn constant_total_does_not_cause_duplicate_stale_redraws() {
        let term = RecordingTerm::new(100);
        let (mut ui, lines) = make_ui(&term, None);

        ui.handle(Progress::Step("Downloading May 0.1.1…".into()));
        let total = 10_000_000;
        ui.handle(Progress::Download {
            downloaded: 1,
            total: Some(total),
        });
        ui.handle(Progress::Download {
            downloaded: 2_500_000,
            total: Some(total),
        });
        ui.handle(Progress::Download {
            downloaded: 5_000_000,
            total: Some(total),
        });

        assert_eq!(
            *lines.lock().expect("lines"),
            vec!["Downloading May 0.1.1…"]
        );

        let frames = term.non_empty_frames();
        // Frame 0 is the initial spinner, followed by one frame per download update —
        // no `0 B` pre-draw and no stale-position duplicate before each new value.
        assert_eq!(frames.len(), 4, "unexpected frames: {frames:#?}");
        assert!(frames[0].contains("Downloading May 0.1.1…"));
        assert!(frames[1].contains("1 B/10.00 MB"), "{}", frames[1]);
        assert!(frames[2].contains("2.50 MB/10.00 MB"), "{}", frames[2]);
        assert!(frames[3].contains("5.00 MB/10.00 MB"), "{}", frames[3]);
    }

    #[test]
    fn final_hundred_percent_renders_even_when_rate_limited() {
        let term = RecordingTerm::new(100);
        let (mut ui, _) = make_ui(&term, Some(20));

        let total = 119_626_788;
        // Exhaust indicatif's 20 Hz rate limiter burst (MAX_BURST = 20) in tight succession.
        for step in 1..=200_u64 {
            ui.handle(Progress::Download {
                downloaded: step * (total / 250),
                total: Some(total),
            });
        }
        // Final completion snapshot arrives immediately inside the throttled window.
        ui.handle(Progress::Download {
            downloaded: total,
            total: Some(total),
        });

        let frames = term.non_empty_frames();
        let last = frames.last().expect("at least one rendered frame");
        assert!(
            last.contains("119.63 MB/119.63 MB"),
            "final 100% frame was dropped under rate limiting: last={last:?}"
        );
    }

    #[test]
    fn unknown_total_switches_to_bar_template_when_total_arrives() {
        let term = RecordingTerm::new(100);
        let (mut ui, _) = make_ui(&term, None);

        ui.handle(Progress::Download {
            downloaded: 500_000,
            total: None,
        });
        ui.handle(Progress::Download {
            downloaded: 1_000_000,
            total: Some(2_000_000),
        });
        ui.handle(Progress::Download {
            downloaded: 2_000_000,
            total: Some(2_000_000),
        });

        let frames = term.non_empty_frames();
        assert!(frames[0].contains("500.00 kB downloaded"), "{}", frames[0]);
        assert!(
            frames[1].contains("1.00 MB/2.00 MB"),
            "template did not switch on None -> Some: {}",
            frames[1]
        );
        assert!(frames[2].contains("2.00 MB/2.00 MB"), "{}", frames[2]);
    }

    #[test]
    fn step_after_download_clears_bar_and_starts_spinner() {
        let term = RecordingTerm::new(100);
        let (mut ui, _) = make_ui(&term, None);

        ui.handle(Progress::Step("Downloading the CI build…".into()));
        ui.handle(Progress::Download {
            downloaded: 1_000_000,
            total: Some(1_000_000),
        });
        ui.handle(Progress::Step("Verifying download…".into()));
        ui.handle(Progress::Step("Unpacking the CI build…".into()));

        let frames = term.non_empty_frames();
        assert!(
            frames.iter().any(|frame| frame.contains("1.00 MB/1.00 MB")),
            "expected completed download bar before verification: {frames:#?}"
        );
        assert!(
            frames
                .last()
                .is_some_and(|frame| frame.contains("Unpacking the CI build…")),
            "expected unpacking spinner: {frames:#?}"
        );

        drop(ui);
        assert_eq!(
            term.frames().last().map(String::as_str),
            Some(""),
            "dropping UpdateProgress must clear the active bar"
        );
    }

    #[test]
    fn plain_mode_writes_step_lines_without_escape_codes() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let lines_out = Arc::clone(&lines);
        let mut ui = UpdateProgress::plain_for_test(move |line| {
            lines_out.lock().expect("lines").push(line.to_owned());
        });

        ui.handle(Progress::Step("Looking for the latest release…".into()));
        ui.handle(Progress::Download {
            downloaded: 1024,
            total: Some(2048),
        });
        ui.handle(Progress::Step("Verifying download…".into()));
        ui.finish();

        assert_eq!(
            *lines.lock().expect("lines"),
            vec!["Looking for the latest release…", "Verifying download…"]
        );
    }
}
