//! Per-request accounting for one agent attempt.
//!
//! The model emits one `Usage` event per request, and a turn with tool calls
//! makes several requests. The ledger turns each into its own `request_logs`
//! row carrying that request's tokens and its own elapsed time.

use std::time::Instant;

use aither_core::llm::Usage;

use crate::daemon::types::RequestLogEntry;

pub(super) struct RequestLedger<'a> {
    session_id: &'a str,
    model: &'a str,
    provider_id: &'a str,
    context_window: Option<u32>,
    request_started: Instant,
    logged: u32,
}

impl<'a> RequestLedger<'a> {
    pub(super) const fn new(
        session_id: &'a str,
        model: &'a str,
        provider_id: &'a str,
        context_window: Option<u32>,
        now: Instant,
    ) -> Self {
        Self {
            session_id,
            model,
            provider_id,
            context_window,
            request_started: now,
            logged: 0,
        }
    }

    fn base(&self, usage: &Usage) -> RequestLogEntry {
        RequestLogEntry::new(
            self.session_id,
            self.model,
            self.provider_id,
            usage,
            self.context_window,
        )
    }

    fn lap(&mut self, now: Instant) -> u64 {
        let elapsed = now.saturating_duration_since(self.request_started);
        self.request_started = now;
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    }

    /// A request finished and reported its usage.
    pub(super) fn completed(&mut self, usage: &Usage, now: Instant) -> RequestLogEntry {
        self.logged += 1;
        let elapsed = self.lap(now);
        self.base(usage).success(elapsed)
    }

    /// The request in flight ended without reporting usage (error, takeover
    /// or cancellation); its tokens, if any, were never counted.
    pub(super) fn interrupted(&mut self, now: Instant, error: &str) -> RequestLogEntry {
        self.logged += 1;
        let elapsed = self.lap(now);
        self.base(&Usage::default()).failure(elapsed, error)
    }

    /// The attempt succeeded. A provider that never reported usage still gets
    /// one row for its request; otherwise every request is already logged.
    pub(super) fn finished(&mut self, now: Instant) -> Option<RequestLogEntry> {
        (self.logged == 0).then(|| {
            self.logged += 1;
            let elapsed = self.lap(now);
            self.base(&Usage::default()).success(elapsed)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn each_usage_event_yields_its_own_row() {
        let t0 = Instant::now();
        let mut ledger = RequestLedger::new("s", "m", "p", Some(1000), t0);
        let a = ledger.completed(&Usage::new(100, 10), t0 + Duration::from_millis(500));
        let b = ledger.completed(&Usage::new(200, 20), t0 + Duration::from_millis(1700));
        let c = ledger.completed(&Usage::new(300, 30), t0 + Duration::from_millis(2000));
        assert_eq!(
            [a.prompt_tokens, b.prompt_tokens, c.prompt_tokens],
            [Some(100), Some(200), Some(300)]
        );
        assert_eq!([a.elapsed_ms, b.elapsed_ms, c.elapsed_ms], [500, 1200, 300]);
        assert_eq!(b.total_tokens, Some(220));
        assert!(a.ok && b.ok && c.ok);
        assert!(ledger.finished(t0 + Duration::from_secs(3)).is_none());
    }

    #[test]
    fn interrupted_request_logs_no_tokens_already_counted() {
        let t0 = Instant::now();
        let mut ledger = RequestLedger::new("s", "m", "p", None, t0);
        ledger.completed(&Usage::new(100, 10), t0 + Duration::from_millis(400));
        let failed = ledger.interrupted(t0 + Duration::from_millis(900), "boom");
        assert!(!failed.ok);
        assert_eq!(failed.prompt_tokens, None);
        assert_eq!(failed.elapsed_ms, 500);
        assert_eq!(failed.error.as_deref(), Some("boom"));
    }

    #[test]
    fn provider_without_usage_still_logs_one_row() {
        let t0 = Instant::now();
        let mut ledger = RequestLedger::new("s", "m", "p", None, t0);
        let row = ledger
            .finished(t0 + Duration::from_millis(250))
            .expect("one row");
        assert!(row.ok);
        assert_eq!(row.elapsed_ms, 250);
    }
}
