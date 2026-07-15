//! A model for an audio level meter.

use std::fmt;

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
    /// Creates a level and derives its severity from the peak value.
    #[must_use]
    pub fn from_peak_dbfs(peak_dbfs: f32) -> Self {
        Self {
            peak_dbfs,
            state: Self::state_for(peak_dbfs),
        }
    }
    /// Updates the peak reading and derived state.
    pub fn set_peak_dbfs(&mut self, peak_dbfs: f32) {
        self.peak_dbfs = peak_dbfs;
        self.state = Self::state_for(peak_dbfs);
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

#[cfg(test)]
mod tests {
    use super::{MeterLevel, MeterState};
    #[test]
    fn meter_state_tracks_peak_level() {
        let mut meter = MeterLevel::from_peak_dbfs(-12.0);
        meter.set_peak_dbfs(0.0);
        assert_eq!(meter.state.to_string(), "Clipping");
        assert_eq!(meter.state, MeterState::Clipping);
    }
}
