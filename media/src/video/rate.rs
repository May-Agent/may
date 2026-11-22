//! The encoder's target bitrate, from every viewer's feedback.
//!
//! A viewer's REMB estimate leads the way down: it falls when the viewer
//! measures queueing delay. It cannot lead the way back up: after congestion
//! a viewer's estimate sits near what it was last sent for tens of seconds,
//! and then rises a few percent a second. While the viewer's receiver
//! reports show almost no loss, the rate therefore climbs above the estimate
//! on its own, back toward the highest estimate that viewer gave. Loss backs
//! the rate off as libwebrtc's loss-based estimate does, and while loss is
//! recent the climb is as gentle as libwebrtc's; once the link has been calm
//! for a few seconds it doubles each second.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use mediax_codec::Bitrate;

/// A new target is applied once it differs from the applied one by this
/// fraction. The software AV1 encoder restarts its rate control (and emits a
/// keyframe) on every change.
const BITRATE_STEP: f64 = 0.1;

/// Loss is judged over at least this many expected packets, libwebrtc's
/// minimum, or over a second's worth when fewer arrive: receivers send
/// reports far more often under loss, and a report covering a handful of
/// packets says nothing.
const MIN_EXPECTED_PACKETS: u32 = 20;

/// Loss below which the rate climbs: 2%, the threshold libwebrtc's
/// loss-based estimate increases under.
const LOW_LOSS: f64 = 0.02;

/// Loss above which the rate backs off by half of it: 10%, libwebrtc's
/// decrease threshold.
const HIGH_LOSS: f64 = 0.1;

/// An estimate below this fraction of the previous one is the viewer
/// reacting to delay, not noise between reports.
const ESTIMATE_DROP: f64 = 0.95;

/// The longest gap between loss judgements that one climb step spans, so a
/// late report cannot jump the rate.
const MAX_CLIMB_SPAN: Duration = Duration::from_secs(1);

/// The least a viewer is sent: libwebrtc's default minimum video bitrate.
/// Below it the encoder drops frames until too few packets reach the viewer
/// to judge loss by, and the climb back cannot start.
const MIN_BITRATE: u32 = 30_000;

/// Loss lowers the rate at most once in this span, libwebrtc's decrease
/// interval: reports arrive far faster under loss than the rate change they
/// ask for reaches the viewer.
const DECREASE_INTERVAL: Duration = Duration::from_millis(300);

/// Growth per second while loss is recent: libwebrtc's loss-based increase.
/// A link that stays narrow is probed this gently around its capacity.
const CAUTIOUS_CLIMB: f64 = 1.08;

/// How long without loss before the rate doubles again: long enough that a
/// narrow link's next loss arrives first, short enough that a cleared one
/// is back to its rate within seconds.
const CALM: Duration = Duration::from_secs(3);

/// A viewer's cumulative reception counters, as one RTCP reception report
/// states them.
#[derive(Debug, Clone, Copy)]
pub(super) struct Reception {
    /// The extended highest sequence number received.
    pub highest_sequence: u32,
    /// Packets lost since the stream began.
    pub total_lost: u32,
}

/// Packets expected and lost since loss was last judged.
#[derive(Debug, Default)]
struct LossWindow {
    last: Option<Reception>,
    expected: u32,
    lost: u32,
    judged_at: Option<Instant>,
}

impl LossWindow {
    /// Folds in a report; once the window spans enough packets, the loss
    /// fraction over it and the time since the previous judgement.
    fn report(&mut self, reception: Reception, at: Instant) -> Option<(f64, Duration)> {
        let Some(last) = self.last.replace(reception) else {
            // The first report only sets where counting starts.
            self.judged_at = Some(at);
            return None;
        };
        self.expected = self.expected.saturating_add(
            reception
                .highest_sequence
                .wrapping_sub(last.highest_sequence),
        );
        // Duplicates can lower the cumulative count; they recover nothing.
        self.lost = self
            .lost
            .saturating_add(reception.total_lost.saturating_sub(last.total_lost));
        // At the lowest rates a second passes before that many packets do;
        // the second's packets are judged then, or the climb waits on them.
        let second_passed = self
            .judged_at
            .is_some_and(|previous| at.saturating_duration_since(previous) >= MAX_CLIMB_SPAN);
        if self.expected < MIN_EXPECTED_PACKETS && !(second_passed && self.expected > 0) {
            return None;
        }
        let fraction = f64::from(self.lost.min(self.expected)) / f64::from(self.expected);
        let span = self
            .judged_at
            .replace(at)
            .map_or(Duration::ZERO, |previous| {
                at.saturating_duration_since(previous)
            })
            .min(MAX_CLIMB_SPAN);
        self.expected = 0;
        self.lost = 0;
        Some((fraction, span))
    }
}

