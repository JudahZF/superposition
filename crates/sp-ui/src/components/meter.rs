//! Meter ballistics and the bar meter.

use std::time::Duration;

/// UI repaint frequency for live meters. This is deliberately independent of audio callbacks.
pub const METER_REPAINT_HZ: u32 = 30;
/// Interval derived from [`METER_REPAINT_HZ`].
pub const METER_REPAINT_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000 / METER_REPAINT_HZ as u64);
/// Level where a bar meter turns from its lit colour to the warn colour.
pub const METER_WARN_DBFS: f32 = -6.0;
/// Level where a bar meter turns to the fault colour.
pub const METER_FAULT_DBFS: f32 = -3.0;
/// Scale tick levels on the bar meter, loudest first. Labels sit at least 12 dB apart so they
/// do not overlap on the 92 px meter; the warn zone colour marks −6 dBFS.
pub const METER_SCALE_DBFS: [f32; 4] = [0.0, -12.0, -24.0, -48.0];

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

/// Maps a dBFS level onto the −60 to 0 dBFS track: 0.0 at −60 dBFS and 1.0 at 0 dBFS.
#[must_use]
pub fn dbfs_fraction(dbfs: f32) -> f32 {
    ((dbfs - METER_FLOOR_DBFS) / -METER_FLOOR_DBFS).clamp(0.0, 1.0)
}

fn fraction(dbfs: f32) -> f32 {
    dbfs_fraction(dbfs)
}

/// The colour zone of a bar meter at one level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeterZone {
    /// At or below −6 dBFS.
    Lit,
    /// Above −6 dBFS, up to and including −3 dBFS.
    Warn,
    /// Above −3 dBFS.
    Fault,
}

/// Returns the zone for a fill fraction from [`dbfs_fraction`].
#[must_use]
pub fn meter_zone(fraction: f32) -> MeterZone {
    if fraction > dbfs_fraction(METER_FAULT_DBFS) {
        MeterZone::Fault
    } else if fraction > dbfs_fraction(METER_WARN_DBFS) {
        MeterZone::Warn
    } else {
        MeterZone::Lit
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{METER_SCALE_DBFS, MeterBallistics, MeterZone, dbfs_fraction, meter_zone};

    #[test]
    fn scale_ticks_map_onto_the_meter_track() {
        let expected = [1.0, 0.8, 0.6, 0.2];
        for (dbfs, expected) in METER_SCALE_DBFS.into_iter().zip(expected) {
            assert!(
                (dbfs_fraction(dbfs) - expected).abs() < 0.001,
                "{dbfs} dBFS maps to {}",
                dbfs_fraction(dbfs)
            );
        }
    }

    #[test]
    fn meter_zones_change_at_the_warn_and_fault_levels() {
        assert_eq!(meter_zone(dbfs_fraction(-60.0)), MeterZone::Lit);
        assert_eq!(meter_zone(dbfs_fraction(-6.0)), MeterZone::Lit);
        assert_eq!(meter_zone(dbfs_fraction(-5.9)), MeterZone::Warn);
        assert_eq!(meter_zone(dbfs_fraction(-3.0)), MeterZone::Warn);
        assert_eq!(meter_zone(dbfs_fraction(-2.9)), MeterZone::Fault);
        assert_eq!(meter_zone(1.0), MeterZone::Fault);
    }

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
}
