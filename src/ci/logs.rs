//! Per-test durations out of the log of a step that ran tests.
//!
//! Three formats are read: `cargo test` (the per-binary totals and the
//! `has been running for over N seconds` markers, plus the per-test times
//! `--report-time` adds), `cargo nextest` and `bun test`. GitHub's line
//! timestamps and ANSI colour are ignored, so a raw job log parses as it is.

/// What a timing measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestKind {
    /// One test's duration.
    Test,
    /// A whole test binary's total, as `cargo test` prints it.
    Binary,
    /// A test still running when the runner warned about it: the duration is
    /// a lower bound.
    RunningOver,
}

impl TestKind {
    /// The name the database stores.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Binary => "binary",
            Self::RunningOver => "running_over",
        }
    }

    /// The kind a stored name stands for.
    #[must_use]
    pub fn from_stored(name: &str) -> Option<Self> {
        match name {
            "test" => Some(Self::Test),
            "binary" => Some(Self::Binary),
            "running_over" => Some(Self::RunningOver),
            _ => None,
        }
    }
}

/// One duration a log reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestTiming {
    /// What it measures.
    pub kind: TestKind,
    /// The test, or the test binary for [`TestKind::Binary`].
    pub name: String,
    /// The duration in milliseconds.
    pub millis: u64,
}

/// Every timing in `log`, in the order the log reports them.
#[must_use]
pub fn parse(log: &str) -> Vec<TestTiming> {
    let lines: Vec<String> = log.lines().map(clean).collect();
    let mut timings = parse_cargo_test(&lines);
    timings.extend(parse_nextest(&lines));
    timings.extend(parse_bun(&lines));
    timings
}

/// The `limit` longest timings, longest first.
#[must_use]
pub fn slowest(timings: &[TestTiming], limit: usize) -> Vec<&TestTiming> {
    let mut sorted: Vec<&TestTiming> = timings.iter().collect();
    sorted.sort_by_key(|timing| std::cmp::Reverse(timing.millis));
    sorted.truncate(limit);
    sorted
}

/// Longest line an excerpt keeps, in bytes (cut at a character boundary).
const EXCERPT_LINE_BYTES: usize = 240;

/// The lines of a failed job's log that say what went wrong: GitHub's
/// `##[error]` annotations and the compiler, test-runner and panic lines
/// before them. The first `limit` distinct ones, in log order.
#[must_use]
pub fn error_lines(log: &str, limit: usize) -> Vec<String> {
    const MARKERS: [&str; 7] = [
        "##[error]",
        "error:",
        "error[",
        "FAILED",
        "panicked at",
        "Error:",
        "AssertionError",
    ];
    let mut found: Vec<String> = Vec::new();
    for line in log.lines().map(clean) {
        let line = line.trim();
        if MARKERS.iter().any(|marker| line.contains(marker)) && !found.iter().any(|l| l == line) {
            found.push(truncate(line).to_string());
            if found.len() == limit {
                break;
            }
        }
    }
    found
}

fn truncate(line: &str) -> &str {
    let mut end = EXCERPT_LINE_BYTES.min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// `line` without ANSI colour and without GitHub's leading timestamp.
fn clean(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut kept = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() && !bytes[index].is_ascii_alphabetic() {
                index += 1;
            }
        } else {
            kept.push(bytes[index]);
        }
        index += 1;
    }
    let text = String::from_utf8_lossy(&kept).into_owned();
    match text.split_once(' ') {
        Some((stamp, rest))
            if stamp.len() >= 20
                && stamp.as_bytes()[0].is_ascii_digit()
                && stamp.as_bytes()[4] == b'-'
                && stamp.as_bytes()[10] == b'T'
                && stamp.ends_with('Z') =>
        {
            rest.to_string()
        }
        _ => text,
    }
}

/// `1.23s`, `701.42ms`, `12us` or `> 60.000s` as milliseconds.
fn duration_ms(text: &str) -> Option<u64> {
    let text = text.trim().trim_start_matches('>').trim();
    let (number, scale) = if let Some(number) = text.strip_suffix("ms") {
        (number, 1.0)
    } else if let Some(number) = text.strip_suffix("µs").or_else(|| text.strip_suffix("us")) {
        (number, 0.001)
    } else {
        (text.strip_suffix('s')?, 1000.0)
    };
    let value: f64 = number.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((value * scale).round() as u64)
}

