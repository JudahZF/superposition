//! The foreground/background pairings the renderer draws, checked against WCAG AA.
//!
//! The renderer resolves colours straight from [`ColorToken`]; this module is the inventory of
//! which tokens meet on screen. Faint text is reserved for disabled and decorative content (empty
//! slots, tick labels), which WCAG exempts. Hovered rows promote secondary text to
//! [`ColorToken::Text`], because dim on the hover fill is below 4.5:1.

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
/// Preview captions use an opaque surface bar, so plug-in pictures never reduce caption contrast.
pub const TEXT_PAIRINGS: [Pairing; 19] = [
    // Primary, secondary, and state text on the window.
    Pairing::new(ColorToken::Text, ColorToken::Base),
    Pairing::new(ColorToken::Dim, ColorToken::Base),
    Pairing::new(ColorToken::Warn, ColorToken::Base),
    Pairing::new(ColorToken::Fault, ColorToken::Base),
    Pairing::new(ColorToken::Info, ColorToken::Base),
    // The same on cards, rails, popovers, and preview captions.
    Pairing::new(ColorToken::Text, ColorToken::Surface),
    Pairing::new(ColorToken::Dim, ColorToken::Surface),
    Pairing::new(ColorToken::Warn, ColorToken::Surface),
    Pairing::new(ColorToken::Fault, ColorToken::Surface),
    Pairing::new(ColorToken::Info, ColorToken::Surface),
    // Route and count chips.
    Pairing::new(ColorToken::Text, ColorToken::SegmentOff),
    // Active tab, active scene, primary buttons, and the selected menu row.
    Pairing::new(ColorToken::AccentText, ColorToken::Accent),
    // Lit Mute and Bypass toggles, warn chips, and sidechain chips.
    Pairing::new(ColorToken::Warn, ColorToken::WarnTint),
    Pairing::new(ColorToken::Info, ColorToken::InfoTint),
    // Fault banner.
    Pairing::new(ColorToken::Text, ColorToken::FaultTint),
    Pairing::new(ColorToken::Fault, ColorToken::FaultTint),
    // Hovered rows and buttons.
    Pairing::new(ColorToken::Text, ColorToken::HoverFill),
    Pairing::new(ColorToken::Fault, ColorToken::HoverFill),
    Pairing::new(ColorToken::Warn, ColorToken::HoverFill),
];

/// Every non-text boundary the UI relies on. Each must meet [`AA_NON_TEXT_RATIO`].
pub const BOUNDARY_PAIRINGS: [Pairing; 8] = [
    // Selection rings and the focus outline on the window and on cards.
    Pairing::new(ColorToken::Accent, ColorToken::Base),
    Pairing::new(ColorToken::Accent, ColorToken::Surface),
    // Hovered card and popover borders.
    Pairing::new(ColorToken::Dim, ColorToken::Surface),
    // Lit and clipping meter segments against unlit ones.
    Pairing::new(ColorToken::Text, ColorToken::SegmentOff),
    Pairing::new(ColorToken::Fault, ColorToken::SegmentOff),
    // Status dots on cards.
    Pairing::new(ColorToken::Success, ColorToken::Surface),
    // Fader thumbs on their track.
    Pairing::new(ColorToken::Text, ColorToken::Surface),
    // Lit toggle borders.
    Pairing::new(ColorToken::Info, ColorToken::Surface),
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
