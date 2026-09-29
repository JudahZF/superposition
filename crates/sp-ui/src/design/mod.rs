//! Design tokens, typography, and accessibility rules.

/// Accessibility calculations for colors and text.
pub mod accessibility;
/// Embedded OFL-licensed application-UI font payloads.
pub mod fonts;
/// Reduced-motion policy.
pub mod motion;
/// The foreground/background pairings the renderer draws, with contrast tests.
pub mod theme;
/// Chrome colours, spacing, and fixed layout dimensions.
pub mod tokens;
/// Typography roles on the 16 px line grid.
pub mod typography;

pub use accessibility::{meets_aa_large_text, meets_aa_normal_text};
pub use motion::{MotionKind, MotionPreference, motion_allowed};
pub use theme::{
    AA_NON_TEXT_RATIO, AA_NORMAL_TEXT_RATIO, BOUNDARY_PAIRINGS, Pairing, TEXT_PAIRINGS,
};
pub use tokens::{Color, ColorToken, Layout, Spacing};
pub use typography::{FONT_STACK, FontStack, FontWeight, Typography, TypographyRole};
