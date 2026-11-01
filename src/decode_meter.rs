//! Real-time decode (output token) throughput measurement.

use std::time::{Duration, Instant};

/// Estimated bytes per output token, matching
/// `aither_agent::compression::estimate_tokens` (~4 bytes per token).
const BYTES_PER_TOKEN: u64 = 4;

/// Minimum decode time before a rate is reported; over shorter windows timer
/// granularity dominates and produces meaningless spikes.
const MIN_MEASUREMENT: Duration = Duration::from_millis(200);

/// A gap between output chunks longer than this means the model was
/// buffering server-side (for instance generating a large tool-call
/// argument the provider does not stream), not decoding — the pause is not
/// decode time and must not dilute the measured rate.
const OUTPUT_GAP: Duration = Duration::from_secs(2);

/// Measures output-token throughput while a model streams a response.
///
/// Output events (text, reasoning, tool-call fragments) arrive in bursts: one
/// per model call, separated by prefill and tool execution. Decode time is
/// accumulated across bursts so pauses between calls don't dilute the rate.
///
/// Tokens are estimated from output bytes until a `Usage` report provides an
/// exact completion count for the calls it covers.
#[derive(Debug, Default)]
pub struct DecodeMeter {
    /// Start of the current output burst; `None` between bursts.
    burst_start: Option<Instant>,
    /// When the most recent output chunk arrived; `None` between bursts.
    last_output_at: Option<Instant>,
    /// Decode time accumulated over completed bursts.
    elapsed: Duration,
    /// Exact completion tokens reported by `Usage` events.
    exact_tokens: u64,
    /// Output bytes streamed since the last `Usage` report.
    in_flight_bytes: u64,
}

impl DecodeMeter {
    /// Reset the meter for a new run.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Record `bytes` of newly streamed model output.
    ///
    /// A chunk arriving after [`OUTPUT_GAP`] of silence closes the open
    /// window at the previous chunk instead of letting the pause inflate
    /// decode time.
    pub fn note_output(&mut self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_output_at
            && now.duration_since(last) > OUTPUT_GAP
            && let Some(start) = self.burst_start.take()
        {
            self.elapsed += self.open_window_end(now).saturating_duration_since(start);
        }
        self.burst_start.get_or_insert(now);
        self.last_output_at = Some(now);
        self.in_flight_bytes += bytes as u64;
    }

    /// Close the current burst: the model stopped generating (tool-call
    /// boundary or end of stream). Pauses between bursts are excluded from
    /// decode time.
    pub fn note_boundary(&mut self) {
        if let Some(start) = self.burst_start.take() {
            self.elapsed += self
                .open_window_end(Instant::now())
                .saturating_duration_since(start);
        }
        self.last_output_at = None;
    }

    /// Instant up to which the open burst counts as decode time: the latest
    /// chunk's arrival plus at most [`OUTPUT_GAP`]. Past that the window is
    /// capped — silence after the last chunk is buffering, not decoding.
    fn open_window_end(&self, now: Instant) -> Instant {
        self.last_output_at
            .map_or(now, |last| (last + OUTPUT_GAP).min(now))
    }

    /// Fold an exact completion-token report into the measurement, dropping
    /// the in-flight estimate it supersedes.
    pub fn note_usage(&mut self, completion_tokens: Option<u32>) {
        let estimated = self.in_flight_bytes / BYTES_PER_TOKEN;
        self.exact_tokens += completion_tokens.map_or(estimated, u64::from);
        self.in_flight_bytes = 0;
    }

    /// Decoded tokens so far: exact counts plus the in-flight estimate.
    #[must_use]
    pub const fn tokens(&self) -> u64 {
        self.exact_tokens + self.in_flight_bytes / BYTES_PER_TOKEN
    }

