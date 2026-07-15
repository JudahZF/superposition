//! Semantic design tokens and theme definitions.

/// Accessibility calculations for colors and text.
pub mod accessibility;
/// Semantic icon identifiers independent of a renderer.
pub mod icons;
/// The application theme and its accessible control color pairings.
pub mod theme;
/// Foundational color, layout, typography, and status tokens.
pub mod tokens;
/// Named typography roles and font fallback stacks.
pub mod typography;

pub use accessibility::{meets_aa_large_text, meets_aa_normal_text};
pub use icons::IconId;
pub use theme::{ButtonKind, Control, ControlColors, DARK, Theme};
pub use tokens::{
    Accent, BorderWidth, Color, ColorToken, Radius, Spacing, Status, Surface, TextColor,
};
pub use typography::{FontFamily, FontStack, FontWeight, Typography, TypographyRole};
