//! Models for compact editable controls used by the live rack and gallery.

/// Rack gain state, expressed in decibels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainFaderState {
    /// Current gain in the supported `-120..=24 dB` range.
    pub gain_db: f32,
    /// Defined reset value.
    pub default_db: f32,
    /// Whether the rack output is muted.
    pub muted: bool,
}

impl GainFaderState {
    /// Sets a finite gain, clamped to the product range.
    pub fn set_gain_db(&mut self, gain_db: f32) {
        if gain_db.is_finite() {
            self.gain_db = gain_db.clamp(-120.0, 24.0);
        }
    }

    /// Restores the defined default gain.
    pub fn reset(&mut self) {
        self.set_gain_db(self.default_db);
    }
}

/// Generic VST3 parameter presentation state.
#[derive(Clone, Debug, PartialEq)]
pub struct ParameterControlState {
    /// Stable host-facing parameter identifier.
    pub id: u32,
    /// User-visible parameter name.
    pub name: String,
    /// Current normalized value.
    pub normalized: f32,
    /// Optional reset value.
    pub default: Option<f32>,
    /// Formatted value supplied by the plug-in.
    pub formatted: String,
    /// Whether the control is display-only.
    pub read_only: bool,
    /// Whether the control uses discrete steps.
    pub discrete: bool,
}

impl ParameterControlState {
    /// Updates an editable finite value and clamps it to `0..=1`.
    pub fn set_normalized(&mut self, normalized: f32) {
        if !self.read_only && normalized.is_finite() {
            self.normalized = normalized.clamp(0.0, 1.0);
        }
    }

    /// Restores the defined default when the parameter is editable.
    pub fn reset(&mut self) {
        if let Some(default) = self.default {
            self.set_normalized(default);
        }
    }
}

/// A named set of mutually exclusive compact options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentedControlState {
    /// Accessible group label.
    pub label: String,
    /// Ordered option labels.
    pub options: Vec<String>,
    /// Selected option index.
    pub selected: usize,
}

impl SegmentedControlState {
    /// Selects an existing option and ignores out-of-range requests.
    pub fn select(&mut self, index: usize) {
        if index < self.options.len() {
            self.selected = index;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GainFaderState, ParameterControlState, SegmentedControlState};

    #[test]
    fn controls_clamp_select_and_reset_defined_values() {
        let mut gain = GainFaderState {
            gain_db: -6.0,
            default_db: 0.0,
            muted: false,
        };
        gain.set_gain_db(99.0);
        assert!((gain.gain_db - 24.0).abs() < f32::EPSILON);
        gain.reset();
        assert!(gain.gain_db.abs() < f32::EPSILON);

        let mut parameter = ParameterControlState {
            id: 1,
            name: "Threshold".to_owned(),
            normalized: 0.2,
            default: Some(0.5),
            formatted: "-18.0 dB".to_owned(),
            read_only: false,
            discrete: false,
        };
        parameter.reset();
        assert!((parameter.normalized - 0.5).abs() < f32::EPSILON);

        let mut segmented = SegmentedControlState {
            label: "Channels".to_owned(),
            options: vec!["Mono".to_owned(), "Stereo".to_owned()],
            selected: 0,
        };
        segmented.select(1);
        assert_eq!(segmented.selected, 1);
    }
}
