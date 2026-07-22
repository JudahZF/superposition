//! Motion policy shared by renderer implementations.

use std::time::Duration;

/// User preference for UI motion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MotionPreference {
    /// Use the component's ordinary bounded transition.
    #[default]
    Full,
    /// Suppress decorative and state-transition animation.
    Reduced,
}

/// A kind of motion requested by a component.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionKind {
    /// A non-essential hover, selection, or layout transition.
    InterfaceTransition,
    /// A bounded operation such as scene recall progress.
    BoundedProgress,
    /// Live audio metering. Repaint cadence is bounded separately at 30 Hz.
    Meter,
}

/// Resolves whether a motion kind may animate for this preference.
#[must_use]
pub const fn motion_allowed(preference: MotionPreference, kind: MotionKind) -> bool {
    match (preference, kind) {
        (MotionPreference::Reduced, MotionKind::InterfaceTransition) => false,
        (MotionPreference::Reduced, MotionKind::BoundedProgress | MotionKind::Meter)
        | (MotionPreference::Full, _) => true,
    }
}

/// Returns a renderer transition duration. Reduced motion resolves to an immediate update.
#[must_use]
pub const fn transition_duration(preference: MotionPreference, kind: MotionKind) -> Duration {
    if motion_allowed(preference, kind) {
        match kind {
            MotionKind::InterfaceTransition => Duration::from_millis(120),
            MotionKind::BoundedProgress => Duration::from_millis(160),
            MotionKind::Meter => Duration::ZERO,
        }
    } else {
        Duration::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::{MotionKind, MotionPreference, motion_allowed, transition_duration};

    #[test]
    fn reduced_motion_keeps_only_meter_and_bounded_progress_live() {
        assert!(!motion_allowed(
            MotionPreference::Reduced,
            MotionKind::InterfaceTransition
        ));
        assert!(motion_allowed(MotionPreference::Reduced, MotionKind::Meter));
        assert_eq!(
            transition_duration(MotionPreference::Reduced, MotionKind::InterfaceTransition),
            std::time::Duration::ZERO
        );
    }
}
