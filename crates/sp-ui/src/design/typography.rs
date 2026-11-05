//! Typography: Inter for interface text and IBM Plex Mono for numbers, routes, and key hints.

/// The interface face with its fallback stack.
pub const SANS_STACK: FontStack = FontStack {
    primary: "Inter",
    fallbacks: &["system-ui", "sans-serif"],
};

/// The numeric face with its fallback stack.
pub const MONO_STACK: FontStack = FontStack {
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

/// Which face a role uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontFace {
    /// Inter: labels, names, and actions.
    Sans,
    /// IBM Plex Mono: gains, channels, timings, counts, and key hints.
    Mono,
}

/// A font weight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum FontWeight {
    /// 400: everything except the roles below.
    Regular = 400,
    /// 600: the wordmark, rack names, pane and popover titles.
    SemiBold = 600,
}

/// A typography role. Labels are sentence case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypographyRole {
    /// 12 px interface text on the 16 px line.
    Body,
    /// 12 px semibold: plug-in names and emphasized text.
    Strong,
    /// 13 px semibold: rack names, pane and popover titles.
    Title,
    /// 11 px: secondary labels and captions.
    Label,
    /// 14 px semibold: the wordmark.
    Wordmark,
    /// 11 px mono: values, routes, and key hints.
    Mono,
    /// 10 px mono: meter labels and small counts.
    MonoSmall,
}

/// A complete typography token, measured in logical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Typography {
    /// Font face.
    pub face: FontFace,
    /// Font size.
    pub size: u8,
    /// Line height.
    pub line_height: u8,
    /// Weight.
    pub weight: FontWeight,
}

impl TypographyRole {
    /// Returns this role's face, size, line height, and weight.
    #[must_use]
    pub const fn typography(self) -> Typography {
        let (face, size, line_height, weight) = match self {
            Self::Body => (FontFace::Sans, 12, 16, FontWeight::Regular),
            Self::Strong => (FontFace::Sans, 12, 16, FontWeight::SemiBold),
            Self::Title => (FontFace::Sans, 13, 16, FontWeight::SemiBold),
            Self::Label => (FontFace::Sans, 11, 14, FontWeight::Regular),
            Self::Wordmark => (FontFace::Sans, 14, 16, FontWeight::SemiBold),
            Self::Mono => (FontFace::Mono, 11, 16, FontWeight::Regular),
            Self::MonoSmall => (FontFace::Mono, 10, 14, FontWeight::Regular),
        };
        Typography {
            face,
            size,
            line_height,
            weight,
        }
    }
}
