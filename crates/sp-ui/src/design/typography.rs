//! Typography: Inter for interface text on a 16 px line grid. IBM Plex Mono stays only as the
//! monospace family for diagnostics and logs.

/// The application UI face with its fallback stack.
pub const FONT_STACK: FontStack = FontStack {
    primary: "Inter",
    fallbacks: &["SF Pro Text", "Helvetica Neue", "sans-serif"],
};

/// An ordered font stack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FontStack {
    /// Preferred family name.
    pub primary: &'static str,
    /// Family names attempted after the preferred family.
    pub fallbacks: &'static [&'static str],
}

/// A font weight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum FontWeight {
    /// 400: body, captions, labels, and buttons.
    Regular = 400,
    /// 500: head readout values.
    Medium = 500,
    /// 600: titles, state tokens, and the wordmark.
    SemiBold = 600,
}

/// A typography role. Labels are sentence case; only state tokens are uppercase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypographyRole {
    /// 12 px regular: default text, buttons, menu rows, and scene names.
    Body,
    /// 11 px regular: preview captions, route line, gain label, and status detail.
    Caption,
    /// 10 px regular: readout labels, tick labels, meter labels, and capture age.
    Label,
    /// 13 px semibold: rack names, popover and modal titles, and setup section titles.
    Title,
    /// 13 px medium: head readout values.
    Value,
    /// 10 px semibold: state tokens and sidechain markers.
    Token,
    /// 15 px semibold: the wordmark.
    Wordmark,
}

/// A complete typography token, measured in logical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Typography {
    /// Font size.
    pub size: u8,
    /// Line height.
    pub line_height: u8,
    /// Weight.
    pub weight: FontWeight,
}

impl TypographyRole {
    /// Returns this role's size, line height, and weight.
    #[must_use]
    pub const fn typography(self) -> Typography {
        match self {
            Self::Body => Typography {
                size: 12,
                line_height: 16,
                weight: FontWeight::Regular,
            },
            Self::Caption => Typography {
                size: 11,
                line_height: 14,
                weight: FontWeight::Regular,
            },
            Self::Label => Typography {
                size: 10,
                line_height: 12,
                weight: FontWeight::Regular,
            },
            Self::Title => Typography {
                size: 13,
                line_height: 16,
                weight: FontWeight::SemiBold,
            },
            Self::Value => Typography {
                size: 13,
                line_height: 16,
                weight: FontWeight::Medium,
            },
            Self::Token => Typography {
                size: 10,
                line_height: 12,
                weight: FontWeight::SemiBold,
            },
            Self::Wordmark => Typography {
                size: 15,
                line_height: 20,
                weight: FontWeight::SemiBold,
            },
        }
    }
}
