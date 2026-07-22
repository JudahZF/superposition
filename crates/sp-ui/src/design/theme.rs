//! The dark alpha theme and centrally-defined accessible semantic pairings.

use super::tokens::{Accent, Color, ColorToken, Focus, Status, Surface, TextColor};

/// Minimum WCAG AA contrast ratio for normal-sized text.
pub const AA_NORMAL_TEXT_RATIO: f64 = 4.5;
/// Minimum WCAG AA contrast ratio for non-text component boundaries.
pub const AA_NON_TEXT_RATIO: f64 = 3.0;

/// The documented alpha dark application theme.
///
/// This theme deliberately retains documented system-font and text-mark fallbacks. Production
/// Sora/Space Mono files, logo SVGs, and an approved fault-red token are not available yet.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Theme;

/// The default alpha dark theme.
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

/// Every legacy foreground/background control pairing exposed by the theme.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Control {
    /// A button variant.
    Button(ButtonKind),
    /// A status treatment.
    Status(Status),
}

/// Explicit interaction states available to every relevant component renderer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Interaction {
    /// Resting, actionable state.
    #[default]
    Default,
    /// Pointer-hovered state.
    Hover,
    /// Pointer-pressed state.
    Pressed,
    /// Keyboard focus state. A 2 px cyan ring is required independent of selection.
    Focused,
    /// Unavailable state.
    Disabled,
    /// Persistently selected state.
    Selected,
    /// Work is in progress.
    Loading,
    /// A completed or healthy state.
    Success,
    /// A failed or faulted state. The alpha fallback is explicit text/icon, not red alone.
    Fault,
    /// Compatibility alias for [`Self::Default`].
    Inactive,
    /// Compatibility alias for [`Self::Hover`].
    Hovered,
    /// Compatibility alias for [`Self::Pressed`].
    Active,
}

/// A centrally-defined pairing consumed by renderers and contrast tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticPairing {
    /// Primary action text and fill.
    PrimaryAction,
    /// Secondary action text and fill.
    SecondaryAction,
    /// Quiet action text and fill.
    QuietAction,
    /// Default component treatment.
    Default,
    /// Hover treatment.
    Hover,
    /// Pressed treatment.
    Pressed,
    /// Focused treatment; its ring is obtained from [`Theme::focus_color`].
    Focused,
    /// Disabled component treatment.
    Disabled,
    /// Selected component treatment.
    Selected,
    /// Loading component treatment.
    Loading,
    /// Success or healthy status treatment.
    Success,
    /// Fault treatment. Meaning must also be conveyed through text and an icon.
    Fault,
    /// Informational status treatment.
    Info,
    /// Warning status treatment. Meaning must also be conveyed through text and an icon.
    Warning,
}

impl SemanticPairing {
    /// Exhaustive inventory of all normal-text foreground/background pairings that a
    /// Phase 7A component renderer may emit.
    pub const ALL: [Self; 14] = [
        Self::PrimaryAction,
        Self::SecondaryAction,
        Self::QuietAction,
        Self::Default,
        Self::Hover,
        Self::Pressed,
        Self::Focused,
        Self::Disabled,
        Self::Selected,
        Self::Loading,
        Self::Success,
        Self::Fault,
        Self::Info,
        Self::Warning,
    ];
}

impl Control {
    /// All legacy control pairings exposed by the theme.
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

/// Foreground and background tokens for one treatment.
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

    /// Returns a single centrally-defined semantic pairing.
    #[must_use]
    pub const fn pairing(self, pairing: SemanticPairing) -> ControlColors {
        match pairing {
            SemanticPairing::PrimaryAction => ControlColors {
                foreground: ColorToken::Text(TextColor::OnAccent),
                background: ColorToken::Accent(Accent::Cyan),
            },
            SemanticPairing::SecondaryAction
            | SemanticPairing::Pressed
            | SemanticPairing::Selected => ControlColors {
                foreground: ColorToken::Text(TextColor::OnAccent),
                background: ColorToken::Accent(Accent::Blue),
            },
            SemanticPairing::QuietAction => ControlColors {
                foreground: ColorToken::Text(TextColor::Primary),
                background: ColorToken::Surface(Surface::Panel),
            },
            SemanticPairing::Default | SemanticPairing::Focused | SemanticPairing::Loading => {
                ControlColors {
                    foreground: ColorToken::Text(TextColor::Primary),
                    background: ColorToken::Surface(Surface::Raised),
                }
            }
            SemanticPairing::Hover => ControlColors {
                foreground: ColorToken::Text(TextColor::Primary),
                background: ColorToken::Surface(Surface::Steel),
            },
            SemanticPairing::Disabled => ControlColors {
                foreground: ColorToken::Text(TextColor::Secondary),
                background: ColorToken::Surface(Surface::Steel),
            },
            SemanticPairing::Success => ControlColors {
                foreground: ColorToken::Accent(Accent::Lime),
                background: ColorToken::Surface(Surface::Raised),
            },
            SemanticPairing::Fault => ControlColors {
                foreground: ColorToken::Text(TextColor::Primary),
                background: ColorToken::Surface(Surface::Raised),
            },
            SemanticPairing::Info => ControlColors {
                foreground: ColorToken::Accent(Accent::Cyan),
                background: ColorToken::Surface(Surface::Raised),
            },
            SemanticPairing::Warning => ControlColors {
                foreground: ColorToken::Text(TextColor::Secondary),
                background: ColorToken::Surface(Surface::Raised),
            },
        }
    }