fn parse_cargo_test(lines: &[String]) -> Vec<TestTiming> {
    let mut timings = Vec::new();
    let mut binary = String::new();
    for line in lines {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Running ") {
            binary = binary_name(rest);
        } else if let Some(rest) = line.strip_prefix("Doc-tests ") {
            binary = format!("doc-tests {}", rest.trim());
        } else if line.starts_with("test result:")
            && let Some((_, finished)) = line.rsplit_once("finished in ")
            && let Some(millis) = duration_ms(finished)
        {
            timings.push(TestTiming {
                kind: TestKind::Binary,
                name: binary.clone(),
                millis,
            });
        } else if let Some(rest) = line.strip_prefix("test ") {
            if let Some((name, marker)) = rest.split_once(" has been running for over ")
                && let Some(seconds) = marker.strip_suffix(" seconds")
                && let Ok(seconds) = seconds.trim().parse::<u64>()
            {
                timings.push(TestTiming {
                    kind: TestKind::RunningOver,
                    name: name.to_string(),
                    millis: seconds * 1000,
                });
            } else if let Some((name, result)) = rest.split_once(" ... ")
                && let Some((_, report)) = result.rsplit_once('<')
                && let Some(report) = report.strip_suffix('>')
                && let Some(millis) = duration_ms(report)
            {
                timings.push(TestTiming {
                    kind: TestKind::Test,
                    name: name.to_string(),
                    millis,
                });
            }
        }
    }
    timings
}

/// The binary of a `Running ...` line: its source file, without the
/// `unittests` label and the build path in parentheses.
fn binary_name(running: &str) -> String {
    let without_path = running
        .rsplit_once(" (")
        .map_or(running, |(before, _)| before);
    without_path
        .strip_prefix("unittests ")
        .unwrap_or(without_path)
        .trim()
        .to_string()
}

fn parse_nextest(lines: &[String]) -> Vec<TestTiming> {
    const STATUSES: [&str; 5] = ["PASS", "FAIL", "SLOW", "TIMEOUT", "LEAK"];
    let mut timings = Vec::new();
    for line in lines {
        let line = line.trim();
        let Some((status, rest)) = line.split_once(' ') else {
            continue;
        };
        if !STATUSES.contains(&status) {
            continue;
        }
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('[') else {
            continue;
        };
        let Some((time, name)) = rest.split_once(']') else {
            continue;
        };
        let Some(millis) = duration_ms(time) else {
            continue;
        };
        let name = name.trim_start();
        // The progress counter, `(3/5)` or `(───)`, precedes the name.
        let name = match name.split_once(' ') {
            Some((counter, after)) if counter.starts_with('(') && counter.ends_with(')') => after,
            _ => name,
        };
        timings.push(TestTiming {
            kind: if status == "SLOW" {
                TestKind::RunningOver
            } else {
                TestKind::Test
            },
            name: name.trim().to_string(),
            millis,
        });
    }
    timings
}

