//! Semantic design tokens and theme definitions.

/// Accessibility calculations for colors and text.
pub mod accessibility;
/// Embedded OFL-licensed application-UI font payloads.
pub mod fonts;
/// Semantic icon identifiers independent of a renderer.
pub mod icons;
/// Reduced-motion policy and transition durations.
pub mod motion;
/// Renderer-independent accessibility semantics and keyboard contracts.
pub mod semantics;
/// The application theme and its accessible control color pairings.
pub mod theme;
/// Foundational color, layout, typography, and status tokens.
pub mod tokens;
/// Named typography roles and font fallback stacks.
pub mod typography;

pub use accessibility::{meets_aa_large_text, meets_aa_normal_text};
pub use icons::IconId;
pub use motion::{MotionKind, MotionPreference, motion_allowed, transition_duration};
pub use semantics::{
    AccessibilityNode, AccessibilityRange, AccessibilityRole, AccessibilityState,
    AccessibleComponent, FocusOrder, KeyboardAction,
};
pub use theme::{ButtonKind, Control, ControlColors, DARK, Interaction, SemanticPairing, Theme};
pub use tokens::{
    Accent, BorderWidth, Color, ColorToken, Focus, Layout, Radius, Spacing, Status, Surface,
    TextColor,
};
pub use typography::{FontFamily, FontStack, FontWeight, Typography, TypographyRole};
