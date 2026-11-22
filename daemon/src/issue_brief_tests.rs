//! A failed issue lookup is logged at WARN.

use std::io::Write;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

use super::sessions::unresolved_issue_brief;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl MakeWriter<'_> for Captured {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

/// A failed issue lookup leaves a WARN line naming the issue and the error,
/// in Chinese, Japanese and Korean as well as English.
#[test]
fn a_failed_issue_lookup_is_logged_at_warn() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    let errors = [
        "issue not found",
        "未找到该工单",
        "チケットが見つかりません",
        "이슈를 찾을 수 없습니다",
    ];
    tracing::subscriber::with_default(subscriber, || {
        for (number, error) in (1_i64..).zip(errors) {
            let brief = unresolved_issue_brief(number, &error);
            assert_eq!(brief.number, number);
            assert!(brief.title.is_none());
            assert!(!brief.closed);
        }
    });
    let log = String::from_utf8(captured.0.lock().expect("log buffer").clone()).expect("UTF-8 log");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), errors.len(), "{log}");
    for (line, error) in lines.iter().zip(errors) {
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("issue lookup failed"), "{line}");
        assert!(line.contains(error), "{line}");
    }
}
