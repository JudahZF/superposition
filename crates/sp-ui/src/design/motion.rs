//! Motion policy shared by renderer implementations.
//!
//! Only meters animate. Reduced motion holds them still.

/// User preference for UI motion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MotionPreference {
    /// Meters animate.
    #[default]
    Full,
    /// The system asks for reduced motion.
    Reduced,
}

/// A kind of motion requested by a component.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionKind {
    /// A hover, selection, or layout transition. The film-strip UI has none.
    InterfaceTransition,
    /// Live audio metering, repainted at [`crate::components::METER_REPAINT_INTERVAL`].
    Meter,
}

/// Resolves whether a motion kind may animate for this preference.
#[must_use]
pub const fn motion_allowed(preference: MotionPreference, kind: MotionKind) -> bool {
    matches!(
        (preference, kind),
        (MotionPreference::Full, MotionKind::Meter)
    )
}

#[cfg(test)]
mod tests {
    use super::{MotionKind, MotionPreference, motion_allowed};

    #[test]
    fn only_meters_animate_and_reduced_motion_holds_them_still() {
        assert!(motion_allowed(MotionPreference::Full, MotionKind::Meter));
        assert!(!motion_allowed(
            MotionPreference::Reduced,
            MotionKind::Meter
        ));
        assert!(!motion_allowed(
            MotionPreference::Full,
            MotionKind::InterfaceTransition
        ));
    }
}
