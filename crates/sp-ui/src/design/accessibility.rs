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
    use crate::design::{ColorToken, Surface, TextColor};

    #[test]
    fn primary_text_is_accessible_on_the_canvas() {
        let foreground = ColorToken::Text(TextColor::Primary).color();
        let background = ColorToken::Surface(Surface::Canvas).color();
        assert!(meets_aa_normal_text(foreground, background));
        assert!(meets_aa_large_text(foreground, background));
    }
}
