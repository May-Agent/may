//! Whether a stream's frame source runs: only while the stream has viewers.

/// A change to a source's running state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateChange {
    /// The first viewer arrived: the source starts. The transport has asked
    /// the encoder for a keyframe, which the first frame after the start
    /// carries.
    Start,
    /// The last viewer left: the source stops.
    Stop,
}

/// The running state of a stream's source (the CDP screencast, the display
/// capture). The viewer count is the caller's peers map and is passed in on
/// every call; the gate keeps only whether the source is up, so the two
/// cannot disagree about how many viewers there are.
#[derive(Debug, Default)]
pub struct SourceGate {
    running: bool,
}

impl SourceGate {
    /// A gate over a source that is stopped.
    #[must_use]
    pub const fn stopped() -> Self {
        Self { running: false }
    }

    /// What the source must do to match `viewers`, if anything.
    #[must_use]
    pub const fn required(&self, viewers: usize) -> Option<GateChange> {
        match (self.running, viewers > 0) {
            (false, true) => Some(GateChange::Start),
            (true, false) => Some(GateChange::Stop),
            _ => None,
        }
    }

    /// Records that `change` was carried out on the source.
    pub const fn applied(&mut self, change: GateChange) {
        self.running = matches!(change, GateChange::Start);
    }
}

#[cfg(test)]
mod tests {
    use super::{GateChange, SourceGate};

    #[test]
    fn stays_stopped_without_viewers() {
        assert_eq!(SourceGate::stopped().required(0), None);
    }

    #[test]
    fn first_viewer_starts_and_more_change_nothing() {
        let mut gate = SourceGate::stopped();
        assert_eq!(gate.required(1), Some(GateChange::Start));
        gate.applied(GateChange::Start);
        assert_eq!(gate.required(1), None);
        assert_eq!(gate.required(3), None);
    }

    #[test]
    fn last_viewer_stops_and_the_next_restarts() {
        let mut gate = SourceGate::stopped();
        gate.applied(GateChange::Start);
        assert_eq!(gate.required(2), None);
        assert_eq!(gate.required(0), Some(GateChange::Stop));
        gate.applied(GateChange::Stop);
        assert_eq!(gate.required(0), None);
        assert_eq!(gate.required(1), Some(GateChange::Start));
    }

    #[test]
    fn unapplied_change_stays_required() {
        let gate = SourceGate::stopped();
        assert_eq!(gate.required(1), Some(GateChange::Start));
        assert_eq!(gate.required(1), Some(GateChange::Start));
    }
}
