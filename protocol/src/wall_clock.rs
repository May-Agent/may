//! Interval arithmetic behind the `/usage` wall-clock split.
//!
//! A session's wall clock is divided into API time, tool time, time waiting
//! on the user and idle time. The three recorded kinds overlap (a tool call
//! runs while a request is still decoding its arguments, an `ask_user`
//! command is itself a tool call), so each instant of the wall clock is
//! attributed to the first kind that covers it, in the order API, user wait,
//! tools, and the remainder is idle. A tool call blocked on a question
//! therefore counts as waiting. The four parts therefore sum to the wall clock exactly.

/// A half-open span `[start_ms, end_ms)` of Unix milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    start_ms: i64,
    end_ms: i64,
}

impl Interval {
    /// The span from `start_ms` to `end_ms`, or `None` when it ends before it
    /// starts.
    #[must_use]
    pub const fn new(start_ms: i64, end_ms: i64) -> Option<Self> {
        if end_ms < start_ms {
            None
        } else {
            Some(Self { start_ms, end_ms })
        }
    }

    /// The span from `start_ms` to `end_ms`, empty when the end is earlier
    /// (a clock that stepped back).
    #[must_use]
    pub const fn clamped(start_ms: i64, end_ms: i64) -> Self {
        Self {
            start_ms,
            end_ms: if end_ms < start_ms { start_ms } else { end_ms },
        }
    }

    /// The span of `duration_ms` that ends at `end_ms`.
    #[must_use]
    pub const fn ending_at(end_ms: i64, duration_ms: u64) -> Self {
        let duration = duration_ms.cast_signed();
        Self {
            start_ms: end_ms.saturating_sub(duration),
            end_ms,
        }
    }

    /// Start, Unix milliseconds.
    #[must_use]
    pub const fn start_ms(self) -> i64 {
        self.start_ms
    }

    /// End, Unix milliseconds.
    #[must_use]
    pub const fn end_ms(self) -> i64 {
        self.end_ms
    }

    /// Length in milliseconds.
    #[must_use]
    pub const fn len_ms(self) -> u64 {
        self.end_ms.abs_diff(self.start_ms)
    }

    const fn is_empty(self) -> bool {
        self.start_ms == self.end_ms
    }
}

/// A set of instants kept as sorted, disjoint, non-empty intervals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntervalSet {
    spans: Vec<Interval>,
}

impl IntervalSet {
    /// The union of `intervals`; overlapping and touching spans merge.
    #[must_use]
    pub fn new(intervals: impl IntoIterator<Item = Interval>) -> Self {
        let mut spans: Vec<Interval> = intervals
            .into_iter()
            .filter(|interval| !interval.is_empty())
            .collect();
        spans.sort_by_key(|interval| interval.start_ms);
        let mut merged: Vec<Interval> = Vec::with_capacity(spans.len());
        for span in spans {
            match merged.last_mut() {
                Some(last) if span.start_ms <= last.end_ms => {
                    last.end_ms = last.end_ms.max(span.end_ms);
                }
                _ => merged.push(span),
            }
        }
        Self { spans: merged }
    }

    /// The instants in either set.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        Self::new(self.spans.iter().chain(&other.spans).copied())
    }

    /// The instants of `self` that `other` does not cover.
    #[must_use]
    pub fn subtract(&self, other: &Self) -> Self {
        let mut spans = Vec::new();
        let mut cuts = other.spans.iter().peekable();
        for span in &self.spans {
            let mut start = span.start_ms;
            // Cuts ending before this span cannot touch it or any later one.
            while cuts.next_if(|cut| cut.end_ms <= start).is_some() {}
            // A cut may reach into the next span too, so it is only peeked.
            let mut inner = cuts.clone();
            while let Some(cut) = inner.next_if(|cut| cut.start_ms < span.end_ms) {
                if cut.start_ms > start {
                    spans.push(Interval {
                        start_ms: start,
                        end_ms: cut.start_ms,
                    });
                }
                start = start.max(cut.end_ms);
                if start >= span.end_ms {
                    break;
                }
            }
            if start < span.end_ms {
                spans.push(Interval {
                    start_ms: start,
                    end_ms: span.end_ms,
                });
            }
        }
        Self { spans }
    }

    /// The instants of `self` inside `window`.
    #[must_use]
    pub fn clip(&self, window: Interval) -> Self {
        let spans = self
            .spans
            .iter()
            .filter_map(|span| {
                Interval::new(
                    span.start_ms.max(window.start_ms),
                    span.end_ms.min(window.end_ms),
                )
            })
            .filter(|span| !span.is_empty())
            .collect();
        Self { spans }
    }

    /// Total covered milliseconds.
    #[must_use]
    pub fn total_ms(&self) -> u64 {
        self.spans.iter().map(|span| span.len_ms()).sum()
    }
}

