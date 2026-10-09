//! The foreground/background pairings the renderer draws, checked against WCAG AA.
//!
//! The renderer resolves colours straight from [`ColorToken`]; this module is the inventory of
//! which tokens meet on screen. Disabled text ([`ColorToken::Faint`]) is exempt from WCAG and is
//! not listed. Hovered buttons and list rows promote secondary text to [`ColorToken::Text`],
//! because [`ColorToken::Dim`] on the hover fill is below 4.5:1.

use super::tokens::ColorToken;

/// Minimum WCAG AA contrast ratio for normal-sized text.
pub const AA_NORMAL_TEXT_RATIO: f64 = 4.5;
/// Minimum WCAG AA contrast ratio for non-text component boundaries.
pub const AA_NON_TEXT_RATIO: f64 = 3.0;

/// A foreground drawn on a background.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pairing {
    /// Text or mark colour.
    pub foreground: ColorToken,
    /// Surface colour beneath it.
    pub background: ColorToken,
}

impl Pairing {
    const fn new(foreground: ColorToken, background: ColorToken) -> Self {
        Self {
            foreground,
            background,
        }
    }

    /// Calculates this pairing's WCAG contrast ratio.
    #[must_use]
    pub fn contrast_ratio(self) -> f64 {
        self.foreground
            .color()
            .contrast_ratio(self.background.color())
    }
}

/// Every text pairing the UI draws. Each must meet [`AA_NORMAL_TEXT_RATIO`].
///
/// Preview captions use an opaque [`ColorToken::Well`] bar, so plug-in pictures never reduce
/// caption contrast.
pub const TEXT_PAIRINGS: [Pairing; 20] = [
    // Window, rack column, and head and foot text.
    Pairing::new(ColorToken::Text, ColorToken::Base),
    Pairing::new(ColorToken::Strong, ColorToken::Base),
    Pairing::new(ColorToken::Dim, ColorToken::Base),
    Pairing::new(ColorToken::Warn, ColorToken::Base),
    Pairing::new(ColorToken::Fault, ColorToken::Base),
    Pairing::new(ColorToken::Info, ColorToken::Base),
    // Head, foot, selected rack column, popovers, and modals.
    Pairing::new(ColorToken::Text, ColorToken::Surface),
    Pairing::new(ColorToken::Strong, ColorToken::Surface),
    Pairing::new(ColorToken::Dim, ColorToken::Surface),
    // Preview captions and text fields.
    Pairing::new(ColorToken::Strong, ColorToken::Well),
    Pairing::new(ColorToken::Dim, ColorToken::Well),
    // Buttons and toggles.
    Pairing::new(ColorToken::Text, ColorToken::Raised),
    Pairing::new(ColorToken::Dim, ColorToken::Raised),
    // Hovered buttons, pads, and list rows.
    Pairing::new(ColorToken::Text, ColorToken::HoverFill),
    Pairing::new(ColorToken::Strong, ColorToken::HoverFill),
    // Lit Mute and lit Bypass.
    Pairing::new(ColorToken::Base, ColorToken::Warn),
    Pairing::new(ColorToken::Base, ColorToken::Info),
    // Active scene pad and text selection.
    Pairing::new(ColorToken::Strong, ColorToken::AccentFill),
    // Fault line.
    Pairing::new(ColorToken::Strong, ColorToken::FaultTint),
    Pairing::new(ColorToken::Fault, ColorToken::FaultTint),
];

/// Every non-text boundary the UI relies on. Each must meet [`AA_NON_TEXT_RATIO`].
///
/// Outline borders are decorative and not listed.
pub const BOUNDARY_PAIRINGS: [Pairing; 7] = [
    // Selection rings and the selected rack bar, against the window, column, and slot fills.
    Pairing::new(ColorToken::Accent, ColorToken::Base),
    Pairing::new(ColorToken::Accent, ColorToken::Surface),
    Pairing::new(ColorToken::Accent, ColorToken::Well),
    // Gain marker on the gain track.
    Pairing::new(ColorToken::Strong, ColorToken::Groove),
    // Meter bar zones against the unlit well.
    Pairing::new(ColorToken::MeterLit, ColorToken::Well),
    Pairing::new(ColorToken::Fault, ColorToken::Well),
    Pairing::new(ColorToken::Warn, ColorToken::Well),
];

#[cfg(test)]
mod tests {
    use super::{AA_NON_TEXT_RATIO, AA_NORMAL_TEXT_RATIO, BOUNDARY_PAIRINGS, TEXT_PAIRINGS};

    #[test]
    fn every_text_pairing_meets_wcag_aa_for_normal_text() {
        for pairing in TEXT_PAIRINGS {
            assert!(
                pairing.contrast_ratio() >= AA_NORMAL_TEXT_RATIO,
                "{pairing:?} contrast ratio was {}",
                pairing.contrast_ratio()
            );
        }
    }

    #[test]
    fn every_boundary_pairing_meets_wcag_aa_for_non_text() {
        for pairing in BOUNDARY_PAIRINGS {
            assert!(
                pairing.contrast_ratio() >= AA_NON_TEXT_RATIO,
                "{pairing:?} contrast ratio was {}",
                pairing.contrast_ratio()
            );
        }
    }
}
