//! Audio meter models with a UI-owned, deterministic 30 Hz repaint cadence.

use std::{fmt, time::Duration};

use crate::design::{
    AccessibilityNode, AccessibilityRange, AccessibilityRole, AccessibilityState, FocusOrder,
    MotionKind, MotionPreference,
};

/// UI repaint frequency for live meters. This is deliberately independent of audio callbacks.
pub const METER_REPAINT_HZ: u32 = 30;
/// Interval derived from [`METER_REPAINT_HZ`].
pub const METER_REPAINT_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000 / METER_REPAINT_HZ as u64);

/// The severity band of a [`MeterLevel`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeterState {
    /// Normal signal level.
    Nominal,
    /// Signal is approaching clipping.
    Warning,
    /// Signal has clipped.
    Clipping,
}

impl MeterState {
    /// Returns the short UI label for this state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Nominal => "Nominal",
            Self::Warning => "Warning",
            Self::Clipping => "Clipping",
        }
    }
}

impl fmt::Display for MeterState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Audio meter reading and its derived severity state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterLevel {
    /// Peak level in decibels relative to full scale.
    pub peak_dbfs: f32,
    /// Derived severity state.
    pub state: MeterState,
}

impl MeterLevel {
    /// Creates a finite level and derives its severity from the peak value.
    #[must_use]
    pub fn from_peak_dbfs(peak_dbfs: f32) -> Self {
        let peak_dbfs = sanitize_peak(peak_dbfs);
        Self {
            peak_dbfs,
            state: Self::state_for(peak_dbfs),
        }
    }

    /// Updates a peak reading and derived state. Non-finite samples become the meter floor.
    pub fn set_peak_dbfs(&mut self, peak_dbfs: f32) {
        self.peak_dbfs = sanitize_peak(peak_dbfs);
        self.state = Self::state_for(self.peak_dbfs);
    }

    /// Returns the clamped `0.0..=1.0` fill fraction for a `-120..=0 dBFS` meter.
    #[must_use]
    pub fn fill_fraction(self) -> f32 {
        ((self.peak_dbfs + 120.0) / 120.0).clamp(0.0, 1.0)
    }

    fn state_for(peak_dbfs: f32) -> MeterState {
        if peak_dbfs >= 0.0 {
            MeterState::Clipping
        } else if peak_dbfs >= -6.0 {
            MeterState::Warning
        } else {
            MeterState::Nominal
        }
    }
}

fn sanitize_peak(peak_dbfs: f32) -> f32 {
    if peak_dbfs.is_finite() {
        peak_dbfs.clamp(-120.0, 0.0)
    } else {
        -120.0
    }
}

/// UI-owned cadence controller for a live meter.
///
/// Call [`Self::repaint_due`] with monotonically increasing UI elapsed time. The audio engine
/// publishes values whenever it needs to; it never calls this scheduler or controls UI repaint.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MeterRepaintScheduler {
    next_repaint: Option<Duration>,
}

impl MeterRepaintScheduler {
    /// Returns whether the UI should repaint now, then schedules the next 30 Hz repaint.
    pub fn repaint_due(&mut self, elapsed: Duration) -> bool {
        let Some(next_repaint) = self.next_repaint else {
            self.next_repaint = Some(elapsed.saturating_add(METER_REPAINT_INTERVAL));
            return true;
        };
        if elapsed < next_repaint {
            return false;
        }

        let elapsed_intervals = elapsed
            .saturating_sub(next_repaint)
            .as_nanos()
            .saturating_div(METER_REPAINT_INTERVAL.as_nanos());
        let skipped = u32::try_from(elapsed_intervals).unwrap_or(u32::MAX);
        self.next_repaint = Some(
            next_repaint
                .saturating_add(METER_REPAINT_INTERVAL.saturating_mul(skipped.saturating_add(1))),
        );
        true
    }

    /// Returns the amount of time until the next repaint, if the meter has been scheduled.
    #[must_use]
    pub fn until_next_repaint(&self, elapsed: Duration) -> Option<Duration> {
        self.next_repaint.map(|next| next.saturating_sub(elapsed))
    }