/// Where a session's wall clock went, in milliseconds. The four parts sum to
/// the wall clock.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    skyzen::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct WallClockSplit {
    /// A provider request was in flight.
    pub api_ms: u64,
    /// A tool call was running with no request in flight and no wait on the
    /// user.
    pub tools_ms: u64,
    /// The session waited on the user and no request was in flight.
    pub user_wait_ms: u64,
    /// None of the above.
    pub idle_ms: u64,
}

impl WallClockSplit {
    /// Attribute every instant of `wall` to API, then user wait, then tools,
    /// else idle. Intervals outside `wall` are ignored.
    #[must_use]
    pub fn attribute(
        wall: Interval,
        api: &IntervalSet,
        tools: &IntervalSet,
        user_wait: &IntervalSet,
    ) -> Self {
        let api = api.clip(wall);
        let user_wait = user_wait.clip(wall).subtract(&api);
        let tools = tools.clip(wall).subtract(&api.union(&user_wait));
        let api_ms = api.total_ms();
        let tools_ms = tools.total_ms();
        let user_wait_ms = user_wait.total_ms();
        Self {
            api_ms,
            tools_ms,
            user_wait_ms,
            idle_ms: wall.len_ms() - api_ms - tools_ms - user_wait_ms,
        }
    }

    /// The wall clock the parts add up to.
    #[must_use]
    pub const fn wall_ms(&self) -> u64 {
        self.api_ms + self.tools_ms + self.user_wait_ms + self.idle_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: i64, end: i64) -> Interval {
        Interval::new(start, end).expect("test spans run forward")
    }

    fn set<const N: usize>(spans: [(i64, i64); N]) -> IntervalSet {
        IntervalSet::new(spans.map(|(start, end)| span(start, end)))
    }

    fn attribute<const A: usize, const T: usize, const W: usize>(
        wall: Interval,
        api: [(i64, i64); A],
        tools: [(i64, i64); T],
        user_wait: [(i64, i64); W],
    ) -> WallClockSplit {
        WallClockSplit::attribute(wall, &set(api), &set(tools), &set(user_wait))
    }

    #[test]
    fn overlapping_and_touching_spans_merge() {
        let merged = set([(5, 10), (0, 6), (10, 12), (20, 25)]);
        assert_eq!(merged, set([(0, 12), (20, 25)]));
        assert_eq!(merged.total_ms(), 17);
    }

    #[test]
    fn subtract_splits_and_trims() {
        let base = set([(0, 100), (200, 300)]);
        let cuts = set([(10, 20), (90, 210), (250, 260)]);
        assert_eq!(
            base.subtract(&cuts),
            set([(0, 10), (20, 90), (210, 250), (260, 300)])
        );
    }

    #[test]
    fn a_request_overlapping_a_tool_call_counts_once_as_api() {
        // The request decodes the tool call's arguments for 20..40, then the
        // tool runs 30..80: the overlap 30..40 is API time.
        let split = attribute(span(0, 100), [(10, 40)], [(30, 80)], []);
        assert_eq!(
            split,
            WallClockSplit {
                api_ms: 30,
                tools_ms: 40,
                user_wait_ms: 0,
                idle_ms: 30,
            }
        );
    }

    #[test]
    fn a_tool_call_overlapping_a_user_wait_counts_once_as_waiting() {
        // The tool call runs 20..60 and the wait 40..90: the overlap 40..60
        // is waiting, the tool keeps only 20..40.
        let split = attribute(span(0, 100), [], [(20, 60)], [(40, 90)]);
        assert_eq!(
            split,
            WallClockSplit {
                api_ms: 0,
                tools_ms: 20,
                user_wait_ms: 50,
                idle_ms: 30,
            }
        );
    }

    #[test]
    fn a_tool_call_blocked_on_a_user_wait_counts_as_waiting() {
        // `ask_user` runs inside a `terminal` tool call: the question 30..50
        // lies inside the call 10..80, and only the rest is tool time.
        let split = attribute(span(0, 100), [], [(10, 80)], [(30, 50)]);
        assert_eq!(
            split,
            WallClockSplit {
                api_ms: 0,
                tools_ms: 50,
                user_wait_ms: 20,
                idle_ms: 30,
            }
        );
    }

    #[test]
    fn all_three_kinds_overlapping_still_sum_to_the_wall_clock() {
        let split = attribute(
            span(0, 1_300),
            [(0, 300), (250, 400), (900, 1_400)],
            [(100, 500), (450, 900)],
            [(350, 800), (950, 960)],
        );
        // API 0..400 and 900..1300; waiting 400..800; tools 800..900.
        assert_eq!(split.api_ms, 400 + 400);
        assert_eq!(split.user_wait_ms, 400);
        assert_eq!(split.tools_ms, 100);
        assert_eq!(split.idle_ms, 0);
        assert_eq!(split.wall_ms(), 1_300);
    }

    #[test]
    fn intervals_outside_the_wall_clock_are_ignored() {
        let split = attribute(span(100, 200), [(0, 150)], [(180, 500)], []);
        assert_eq!(split.api_ms, 50);
        assert_eq!(split.tools_ms, 20);
        assert_eq!(split.wall_ms(), 100);
    }
}
