//! Foundational design tokens.
//!
//! Brand color values are intentionally defined only in this module. Consumers use
//! semantic token types rather than copying color values into components.

/// An opaque sRGB color.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color {
    red: u8,
    green: u8,
    blue: u8,
}

impl Color {
    const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    /// Returns the red, green, and blue channels for renderer integration.
    #[must_use]
    pub const fn channels(self) -> [u8; 3] {
        [self.red, self.green, self.blue]
    }

    /// Calculates relative luminance using the WCAG sRGB transfer function.
    #[must_use]
    pub fn relative_luminance(self) -> f64 {
        const RED_LUMINANCE: f64 = 0.212_6;
        const GREEN_LUMINANCE: f64 = 0.715_2;
        const BLUE_LUMINANCE: f64 = 0.072_2;

        RED_LUMINANCE * linear_srgb(self.red)
            + GREEN_LUMINANCE * linear_srgb(self.green)
            + BLUE_LUMINANCE * linear_srgb(self.blue)
    }

    /// Calculates the WCAG contrast ratio against another opaque color.
    ///
    /// The returned ratio is in the inclusive range `1.0..=21.0`.
    #[must_use]
    pub fn contrast_ratio(self, other: Self) -> f64 {
        const LUMINANCE_OFFSET: f64 = 0.05;

        let first = self.relative_luminance();
        let second = other.relative_luminance();
        let (lighter, darker) = if first >= second {
            (first, second)
        } else {
            (second, first)
        };

        (lighter + LUMINANCE_OFFSET) / (darker + LUMINANCE_OFFSET)
    }
}

fn linear_srgb(channel: u8) -> f64 {
    const CHANNEL_DIVISOR: f64 = 255.0;
    const LINEAR_THRESHOLD: f64 = 0.040_45;
    const LINEAR_DIVISOR: f64 = 12.92;
    const GAMMA_OFFSET: f64 = 0.055;
    const GAMMA_DIVISOR: f64 = 1.055;
    const GAMMA_EXPONENT: f64 = 2.4;

    let srgb = f64::from(channel) / CHANNEL_DIVISOR;
    if srgb <= LINEAR_THRESHOLD {
        srgb / LINEAR_DIVISOR
    } else {
        ((srgb + GAMMA_OFFSET) / GAMMA_DIVISOR).powf(GAMMA_EXPONENT)
    }
}

const CANVAS: Color = Color::rgb(0x0A, 0x0F, 0x16);
const PANEL: Color = Color::rgb(0x12, 0x18, 0x20);
const RAISED: Color = Color::rgb(0x1B, 0x23, 0x30);
const STEEL: Color = Color::rgb(0x2A, 0x32, 0x42);
const PRIMARY_TEXT: Color = Color::rgb(0xE6, 0xE8, 0xEC);
const SECONDARY_TEXT: Color = Color::rgb(0x9A, 0xA3, 0xAE);
const CYAN: Color = Color::rgb(0x00, 0xE5, 0xFF);
const BLUE: Color = Color::rgb(0x00, 0x7A, 0xFF);
const LIME: Color = Color::rgb(0xA6, 0xFF, 0x00);

/// A semantic background surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Surface {
    /// The application canvas.
    Canvas,
    /// A standard panel.
    Panel,
    /// An elevated panel or popover.
    Raised,
    /// A strong structural surface such as a secondary control.
    Steel,
}

/// A semantic text color.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextColor {
    /// Default high-emphasis text.
    Primary,
    /// Supporting text with lower emphasis.
    Secondary,
    /// Text placed on an accent fill.
    OnAccent,
}

/// A semantic accent color.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Accent {
    /// The active and informational accent.
    Cyan,
    /// The focus and selection accent.
    Blue,
    /// The positive accent.
    Lime,
}

/// A color token resolved by the active theme.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorToken {
    /// A semantic surface.
    Surface(Surface),
    /// A semantic text color.
    Text(TextColor),
    /// A semantic accent color.
    Accent(Accent),
}

impl ColorToken {
    /// Resolves this token to its Phase 0 dark-theme color.
    #[must_use]
    pub const fn color(self) -> Color {
        match self {
            Self::Surface(Surface::Canvas) | Self::Text(TextColor::OnAccent) => CANVAS,
            Self::Surface(Surface::Panel) => PANEL,
            Self::Surface(Surface::Raised) => RAISED,
            Self::Surface(Surface::Steel) => STEEL,
            Self::Text(TextColor::Primary) => PRIMARY_TEXT,
            Self::Text(TextColor::Secondary) => SECONDARY_TEXT,
            Self::Accent(Accent::Cyan) => CYAN,
            Self::Accent(Accent::Blue) => BLUE,
            Self::Accent(Accent::Lime) => LIME,
        }
    }
}

/// The approved spacing scale, measured in logical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Spacing {
    /// 4 logical pixels.
    Xs = 4,
    /// 8 logical pixels.
    Sm = 8,
    /// 12 logical pixels.
    Md = 12,
    /// 16 logical pixels.
    Lg = 16,
    /// 24 logical pixels.
    Xl = 24,
    /// 32 logical pixels.
    Xxl = 32,
}

impl Spacing {
    /// Returns this spacing token's logical-pixel value.
    #[must_use]
    pub const fn pixels(self) -> u8 {
        self as u8
    }
}

/// The approved corner-radius scale, measured in logical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Radius {
    /// 4 logical pixels.
    Small = 4,
    /// 6 logical pixels.
    Medium = 6,
    /// 8 logical pixels.
    Large = 8,
}

impl Radius {
    /// Returns this radius token's logical-pixel value.
    #[must_use]
    pub const fn pixels(self) -> u8 {
        self as u8
    }
}

/// The approved border-width scale, measured in logical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BorderWidth {
    /// 1 logical pixel.
    Thin = 1,
    /// 2 logical pixels.
    Thick = 2,
}

impl BorderWidth {
    /// Returns this border token's logical-pixel value.
    #[must_use]
    pub const fn pixels(self) -> u8 {
        self as u8
    }
}

/// The semantic meaning of status content.
///
/// Status color is always supplemental; components must pair it with explicit text
/// and, where appropriate, an icon or other non-color indicator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// Informational state.
    Info,
    /// Successful or positive state.
    Success,
    /// State requiring attention.
    Warning,
    /// Failed or blocking state.
    Error,
}