    /// Clears the schedule, causing the next call to [`Self::repaint_due`] to repaint immediately.
    pub fn reset(&mut self) {
        self.next_repaint = None;
    }
}

/// Renderer-facing stereo meter model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoMeter {
    /// Left-channel peak.
    pub left: MeterLevel,
    /// Right-channel peak.
    pub right: MeterLevel,
    /// UI-owned repaint policy.
    pub repaint: MeterRepaintScheduler,
}

impl crate::design::AccessibleComponent for MeterLevel {
    fn accessibility(&self, _focus_order: FocusOrder) -> AccessibilityNode {
        let mut node = AccessibilityNode::named(AccessibilityRole::ProgressBar, "Audio level");
        node.value = Some(format!(
            "{:.1} dBFS, {}",
            self.peak_dbfs,
            self.state.label()
        ));
        node.range = Some(AccessibilityRange {
            minimum: -120.0,
            maximum: 0.0,
            step: 0.1,
        });
        node.state = AccessibilityState {
            fault: self.state == MeterState::Clipping,
            ..AccessibilityState::default()
        };
        node
    }
}

impl StereoMeter {
    /// Creates a silent stereo meter.
    #[must_use]
    pub fn silent() -> Self {
        Self {
            left: MeterLevel::from_peak_dbfs(-120.0),
            right: MeterLevel::from_peak_dbfs(-120.0),
            repaint: MeterRepaintScheduler::default(),
        }
    }

    /// Accepts the latest audio-engine values without scheduling a repaint.
    pub fn set_peaks_dbfs(&mut self, left: f32, right: f32) {
        self.left.set_peak_dbfs(left);
        self.right.set_peak_dbfs(right);
    }

    /// Returns UI repaint eligibility. Meters remain live in reduced-motion mode.
    pub fn repaint_due(&mut self, elapsed: Duration, preference: MotionPreference) -> bool {
        crate::design::motion_allowed(preference, MotionKind::Meter)
            && self.repaint.repaint_due(elapsed)
    }

    /// Returns accessibility semantics for both channels.
    #[must_use]
    pub fn accessibility(&self, focus_order: FocusOrder) -> AccessibilityNode {
        let peak = self.left.peak_dbfs.max(self.right.peak_dbfs);
        let mut node =
            AccessibilityNode::named(AccessibilityRole::ProgressBar, "Stereo output level");
        node.value = Some(format!(
            "{peak:.1} dBFS, {}",
            MeterLevel::state_for(peak).label()
        ));
        node.range = Some(AccessibilityRange {
            minimum: -120.0,
            maximum: 0.0,
            step: 0.1,
        });
        node.state = AccessibilityState {
            fault: peak >= 0.0,
            ..AccessibilityState::default()
        };
        node.focus_order = Some(focus_order);
        node
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{METER_REPAINT_INTERVAL, MeterLevel, MeterRepaintScheduler, MeterState};

    #[test]
    fn meter_state_tracks_peak_and_rejects_non_finite_input() {
        let mut meter = MeterLevel::from_peak_dbfs(-12.0);
        meter.set_peak_dbfs(0.0);
        assert_eq!(meter.state, MeterState::Clipping);
        meter.set_peak_dbfs(f32::NAN);
        assert!((meter.peak_dbfs - -120.0).abs() < f32::EPSILON);
    }

    #[test]
    fn repaint_schedule_is_independent_and_capped_at_thirty_hz() {
        let mut scheduler = MeterRepaintScheduler::default();
        assert!(scheduler.repaint_due(Duration::ZERO));
        assert!(!scheduler.repaint_due(METER_REPAINT_INTERVAL / 2));
        assert!(scheduler.repaint_due(METER_REPAINT_INTERVAL));
        assert!(!scheduler.repaint_due(METER_REPAINT_INTERVAL + Duration::from_nanos(1)));
    }
}
