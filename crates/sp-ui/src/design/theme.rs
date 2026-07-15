//! The Phase 0 dark theme and its accessible control colors.

use super::tokens::{Accent, Color, ColorToken, Status, Surface, TextColor};

/// Minimum WCAG AA contrast ratio for normal-sized text.
pub const AA_NORMAL_TEXT_RATIO: f64 = 4.5;

/// The Phase 0 dark application theme.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Theme;

/// The default Phase 0 dark theme.
pub const DARK: Theme = Theme;

/// A button's semantic appearance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonKind {
    /// The highest-emphasis call to action.
    Primary,
    /// A standard secondary action.
    Secondary,
    /// A low-emphasis action on a panel.
    Quiet,
}

/// Every foreground/background control pairing exposed by the Phase 0 theme.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Control {
    /// A button variant.
    Button(ButtonKind),
    /// A status treatment.
    Status(Status),
}

impl Control {
    /// All control pairings exposed by the Phase 0 theme.
    pub const ALL: [Self; 7] = [
        Self::Button(ButtonKind::Primary),
        Self::Button(ButtonKind::Secondary),
        Self::Button(ButtonKind::Quiet),
        Self::Status(Status::Info),
        Self::Status(Status::Success),
        Self::Status(Status::Warning),
        Self::Status(Status::Error),
    ];
}

/// Foreground and background tokens for one control treatment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlColors {
    foreground: ColorToken,
    background: ColorToken,
}

impl ControlColors {
    /// Returns the semantic foreground token.
    #[must_use]
    pub const fn foreground_token(self) -> ColorToken {
        self.foreground
    }

    /// Returns the semantic background token.
    #[must_use]
    pub const fn background_token(self) -> ColorToken {
        self.background
    }

    /// Resolves the foreground color.
    #[must_use]
    pub const fn foreground_color(self) -> Color {
        self.foreground.color()
    }

    /// Resolves the background color.
    #[must_use]
    pub const fn background_color(self) -> Color {
        self.background.color()
    }

    /// Calculates this pairing's WCAG contrast ratio.
    #[must_use]
    pub fn contrast_ratio(self) -> f64 {
        self.foreground_color()
            .contrast_ratio(self.background_color())
    }

    /// Returns whether this pairing meets WCAG AA for normal-sized text.
    #[must_use]
    pub fn meets_aa_normal_text(self) -> bool {
        self.contrast_ratio() >= AA_NORMAL_TEXT_RATIO
    }
}

impl Theme {
    /// Resolves a semantic color token in this theme.
    #[must_use]
    pub const fn color(self, token: ColorToken) -> Color {
        token.color()
    }

    /// Returns the accessible colors for an exposed control treatment.
    ///
    /// Warning and error treatments intentionally use neutral high-contrast surfaces:
    /// Phase 0 has no dedicated warning or error brand colors. Their status meaning
    /// must therefore be conveyed by text and an icon in addition to this treatment.
    #[must_use]
    pub const fn control_colors(self, control: Control) -> ControlColors {
        match control {
            Control::Button(ButtonKind::Primary) => ControlColors {
                foreground: ColorToken::Text(TextColor::OnAccent),
                background: ColorToken::Accent(Accent::Cyan),
            },
            Control::Button(ButtonKind::Secondary) | Control::Status(Status::Info) => {
                ControlColors {
                    foreground: ColorToken::Text(TextColor::OnAccent),
                    background: ColorToken::Accent(Accent::Blue),
                }
            }
            Control::Button(ButtonKind::Quiet) => ControlColors {
                foreground: ColorToken::Text(TextColor::Primary),
                background: ColorToken::Surface(Surface::Panel),
            },
            Control::Status(Status::Success) => ControlColors {
                foreground: ColorToken::Text(TextColor::OnAccent),
                background: ColorToken::Accent(Accent::Lime),
            },
            Control::Status(Status::Warning) => ControlColors {
                foreground: ColorToken::Text(TextColor::Primary),
                background: ColorToken::Surface(Surface::Steel),
            },
            Control::Status(Status::Error) => ControlColors {
                foreground: ColorToken::Text(TextColor::Primary),
                background: ColorToken::Surface(Surface::Raised),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Control, DARK};

    #[test]
    fn all_exposed_control_pairings_meet_wcag_aa_for_normal_text() {
        for control in Control::ALL {
            let colors = DARK.control_colors(control);
            assert!(
                colors.meets_aa_normal_text(),
                "{control:?} contrast ratio was {}",
                colors.contrast_ratio()
            );
        }
    }

    #[test]
    fn contrast_calculation_is_order_independent() {
        for control in Control::ALL {
            let colors = DARK.control_colors(control);
            let forward = colors
                .foreground_color()
                .contrast_ratio(colors.background_color());
            let reverse = colors
                .background_color()
                .contrast_ratio(colors.foreground_color());
            assert!((forward - reverse).abs() < f64::EPSILON);
        }
    }
}