/// One viewer's view of the stream's rate.
#[derive(Debug)]
struct PeerRate {
    /// The latest REMB estimate.
    estimate: u32,
    /// The highest estimate this viewer gave: what a climb returns to.
    ceiling: u32,
    /// The rate this viewer is sent at.
    rate: u32,
    /// `rate` is above `estimate` because it climbed there.
    climbing: bool,
    /// The last loss judgement was above [`HIGH_LOSS`].
    lossy: bool,
    /// When loss was last judged above [`HIGH_LOSS`].
    lossy_at: Option<Instant>,
    /// When loss last lowered the rate.
    decreased_at: Option<Instant>,
    loss: LossWindow,
}

impl PeerRate {
    fn new(estimate: u32) -> Self {
        Self {
            estimate,
            ceiling: estimate,
            rate: estimate.max(MIN_BITRATE),
            climbing: false,
            lossy: false,
            lossy_at: None,
            decreased_at: None,
            loss: LossWindow::default(),
        }
    }

    /// How much the rate may grow per second: cautiously while loss is
    /// recent, doubling once the link has been calm.
    fn climb_per_second(&self, at: Instant) -> f64 {
        if self
            .lossy_at
            .is_some_and(|lossy| at.saturating_duration_since(lossy) < CALM)
        {
            CAUTIOUS_CLIMB
        } else {
            2.0
        }
    }

    fn estimate(&mut self, estimate: u32) {
        let dropped = f64::from(estimate) < f64::from(self.estimate) * ESTIMATE_DROP;
        self.estimate = estimate;
        if self.climbing {
            // The estimate trails what the climb sends; loss judges it.
            if self.rate <= estimate {
                self.climbing = false;
            }
        } else if dropped {
            self.rate = self.rate.min(estimate).max(MIN_BITRATE);
        } else if !self.lossy {
            self.rate = self.rate.max(estimate);
        }
        if !self.lossy {
            self.ceiling = self.ceiling.max(estimate);
        }
    }

    fn reception(&mut self, reception: Reception, at: Instant) {
        let Some((fraction, span)) = self.loss.report(reception, at) else {
            return;
        };
        self.lossy = fraction > HIGH_LOSS;
        if self.lossy {
            self.lossy_at = Some(at);
            if self
                .decreased_at
                .is_some_and(|last| at.saturating_duration_since(last) < DECREASE_INTERVAL)
            {
                return;
            }
            self.decreased_at = Some(at);
            self.climbing = false;
            self.rate = scale(self.rate, 1.0 - fraction / 2.0);
        } else if fraction < LOW_LOSS {
            let growth = self.climb_per_second(at).powf(span.as_secs_f64());
            let climbed = scale(self.rate, growth).min(self.ceiling);
            if climbed > self.rate {
                self.rate = climbed;
                self.climbing = self.rate > self.estimate;
            }
        }
    }
}

/// `bits` scaled by `factor`, kept between [`MIN_BITRATE`] and the `u32`
/// range.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the product is clamped to the u32 range first"
)]
fn scale(bits: u32, factor: f64) -> u32 {
    (f64::from(bits) * factor).clamp(f64::from(MIN_BITRATE), f64::from(u32::MAX)) as u32
}

/// Folds every viewer's rate into the encoder's one target: the lowest,
/// since every viewer receives the same stream.
#[derive(Debug)]
pub(super) struct BitrateTarget {
    peers: HashMap<String, PeerRate>,
    applied: u32,
}

impl BitrateTarget {
    pub(super) fn new(applied: Bitrate) -> Self {
        Self {
            peers: HashMap::new(),
            applied: applied.bits_per_second().get(),
        }
    }

    /// A viewer's REMB estimate.
    pub(super) fn estimate(&mut self, peer: String, bits_per_second: u32) -> Option<Bitrate> {
        self.peers
            .entry(peer)
            .and_modify(|rate| rate.estimate(bits_per_second))
            .or_insert_with(|| PeerRate::new(bits_per_second));
        self.retarget()
    }

