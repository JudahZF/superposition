//! Typography: one monospace face on a 16 px line grid.

/// The application face with its fallback stack.
pub const FONT_STACK: FontStack = FontStack {
    primary: "IBM Plex Mono",
    fallbacks: &["SFMono-Regular", "Menlo", "monospace"],
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
    /// 400: everything except the roles below.
    Regular = 400,
    /// 500: the wordmark, rack names, and popover titles.
    Medium = 500,
}

/// A typography role. Labels are sentence case; only state tokens are uppercase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypographyRole {
    /// 12 px text on the 16 px line.
    Body,
    /// 12 px medium: rack names and popover titles.
    Title,
    /// 10 px: readout labels, captions, and tick labels.
    Label,
    /// 13 px medium: the wordmark.
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
            Self::Title => Typography {
                size: 12,
                line_height: 16,
                weight: FontWeight::Medium,
            },
            Self::Label => Typography {
                size: 10,
                line_height: 14,
                weight: FontWeight::Regular,
            },
            Self::Wordmark => Typography {
                size: 13,
                line_height: 16,
                weight: FontWeight::Medium,
            },
        }
    }
}
