//! Foundational design tokens for the film-strip UI.
//!
//! Every chrome colour and fixed dimension is defined only in this module. The chrome is almost
//! colourless so the plug-in editor pictures supply the colour; warn, fault, and info are the only
//! chromatic values and always accompany a text state token.

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

/// One of the eleven chrome colours.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorToken {
    /// Window background.
    Base,
    /// Primary text, lit meter segments, and the inverted-selection fill.
    Text,
    /// Labels, secondary text, and tick marks.
    Dim,
    /// Empty slots, unlit borders, and disabled text. Also secondary text on an inverted fill.
    Faint,
    /// Column dividers and the head and foot rules.
    Hairline,
    /// An unlit meter segment.
    SegmentOff,
    /// Row hover in lists.
    HoverFill,
    /// LDG and REC tokens, mute lit, and route warnings.
    Warn,
    /// MIS and FLT tokens, the top two meter segments, and the fault line.
    Fault,
    /// Bypass lit, the sidechain marker, and mapped CCs.
    Info,
    /// Fault line background.
    FaultTint,
}

impl ColorToken {
    /// Every chrome colour, in the order of the design brief.
    pub const ALL: [Self; 11] = [
        Self::Base,
        Self::Text,
        Self::Dim,
        Self::Faint,
        Self::Hairline,
        Self::SegmentOff,
        Self::HoverFill,
        Self::Warn,
        Self::Fault,
        Self::Info,
        Self::FaultTint,
    ];

    /// Resolves this token to its colour.
    #[must_use]
    pub const fn color(self) -> Color {
        match self {
            Self::Base => Color::rgb(0x12, 0x13, 0x15),
            Self::Text => Color::rgb(0xD9, 0xDA, 0xD6),
            Self::Dim => Color::rgb(0x7C, 0x80, 0x88),
            Self::Faint => Color::rgb(0x3A, 0x3E, 0x45),
            Self::Hairline => Color::rgb(0x26, 0x29, 0x2E),
            Self::SegmentOff => Color::rgb(0x24, 0x27, 0x2C),
            Self::HoverFill => Color::rgb(0x1B, 0x1D, 0x21),
            Self::Warn => Color::rgb(0xD9, 0xA2, 0x1B),
            Self::Fault => Color::rgb(0xE5, 0x48, 0x4D),
            Self::Info => Color::rgb(0x7F, 0xB0, 0xFF),
            Self::FaultTint => Color::rgb(0x1A, 0x12, 0x14),
        }
    }
}

/// The spacing scale in logical pixels: a 4 px unit with 2 px half-steps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Spacing {
    /// 2 px: meter segment gaps and hairline offsets.
    S2 = 2,
    /// 4 px: the base unit, preview gaps.
    S4 = 4,
    /// 6 px: toggle and caption padding.
    S6 = 6,
    /// 8 px.
    S8 = 8,
    /// 10 px: popover vertical padding, meter column gaps.
    S10 = 10,
    /// 12 px: column and popover horizontal padding.
    S12 = 12,
    /// 16 px.
    S16 = 16,
    /// 20 px: head, foot, and fault-line edge padding.
    S20 = 20,
    /// 24 px: gaps between head readouts.
    S24 = 24,
}

impl Spacing {
    /// Returns this spacing token's logical-pixel value.
    #[must_use]
    pub const fn pixels(self) -> u8 {
        self as u8
    }
}

/// Fixed layout dimensions in logical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Layout {
    /// Divider and border width.
    Hairline,
    /// The text line grid.
    Line,
    /// Head height.
    Head,
    /// Fault line height, shown only while a fault is showing.
    FaultLine,
    /// Foot height.
    Foot,
    /// Rack column width: eight across 1920 px, six on the 1512 px laptop preset.
    ColumnWidth,
    /// The "+ add rack" column.
    AddRackWidth,
    /// Preview tile height at 1920×1080.
    PreviewMaxHeight,
    /// Preview tile height on the 1512×982 laptop preset.
    PreviewMinHeight,
    /// Preview caption bar height.
    Caption,
    /// Editor capture width.
    CaptureWidth,
    /// Editor capture height.
    CaptureHeight,
    /// The live-editor dot.
    LiveDot,
    /// Gain track thickness.
    GainTrack,
    /// Gain marker width.
    GainMarkerWidth,
    /// Gain marker height.
    GainMarkerHeight,
    /// The 0 dB tick height.
    GainTick,
    /// LED meter height.
    MeterHeight,
    /// One LED meter column's width.
    MeterWidth,
    /// One LED segment's height: sixteen segments and 2 px gaps fill the meter height.
    MeterSegment,
    /// Menu minimum width.
    MenuMinWidth,
    /// Route and sidechain popover width.
    PopoverWidth,
    /// Longest device or session name shown in the head.
    SessionNameWidth,
    /// Plug-in picker width.
    PickerWidth,
    /// Plug-in picker and scene modal maximum height.
    ModalMaxHeight,
    /// Scene capture and edit modal width.
    SceneModalWidth,
    /// Confirmation modal width.
    ConfirmWidth,
    /// One route or sidechain jack's width.
    JackWidth,
    /// One route or sidechain jack's height.
    JackHeight,
    /// Setup page tab rail width.
    SetupRailWidth,
    /// Setup page content width limit.
    SetupPageWidth,
    /// One callback-load bar's width.
    LoadBarWidth,
    /// Tallest callback-load bar.
    LoadBarHeight,
    /// Text field width for names.
    NameFieldWidth,
    /// Text field width for filters.
    FilterFieldWidth,
}

impl Layout {
    /// Returns this dimension in logical pixels.
    #[must_use]
    pub const fn pixels(self) -> u16 {
        match self {
            Self::Hairline => 1,
            Self::GainTrack | Self::GainMarkerWidth => 2,
            Self::LoadBarWidth | Self::MeterSegment => 3,
            Self::LiveDot | Self::GainTick => 6,
            Self::GainMarkerHeight | Self::MeterWidth => 12,
            Self::Caption | Self::LoadBarHeight => 14,
            Self::Line => 16,
            Self::JackHeight => 20,
            Self::JackWidth => 30,
            Self::FaultLine => 32,
            Self::Foot => 40,
            Self::Head => 56,
            Self::PreviewMinHeight => 60,
            Self::MeterHeight => 80,
            Self::PreviewMaxHeight => 84,
            Self::AddRackWidth => 120,
            Self::MenuMinWidth => 190,
            Self::ColumnWidth => 240,
            Self::CaptureHeight | Self::SetupRailWidth | Self::FilterFieldWidth => 200,
            Self::NameFieldWidth => 260,
            Self::PopoverWidth => 336,
            Self::SessionNameWidth => 180,
            Self::CaptureWidth => 320,
            Self::ConfirmWidth => 460,
            Self::PickerWidth => 560,
            Self::SceneModalWidth => 640,
            Self::ModalMaxHeight => 760,
            Self::SetupPageWidth => 900,
        }
    }
}