    /// Current decode rate in tokens per second, or `None` before enough
    /// output has accumulated for a stable reading.
    #[must_use]
    pub fn rate(&self) -> Option<f64> {
        let tokens = self.tokens();
        let mut elapsed = self.elapsed;
        if let Some(start) = self.burst_start {
            elapsed += self
                .open_window_end(Instant::now())
                .saturating_duration_since(start);
        }
        let secs = elapsed.as_secs_f64();
        (tokens > 0 && secs >= MIN_MEASUREMENT.as_secs_f64()).then(|| {
            #[expect(clippy::cast_precision_loss)]
            let tokens = tokens as f64;
            tokens / secs
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_tokens_from_bytes() {
        let mut meter = DecodeMeter::default();
        meter.note_output(400);
        assert_eq!(meter.tokens(), 100);
    }

    #[test]
    fn usage_replaces_in_flight_estimate() {
        let mut meter = DecodeMeter::default();
        meter.note_output(400);
        meter.note_usage(Some(120));
        assert_eq!(meter.tokens(), 120);
        meter.note_output(40);
        assert_eq!(meter.tokens(), 130);
    }

    #[test]
    fn missing_usage_keeps_estimate() {
        let mut meter = DecodeMeter::default();
        meter.note_output(400);
        meter.note_usage(None);
        assert_eq!(meter.tokens(), 100);
    }

    #[test]
    fn rate_is_none_before_min_measurement() {
        let mut meter = DecodeMeter::default();
        meter.note_output(400);
        assert!(meter.rate().is_none());
    }

    #[test]
    fn rate_after_min_measurement() {
        let mut meter = DecodeMeter::default();
        meter.note_output(400);
        std::thread::sleep(MIN_MEASUREMENT + Duration::from_millis(100));
        let rate = meter.rate().expect("rate after min measurement");
        // 100 tokens over ~0.3s lands in the hundreds of tok/s.
        assert!(rate > 100.0, "rate {rate} implausibly low");
        assert!(rate < 10_000.0, "rate {rate} implausibly high");
    }

    #[test]
    fn silent_gap_does_not_inflate_decode_time() {
        let mut meter = DecodeMeter::default();
        meter.note_output(400);
        std::thread::sleep(MIN_MEASUREMENT + Duration::from_millis(100));
        // A long pause with no output: the model was buffering server-side,
        // not decoding at a crawling rate. The window caps at the last
        // chunk + OUTPUT_GAP, so the rate freezes instead of collapsing.
        std::thread::sleep(OUTPUT_GAP + Duration::from_secs(3));
        let frozen = meter.rate().expect("rate during gap");
        assert!(
            frozen > 30.0,
            "rate free-fell during silence: {frozen} tok/s"
        );
        // The next chunk closes the window at the same cap; the pause
        // beyond OUTPUT_GAP never enters decode time.
        meter.note_output(4);
        std::thread::sleep(MIN_MEASUREMENT);
        let resumed = meter.rate().expect("rate after resume");
        assert!(resumed > 30.0, "gap leaked into decode time: {resumed}");
    }

    #[test]
    fn empty_output_does_not_start_burst() {
        let mut meter = DecodeMeter::default();
        meter.note_output(0);
        meter.note_output(0);
        std::thread::sleep(MIN_MEASUREMENT + Duration::from_millis(50));
        meter.note_output(400);
        // The burst started at the first real byte, so elapsed is below the
        // measurement floor and no rate is reported.
        assert!(meter.rate().is_none());
    }

    #[test]
    fn boundary_excludes_pause_from_rate() {
        let mut meter = DecodeMeter::default();
        meter.note_output(400);
        std::thread::sleep(MIN_MEASUREMENT + Duration::from_millis(50));
        meter.note_boundary();
        let first = meter.rate().expect("rate after burst");
        std::thread::sleep(Duration::from_millis(500));
        let second = meter.rate().expect("rate after pause");
        // If the pause counted, the rate would roughly halve.
        assert!(
            second > first * 0.8,
            "pause leaked into decode time: {first} -> {second}"
        );
    }
}