    /// Maps an interaction state to its renderer pairing.
    #[must_use]
    pub const fn interaction_pairing(self, interaction: Interaction) -> SemanticPairing {
        match interaction {
            Interaction::Default | Interaction::Inactive => SemanticPairing::Default,
            Interaction::Hover | Interaction::Hovered => SemanticPairing::Hover,
            Interaction::Pressed | Interaction::Active => SemanticPairing::Pressed,
            Interaction::Focused => SemanticPairing::Focused,
            Interaction::Disabled => SemanticPairing::Disabled,
            Interaction::Selected => SemanticPairing::Selected,
            Interaction::Loading => SemanticPairing::Loading,
            Interaction::Success => SemanticPairing::Success,
            Interaction::Fault => SemanticPairing::Fault,
        }
    }

    /// Returns colors for a widget interaction state.
    #[must_use]
    pub const fn interaction_colors(self, interaction: Interaction) -> ControlColors {
        self.pairing(self.interaction_pairing(interaction))
    }

    /// Returns the accessible colors for legacy exposed control treatments.
    #[must_use]
    pub const fn control_colors(self, control: Control) -> ControlColors {
        match control {
            Control::Button(ButtonKind::Primary) => self.pairing(SemanticPairing::PrimaryAction),
            Control::Button(ButtonKind::Secondary) => {
                self.pairing(SemanticPairing::SecondaryAction)
            }
            Control::Button(ButtonKind::Quiet) => self.pairing(SemanticPairing::QuietAction),
            Control::Status(Status::Info) => self.pairing(SemanticPairing::Info),
            Control::Status(Status::Success) => self.pairing(SemanticPairing::Success),
            Control::Status(Status::Warning) => self.pairing(SemanticPairing::Warning),
            Control::Status(Status::Error) => self.pairing(SemanticPairing::Fault),
        }
    }

    /// Returns the foreground token for semantic status text.
    #[must_use]
    pub const fn status_foreground(self, status: Status) -> ColorToken {
        self.control_colors(Control::Status(status))
            .foreground_token()
    }

    /// Returns the product brand foreground token.
    #[must_use]
    pub const fn brand_foreground(self) -> ColorToken {
        ColorToken::Accent(Accent::Cyan)
    }

    /// Returns the mandatory semantic focus-ring color.
    #[must_use]
    pub const fn focus_color(self) -> ColorToken {
        ColorToken::Focus(Focus::Ring)
    }

    /// Returns the boundary color for an active or selected blue fill.
    #[must_use]
    pub const fn selected_boundary_color(self) -> ColorToken {
        ColorToken::Text(TextColor::OnAccent)
    }
}

#[cfg(test)]
mod tests {
    use super::{AA_NON_TEXT_RATIO, Control, ControlColors, DARK, SemanticPairing};
    use crate::design::{Accent, ColorToken, Surface, TextColor};

    const SELECTED_BLUE_BOUNDARY: ControlColors = ControlColors {
        foreground: DARK.selected_boundary_color(),
        background: ColorToken::Accent(Accent::Blue),
    };

    #[test]
    fn every_central_semantic_pairing_meets_wcag_aa_for_normal_text() {
        for pairing in SemanticPairing::ALL {
            let colors = DARK.pairing(pairing);
            assert!(
                colors.meets_aa_normal_text(),
                "{pairing:?} contrast ratio was {}",
                colors.contrast_ratio()
            );
        }
    }

    #[test]
    fn legacy_control_api_is_covered_by_the_semantic_pairing_inventory() {
        for control in Control::ALL {
            assert!(DARK.control_colors(control).meets_aa_normal_text());
        }
    }

    #[test]
    fn selected_boundary_meets_non_text_contrast_requirement() {
        assert!(SELECTED_BLUE_BOUNDARY.contrast_ratio() >= AA_NON_TEXT_RATIO);
    }

    #[test]
    fn status_and_selected_pairings_do_not_depend_on_blue_or_lime_text_on_panel() {
        for pairing in SemanticPairing::ALL {
            let colors = DARK.pairing(pairing);
            assert!(
                !matches!(
                    (colors.foreground_token(), colors.background_token()),
                    (
                        ColorToken::Accent(Accent::Blue | Accent::Lime),
                        ColorToken::Surface(Surface::Panel | Surface::Raised)
                    )
                ) || pairing == SemanticPairing::Success,
                "{pairing:?} must retain its explicit accessible pairing"
            );
        }
        assert_eq!(
            DARK.pairing(SemanticPairing::Selected).foreground_token(),
            ColorToken::Text(TextColor::OnAccent)
        );
    }
}