    /// A viewer's RTCP reception report. A viewer that has sent no estimate
    /// yet does not limit the rate, so its report has nothing to move.
    pub(super) fn reception(
        &mut self,
        peer: &str,
        reception: Reception,
        at: Instant,
    ) -> Option<Bitrate> {
        self.peers.get_mut(peer)?.reception(reception, at);
        self.retarget()
    }

    pub(super) fn peer_gone(&mut self, peer: &str) -> Option<Bitrate> {
        self.peers.remove(peer);
        self.retarget()
    }

    /// The new target when it moved far enough from the applied one.
    fn retarget(&mut self) -> Option<Bitrate> {
        let lowest = self.peers.values().map(|peer| peer.rate).min()?;
        let bits = NonZeroU32::new(lowest)?;
        let moved = f64::from(bits.get().abs_diff(self.applied));
        if moved < f64::from(self.applied) * BITRATE_STEP {
            return None;
        }
        self.applied = bits.get();
        Some(Bitrate::new(bits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(bits: u32) -> BitrateTarget {
        BitrateTarget::new(Bitrate::new(NonZeroU32::new(bits).unwrap()))
    }

    #[allow(
        clippy::single_option_map,
        reason = "the assertions read as `applied(change)`"
    )]
    fn applied(bitrate: Option<Bitrate>) -> Option<u32> {
        bitrate.map(|b| b.bits_per_second().get())
    }

    /// Peer `a`'s receiver reports.
    struct Reports {
        now: Instant,
        reception: Reception,
    }

    impl Reports {
        /// Sends the first report, which only starts the count.
        fn start(target: &mut BitrateTarget) -> Self {
            let mut reports = Self {
                now: Instant::now(),
                reception: Reception {
                    highest_sequence: 1_000,
                    total_lost: 0,
                },
            };
            assert_eq!(reports.after(target, Duration::ZERO, 0, 0), None);
            reports
        }

        /// A report `after` the previous one: `expected` more packets, `lost`
        /// of them lost.
        fn after(
            &mut self,
            target: &mut BitrateTarget,
            after: Duration,
            expected: u32,
            lost: u32,
        ) -> Option<u32> {
            self.reception.highest_sequence += expected;
            self.reception.total_lost += lost;
            self.now += after;
            applied(target.reception("a", self.reception, self.now))
        }

        /// The next second's report.
        fn send(&mut self, target: &mut BitrateTarget, expected: u32, lost: u32) -> Option<u32> {
            self.after(target, Duration::from_secs(1), expected, lost)
        }

        /// A second of 100 packets with `lost` lost.
        fn second(&mut self, target: &mut BitrateTarget, lost: u32) -> Option<u32> {
            self.send(target, 100, lost)
        }
    }

    /// Peer `a` carried 6 Mbps, then its estimate collapsed to 300 kbps.
    fn collapsed() -> (BitrateTarget, Reports) {
        let mut target = target(2_000_000);
        assert_eq!(
            applied(target.estimate("a".into(), 6_000_000)),
            Some(6_000_000)
        );
        let reports = Reports::start(&mut target);
        assert_eq!(applied(target.estimate("a".into(), 300_000)), Some(300_000));
        (target, reports)
    }

    #[test]
    fn the_rate_follows_the_lowest_peer_estimate() {
        let mut target = target(2_000_000);
        // Within the step: not applied.
        assert!(target.estimate("a".into(), 2_100_000).is_none());
        assert_eq!(applied(target.estimate("b".into(), 800_000)), Some(800_000));
        // The lower peer leaves: the other's estimate takes over.
        assert_eq!(applied(target.peer_gone("b")), Some(2_100_000));
    }

    #[test]
    fn without_loss_the_rate_climbs_back_to_what_the_peer_carried() {
        let (mut target, mut reports) = collapsed();
        // The estimate stays down; the rate doubles each second regardless...
        for expected in [600_000, 1_200_000, 2_400_000, 4_800_000] {
            assert_eq!(reports.second(&mut target, 0), Some(expected));
        }
        // ...up to the 6 Mbps the peer carried, and no further.
        assert_eq!(reports.second(&mut target, 0), Some(6_000_000));
        assert_eq!(reports.second(&mut target, 0), None);
    }

    #[test]
    fn after_loss_the_climb_is_gentle_until_the_link_is_calm() {
        let (mut target, mut reports) = collapsed();
        reports.second(&mut target, 0);
        assert_eq!(reports.second(&mut target, 0), Some(1_200_000));
        // 30% loss at 1.2 Mbps: back off by half the loss.
        assert_eq!(reports.second(&mut target, 30), Some(1_020_000));
        // 8% a second while the loss is recent (the first step is within
        // the applied step)...
        assert_eq!(reports.second(&mut target, 0), None);
        assert_eq!(reports.second(&mut target, 0), Some(1_189_728));
        // ...and doubling again three seconds after it.
        assert_eq!(reports.second(&mut target, 0), Some(2_379_456));
    }

    #[test]
    fn an_estimate_does_not_undo_a_loss_backoff_while_loss_lasts() {
        let mut target = target(6_000_000);
        target.estimate("a".into(), 6_000_000);
        let mut reports = Reports::start(&mut target);
        // 40% loss at the estimate itself.
        assert_eq!(reports.second(&mut target, 40), Some(4_800_000));
        assert_eq!(applied(target.estimate("a".into(), 6_000_000)), None);
        // The loss clears: the next estimate takes the rate back up.
        assert_eq!(reports.second(&mut target, 0), None);
        assert_eq!(
            applied(target.estimate("a".into(), 6_000_000)),
            Some(6_000_000)
        );
    }

    #[test]
    fn a_handful_of_packets_judges_no_loss() {
        let mut target = target(6_000_000);
        target.estimate("a".into(), 6_000_000);
        let mut reports = Reports::start(&mut target);
        let ms = Duration::from_millis;
        // Reports a few packets apart, as a receiver sends them under loss.
        assert_eq!(reports.after(&mut target, ms(50), 10, 10), None);
        assert_eq!(reports.after(&mut target, ms(50), 5, 0), None);
        // 25 packets, 10 lost (40%): now it counts.
        assert_eq!(reports.after(&mut target, ms(50), 10, 0), Some(4_800_000));
    }

    #[test]
    fn at_the_lowest_rates_a_second_of_packets_is_judged() {
        let (mut target, mut reports) = collapsed();
        // A few packets a second, none lost: the climb does not wait for 20.
        assert_eq!(reports.send(&mut target, 3, 0), Some(600_000));
        // A second with nothing received judges nothing.
        assert_eq!(reports.send(&mut target, 0, 0), None);
    }

    #[test]
    fn loss_lowers_the_rate_once_per_decrease_interval() {
        let mut target = target(6_000_000);
        target.estimate("a".into(), 6_000_000);
        let mut reports = Reports::start(&mut target);
        let ms = Duration::from_millis;
        // 40% loss judged every 100 ms: one cut, then nothing until 300 ms on.
        assert_eq!(reports.after(&mut target, ms(100), 20, 8), Some(4_800_000));
        assert_eq!(reports.after(&mut target, ms(100), 20, 8), None);
        assert_eq!(reports.after(&mut target, ms(100), 20, 8), None);
        assert_eq!(reports.after(&mut target, ms(100), 20, 8), Some(3_840_000));
    }

    #[test]
    fn loss_never_takes_the_rate_below_the_minimum() {
        let mut target = target(6_000_000);
        target.estimate("a".into(), 6_000_000);
        let mut reports = Reports::start(&mut target);
        let mut last = 6_000_000;
        for _ in 0..40 {
            if let Some(bits) = reports.second(&mut target, 95) {
                last = bits;
            }
        }
        assert_eq!(last, MIN_BITRATE);
        // An estimate collapsing below it is held there too.
        assert_eq!(applied(target.estimate("a".into(), 1_000)), None);
    }

    #[test]
    fn a_rising_estimate_raises_the_ceiling() {
        let (mut target, mut reports) = collapsed();
        assert_eq!(
            applied(target.estimate("a".into(), 9_000_000)),
            Some(9_000_000)
        );
        assert_eq!(
            applied(target.estimate("a".into(), 1_000_000)),
            Some(1_000_000)
        );
        for expected in [2_000_000, 4_000_000, 8_000_000, 9_000_000] {
            assert_eq!(reports.second(&mut target, 0), Some(expected));
        }
    }
}
