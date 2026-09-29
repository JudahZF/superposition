//! Accessibility helpers shared by UI component models.

use super::tokens::Color;

/// Minimum WCAG AA contrast ratio for large text.
pub const AA_LARGE_TEXT_RATIO: f64 = 3.0;

/// Returns whether two colors meet WCAG AA for normal-sized text.
#[must_use]
pub fn meets_aa_normal_text(foreground: Color, background: Color) -> bool {
    foreground.contrast_ratio(background) >= super::theme::AA_NORMAL_TEXT_RATIO
}

/// Returns whether two colors meet WCAG AA for large text.
#[must_use]
pub fn meets_aa_large_text(foreground: Color, background: Color) -> bool {
    foreground.contrast_ratio(background) >= AA_LARGE_TEXT_RATIO
}

#[cfg(test)]
mod tests {
    use super::{meets_aa_large_text, meets_aa_normal_text};
    use crate::design::ColorToken;

    #[test]
    fn faint_is_only_decorative_on_the_window() {
        let base = ColorToken::Base.color();
        assert!(meets_aa_normal_text(ColorToken::Dim.color(), base));
        assert!(!meets_aa_large_text(ColorToken::Faint.color(), base));
    }
}
