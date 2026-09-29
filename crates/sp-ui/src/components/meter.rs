//! Meter ballistics and the segmented LED meter.

use std::time::Duration;

/// UI repaint frequency for live meters. This is deliberately independent of audio callbacks.
pub const METER_REPAINT_HZ: u32 = 30;
/// Interval derived from [`METER_REPAINT_HZ`].
pub const METER_REPAINT_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000 / METER_REPAINT_HZ as u64);
/// Segments in one LED meter column.
pub const LED_SEGMENTS: usize = 16;
/// Topmost segments that light in the fault colour.
pub const LED_FAULT_SEGMENTS: usize = 2;

const METER_FLOOR_DBFS: f32 = -60.0;
const METER_DECAY_DB_PER_SECOND: f32 = 24.0;
const PEAK_HOLD: Duration = Duration::from_millis(250);
const CLIP_HOLD: Duration = Duration::from_millis(500);

/// UI-only ballistics for the latest stereo peak snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterBallistics {
    displayed_dbfs: [f32; 2],
    held_dbfs: f32,
    peak_hold_until: Duration,
    clip_hold_until: Duration,
    last_update: Option<Duration>,
}

impl Default for MeterBallistics {
    fn default() -> Self {
        Self {
            displayed_dbfs: [METER_FLOOR_DBFS; 2],
            held_dbfs: METER_FLOOR_DBFS,
            peak_hold_until: Duration::ZERO,
            clip_hold_until: Duration::ZERO,
            last_update: None,
        }
    }
}

impl MeterBallistics {
    /// Updates the visual reading using monotonically increasing UI elapsed time.
    pub fn update(&mut self, peak: [f32; 2], clipped: bool, elapsed: Duration) {
        let dt = self
            .last_update
            .map_or(0.0, |last| elapsed.saturating_sub(last).as_secs_f32());
        self.last_update = Some(elapsed);
        for (displayed, sample) in self.displayed_dbfs.iter_mut().zip(peak) {
            let sample_dbfs = if sample.is_finite() && sample > 0.0 {
                (20.0 * sample.log10()).max(METER_FLOOR_DBFS)
            } else {
                METER_FLOOR_DBFS
            };
            *displayed = sample_dbfs.max(*displayed - METER_DECAY_DB_PER_SECOND * dt);
        }
        let peak_dbfs = self.displayed_dbfs[0].max(self.displayed_dbfs[1]);
        if peak_dbfs >= self.held_dbfs {
            self.held_dbfs = peak_dbfs;
            self.peak_hold_until = elapsed.saturating_add(PEAK_HOLD);
        } else if elapsed >= self.peak_hold_until {
            self.held_dbfs = peak_dbfs.max(self.held_dbfs - METER_DECAY_DB_PER_SECOND * dt);
        }
        if clipped {
            self.clip_hold_until = elapsed.saturating_add(CLIP_HOLD);
        }
    }

    /// Returns the smoothed left and right peaks in dBFS.
    #[must_use]
    pub fn displayed_dbfs(self) -> [f32; 2] {
        self.displayed_dbfs
    }

    /// Returns the louder channel's fill fraction for a −60 to 0 dBFS track.
    #[must_use]
    pub fn fill_fraction(self) -> f32 {
        fraction(self.displayed_dbfs[0].max(self.displayed_dbfs[1]))
    }

    /// Returns one channel's fill fraction on the same track. Channel 1 is right.
    #[must_use]
    pub fn channel_fraction(self, channel: usize) -> f32 {
        fraction(self.displayed_dbfs[channel.min(1)])
    }

    /// Returns the recent held peak position on the same track.
    #[must_use]
    pub fn held_fraction(self) -> f32 {
        fraction(self.held_dbfs)
    }

    /// Reports clipping for a short time after an observed clipped block.
    #[must_use]
    pub fn clipping(self, elapsed: Duration) -> bool {
        self.clip_hold_until > elapsed
    }
}

fn fraction(dbfs: f32) -> f32 {
    ((dbfs - METER_FLOOR_DBFS) / -METER_FLOOR_DBFS).clamp(0.0, 1.0)
}

/// How one LED segment is drawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Segment {
    /// Unlit.
    Off,
    /// Lit in the text colour.
    Lit,
    /// Lit in the fault colour: one of the top [`LED_FAULT_SEGMENTS`].
    Hot,
}

/// Returns segment `index` (0 is the bottom) of a [`LED_SEGMENTS`]-tall meter showing `level`
/// with the peak hold at `held`, both as fill fractions.
#[must_use]
pub fn led_segment(level: f32, held: f32, index: usize) -> Segment {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "fractions are clamped to 0..=1 and the meter has 16 segments"
    )]
    let lit = |fraction: f32| (fraction.clamp(0.0, 1.0) * LED_SEGMENTS as f32).round() as usize;
    let held_index = lit(held).checked_sub(1);
    if index < lit(level) || Some(index) == held_index {
        if index >= LED_SEGMENTS - LED_FAULT_SEGMENTS {
            Segment::Hot
        } else {
            Segment::Lit
        }
    } else {
        Segment::Off
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{LED_SEGMENTS, MeterBallistics, Segment, led_segment};

    #[test]
    fn meter_ballistics_attack_decay_hold_and_clip() {
        let mut meter = MeterBallistics::default();
        meter.update([1.0, 0.5], true, Duration::ZERO);
        assert!(meter.displayed_dbfs()[0].abs() < f32::EPSILON);
        assert!(meter.clipping(Duration::from_millis(499)));

        meter.update([0.0; 2], false, Duration::from_millis(100));
        assert!((meter.displayed_dbfs()[0] + 2.4).abs() < 0.001);
        assert!((meter.held_fraction() - 1.0).abs() < f32::EPSILON);

        meter.update([0.0; 2], false, Duration::from_millis(300));
        assert!(meter.held_fraction() < 1.0);
        assert!(!meter.clipping(Duration::from_millis(500)));
    }

    #[test]
    fn led_meter_lights_from_the_bottom_with_a_hot_top_and_a_held_peak() {
        let lit = |level, held| {
            (0..LED_SEGMENTS)
                .map(|index| led_segment(level, held, index))
                .collect::<Vec<_>>()
        };
        assert!(lit(0.0, 0.0).iter().all(|segment| *segment == Segment::Off));
        let half = lit(0.5, 0.75);
        assert_eq!(half[7], Segment::Lit);
        assert_eq!(half[8], Segment::Off);
        assert_eq!(half[11], Segment::Lit, "the held peak stays lit");
        let full = lit(1.0, 1.0);
        assert_eq!(full[LED_SEGMENTS - 3], Segment::Lit);
        assert_eq!(full[LED_SEGMENTS - 2], Segment::Hot);
        assert_eq!(full[LED_SEGMENTS - 1], Segment::Hot);
    }
}