fn parse_bun(lines: &[String]) -> Vec<TestTiming> {
    let mut timings = Vec::new();
    let mut file = String::new();
    for line in lines {
        let line = line.trim_end();
        if let Some(path) = line.strip_suffix(':')
            && !path.contains(' ')
            && (path.contains(".test.") || path.contains(".spec."))
        {
            file = path.to_string();
            continue;
        }
        let Some(rest) = line
            .strip_prefix("(pass) ")
            .or_else(|| line.strip_prefix("(fail) "))
        else {
            continue;
        };
        let Some((name, time)) = rest.rsplit_once(" [") else {
            continue;
        };
        let Some(millis) = time.strip_suffix(']').and_then(duration_ms) else {
            continue;
        };
        timings.push(TestTiming {
            kind: TestKind::Test,
            name: if file.is_empty() {
                name.to_string()
            } else {
                format!("{file} > {name}")
            },
            millis,
        });
    }
    timings
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO: &str = include_str!("../../tests/fixtures/ci/cargo_test.log");
    const NEXTEST: &str = include_str!("../../tests/fixtures/ci/nextest.log");
    const BUN: &str = include_str!("../../tests/fixtures/ci/bun_test.log");

    #[test]
    fn the_error_lines_of_a_failed_log_are_the_first_distinct_ones() {
        let log = "2026-11-09T10:00:00.1Z compiling\n\
                   2026-11-09T10:00:01.2Z \u{1b}[31merror[E0308]\u{1b}[0m: 类型不匹配 型の不一致 타입 불일치\n\
                   2026-11-09T10:00:02.3Z error[E0308]: 类型不匹配 型の不一致 타입 불일치\n\
                   2026-11-09T10:00:03.4Z test a::b ... FAILED\n\
                   2026-11-09T10:00:04.5Z ##[error]Process completed with exit code 101.\n";
        assert_eq!(
            error_lines(log, 2),
            [
                "error[E0308]: 类型不匹配 型の不一致 타입 불일치",
                "test a::b ... FAILED"
            ]
        );
        assert_eq!(error_lines("all fine\n", 5), [] as [String; 0]);
        let wide = format!("error: {}", "测".repeat(200));
        assert!(error_lines(&wide, 1)[0].len() <= EXCERPT_LINE_BYTES);
    }

    fn find<'a>(timings: &'a [TestTiming], kind: TestKind, name: &str) -> &'a TestTiming {
        timings
            .iter()
            .find(|t| t.kind == kind && t.name == name)
            .unwrap_or_else(|| panic!("no {kind:?} {name} in {timings:?}"))
    }

    #[test]
    fn cargo_test_reports_binary_totals_and_the_over_sixty_second_marker() {
        let timings = parse(CARGO);
        assert_eq!(
            find(&timings, TestKind::Binary, "src/lib.rs").millis,
            62_000
        );
        assert_eq!(
            find(&timings, TestKind::Binary, "tests/integration.rs").millis,
            2_100
        );
        assert_eq!(
            find(&timings, TestKind::Binary, "doc-tests sample").millis,
            0
        );
        assert_eq!(
            find(&timings, TestKind::RunningOver, "tests::slow_index_rebuild").millis,
            60_000
        );
        assert_eq!(timings.len(), 4);
        let slowest = slowest(&timings, 1);
        assert_eq!(slowest[0].name, "src/lib.rs");
    }

    #[test]
    fn cargo_test_reads_per_test_report_times() {
        let log = "test a::fast ... ok <0.015s>\ntest a::slow ... ok <12.500s>\ntest b ... ok\n";
        let timings = parse(log);
        assert_eq!(
            timings,
            [
                TestTiming {
                    kind: TestKind::Test,
                    name: "a::fast".into(),
                    millis: 15
                },
                TestTiming {
                    kind: TestKind::Test,
                    name: "a::slow".into(),
                    millis: 12_500
                },
            ]
        );
    }

    #[test]
    fn nextest_reports_every_test_and_the_slow_marker() {
        let timings = parse(NEXTEST);
        assert_eq!(
            find(&timings, TestKind::Test, "sample tests::slow_index_rebuild").millis,
            62_026
        );
        assert_eq!(
            find(&timings, TestKind::Test, "sample tests::fast_parse").millis,
            40
        );
        assert_eq!(
            find(&timings, TestKind::Test, "sample::integration cold_start").millis,
            2_125
        );
        assert_eq!(
            find(
                &timings,
                TestKind::RunningOver,
                "sample tests::slow_index_rebuild"
            )
            .millis,
            60_000
        );
        assert_eq!(timings.len(), 6);
    }

    #[test]
    fn bun_test_reports_each_test_under_its_file() {
        let timings = parse(BUN);
        assert_eq!(
            find(
                &timings,
                TestKind::Test,
                "slow.test.ts > rebuilds the index"
            )
            .millis,
            1_507
        );
        assert_eq!(
            find(&timings, TestKind::Test, "other.test.ts > cache > warms up").millis,
            701
        );
        assert_eq!(timings.len(), 5);
        let slowest = slowest(&timings, 2);
        assert_eq!(slowest[0].name, "slow.test.ts > rebuilds the index");
        assert_eq!(slowest[1].name, "other.test.ts > cache > warms up");
    }

    #[test]
    fn colour_and_timestamps_do_not_change_what_is_parsed() {
        let plain = "        PASS [   0.040s] (1/5) sample tests::fast_parse";
        let dressed = "2026-11-06T10:14:03.1234567Z \u{1b}[32m        PASS\u{1b}[0m [   0.040s] (1/5) sample tests::fast_parse";
        assert_eq!(parse(plain), parse(dressed));
        assert_eq!(parse(plain).len(), 1);
    }

    #[test]
    fn a_log_without_tests_has_no_timings() {
        assert_eq!(parse("Compiling foo\nFinished dev in 3s\n"), []);
    }
}
