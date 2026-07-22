//! Named typography roles and their renderer-independent font stacks.

/// A font family role with a defined fallback stack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontFamily {
    /// General interface text set in Sora.
    Interface,
    /// Display and heading text set in Sora.
    Display,
    /// Technical text set in Space Mono.
    Monospace,
}

/// An ordered font stack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FontStack {
    /// Preferred family name.
    pub primary: &'static str,
    /// Family names attempted after the preferred family.
    pub fallbacks: &'static [&'static str],
}

const SORA_FALLBACKS: &[&str] = &["Helvetica Neue", "Arial", "sans-serif"];
const SPACE_MONO_FALLBACKS: &[&str] = &["SFMono-Regular", "Menlo", "Monaco", "monospace"];

impl FontFamily {
    /// Returns this role's ordered font fallback stack.
    #[must_use]
    pub const fn stack(self) -> FontStack {
        match self {
            Self::Interface | Self::Display => FontStack {
                primary: "Sora",
                fallbacks: SORA_FALLBACKS,
            },
            Self::Monospace => FontStack {
                primary: "Space Mono",
                fallbacks: SPACE_MONO_FALLBACKS,
            },
        }
    }
}

/// A semantic font weight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum FontWeight {
    /// 400 weight.
    Regular = 400,
    /// 500 weight.
    Medium = 500,
    /// 600 weight.
    Semibold = 600,
    /// 700 weight.
    Bold = 700,
}

/// A reusable typography role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypographyRole {
    /// Product wordmark text.
    Brand,
    /// A rack or primary section title.
    Section,
    /// A card title.
    Card,
    /// Supporting descriptive text.
    Supporting,
    /// Component gallery title.
    Gallery,
    /// Compact explanatory text.
    Caption,
    /// The smallest metadata label.
    Meta,
    /// Prominent display text.
    Display,
    /// Section headings.
    Heading,
    /// Standard readable text.
    Body,
    /// Compact supporting text.
    BodySmall,
    /// Labels for controls and metadata.
    Label,
    /// Fixed-width technical content.
    Code,
}

/// A complete typography token, measured in logical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Typography {
    family: FontFamily,
    size: u8,
    line_height: u8,
    weight: FontWeight,
}

impl Typography {
    /// Returns the semantic font-family role.
    #[must_use]
    pub const fn family(self) -> FontFamily {
        self.family
    }
    /// Returns the font size in logical pixels.
    #[must_use]
    pub const fn size(self) -> u8 {
        self.size
    }
    /// Returns the line height in logical pixels.
    #[must_use]
    pub const fn line_height(self) -> u8 {
        self.line_height
    }
    /// Returns the semantic font weight.
    #[must_use]
    pub const fn weight(self) -> FontWeight {
        self.weight
    }
}

impl TypographyRole {
    /// Returns the typography token for this role.
    #[must_use]
    pub const fn typography(self) -> Typography {
        match self {
            Self::Brand => Typography {
                family: FontFamily::Display,
                size: 18,
                line_height: 24,
                weight: FontWeight::Bold,
            },
            Self::Section => Typography {
                family: FontFamily::Display,
                size: 22,
                line_height: 28,
                weight: FontWeight::Semibold,
            },
            Self::Card => Typography {
                family: FontFamily::Interface,
                size: 15,
                line_height: 20,
                weight: FontWeight::Semibold,
            },
            Self::Supporting => Typography {
                family: FontFamily::Interface,
                size: 13,
                line_height: 18,
                weight: FontWeight::Regular,
            },
            Self::Gallery => Typography {
                family: FontFamily::Display,
                size: 26,
                line_height: 32,
                weight: FontWeight::Bold,
            },
            Self::Caption => Typography {
                family: FontFamily::Interface,
                size: 11,
                line_height: 16,
                weight: FontWeight::Regular,
            },
            Self::Meta => Typography {
                family: FontFamily::Interface,
                size: 10,
                line_height: 14,
                weight: FontWeight::Medium,
            },
            Self::Display => Typography {
                family: FontFamily::Display,
                size: 32,
                line_height: 40,
                weight: FontWeight::Bold,
            },
            Self::Heading => Typography {
                family: FontFamily::Display,
                size: 24,
                line_height: 32,
                weight: FontWeight::Semibold,
            },
            Self::Body => Typography {
                family: FontFamily::Interface,
                size: 16,
                line_height: 24,
                weight: FontWeight::Regular,
            },
            Self::BodySmall => Typography {
                family: FontFamily::Interface,
                size: 12,
                line_height: 16,
                weight: FontWeight::Regular,
            },
            Self::Label => Typography {
                family: FontFamily::Interface,
                size: 12,
                line_height: 16,
                weight: FontWeight::Medium,
            },
            Self::Code => Typography {
                family: FontFamily::Monospace,
                size: 12,
                line_height: 16,
                weight: FontWeight::Regular,
            },
        }
    }
}
