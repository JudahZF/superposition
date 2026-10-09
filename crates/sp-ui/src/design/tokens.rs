//! Foundational design tokens for the Cue console UI.
//!
//! Every chrome colour and fixed dimension is defined only in this module. The chrome is a calm,
//! neutral console so the plug-in editor pictures supply the colour. Teal marks selection and
//! focus only. Warn, fault, and info are the only status colours and always accompany a text
//! state token.

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

/// One of the nineteen Cue colours.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorToken {
    /// Window and rack-column background.
    Base,
    /// Head and foot bars, the selected rack column, scene pads, popovers, and modals.
    Surface,
    /// Preview slot fill, the opaque caption bar, and text-field fill.
    Well,
    /// Button and toggle fill.
    Raised,
    /// Hovered button, pad, or list row.
    HoverFill,
    /// Rack names, readout values, caption names, the gain marker, and the peak hold.
    Strong,
    /// Body text.
    Text,
    /// Labels, secondary text, tick labels, key hints, and the gain track fill.
    Dim,
    /// Disabled text, the hovered preview ring, and the hovered empty-slot border.
    Faint,
    /// Column dividers, head and foot rules, and readout-cell dividers.
    Hairline,
    /// Empty-slot dashes, the gain track, and the scene pad border.
    Groove,
    /// Button, toggle, text-field, and popover borders.
    Outline,
    /// Selection and focus only: the selected rack bar, selected slot ring, and focus outline.
    Accent,
    /// Active scene pad fill, selected jack fill, and text selection background.
    AccentFill,
    /// Meter fill below −6 dBFS.
    MeterLit,
    /// LDG and REC tokens, lit Mute, route warnings, and meter −6 to −3 dBFS.
    Warn,
    /// MIS and FLT tokens, the "Fault" label, the live dot, and meter above −3 dBFS.
    Fault,
    /// Lit Bypass, sidechain markers, and mapped CCs.
    Info,
    /// Fault line background.
    FaultTint,
}

impl ColorToken {
    /// Every Cue colour, in the order of the design spec.
    pub const ALL: [Self; 19] = [
        Self::Base,
        Self::Surface,
        Self::Well,
        Self::Raised,
        Self::HoverFill,
        Self::Strong,
        Self::Text,
        Self::Dim,
        Self::Faint,
        Self::Hairline,
        Self::Groove,
        Self::Outline,
        Self::Accent,
        Self::AccentFill,
        Self::MeterLit,
        Self::Warn,
        Self::Fault,
        Self::Info,
        Self::FaultTint,
    ];

    /// Resolves this token to its colour.
    #[must_use]
    pub const fn color(self) -> Color {
        match self {
            Self::Base => Color::rgb(0x15, 0x18, 0x1C),
            Self::Surface => Color::rgb(0x1A, 0x1E, 0x23),
            Self::Well => Color::rgb(0x10, 0x12, 0x15),
            Self::Raised => Color::rgb(0x1D, 0x21, 0x26),
            Self::HoverFill => Color::rgb(0x25, 0x2A, 0x31),
            Self::Strong => Color::rgb(0xE7, 0xEA, 0xEE),
            Self::Text => Color::rgb(0xC9, 0xCE, 0xD5),
            Self::Dim => Color::rgb(0x86, 0x8E, 0x99),
            Self::Faint => Color::rgb(0x4D, 0x55, 0x60),
            Self::Hairline => Color::rgb(0x25, 0x2A, 0x30),
            Self::Groove => Color::rgb(0x2E, 0x35, 0x3E),
            Self::Outline => Color::rgb(0x35, 0x3C, 0x45),
            Self::Accent => Color::rgb(0x2B, 0xB3, 0xA3),
            Self::AccentFill => Color::rgb(0x1B, 0x4A, 0x46),
            Self::MeterLit => Color::rgb(0xAA, 0xB2, 0xBC),
            Self::Warn => Color::rgb(0xD9, 0xA2, 0x1B),
            Self::Fault => Color::rgb(0xE6, 0x49, 0x4D),
            Self::Info => Color::rgb(0x7F, 0xB0, 0xFF),
            Self::FaultTint => Color::rgb(0x24, 0x16, 0x1A),
        }
    }
}

/// The spacing scale in logical pixels: a 4 px unit with 2 px half-steps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Spacing {
    /// 2 px: small gaps, focus offsets, and check-mark corners.
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
    /// 14 px: padding on each side of a head readout cell's divider.
    S14 = 14,
    /// 16 px: head, foot, and fault-line edge padding.
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
    /// Head height: wordmark, readouts, and actions.
    Head,
    /// Fault line height, also the page line height. The fault line shows only while a fault is showing.
    FaultLine,
    /// Foot height: scene pads, fade, and load.
    Foot,
    /// Button and toggle height. Square fade buttons use the same height and width.
    Control,
    /// Scene pad height. Page pads are 22 px tall.
    ScenePad,
    /// Rack column header row height.
    RackHeader,
    /// Route line row height and list and menu row height.
    RouteRow,
    /// Corner radius of controls, preview slots, and popovers.
    Radius,
    /// Corner radius of small bars and markers: meter bars, gain marker, and load bars.
    SmallRadius,
    /// Rack column width: eight across 1920 px, six on the 1512 px laptop preset.
    ColumnWidth,
    /// The "+ add rack" column.
    AddRackWidth,
    /// Preview tile height at 1920×1080.
    PreviewMaxHeight,
    /// Preview tile height on the 1512×982 laptop preset.
    PreviewMinHeight,
    /// Preview caption bar height: an opaque bar across the bottom of each slot.
    Caption,
    /// Editor capture width.
    CaptureWidth,
    /// Editor capture height.
    CaptureHeight,
    /// The live-editor dot.
    LiveDot,
    /// Gain track thickness.
    GainTrack,
    /// Gain thumb width: the marker that shows the rack's gain on the track.
    GainThumbWidth,
    /// Gain thumb height.
    GainThumbHeight,
    /// The 0 dB tick height.
    GainTick,
    /// Meter bar height.
    MeterHeight,
    /// One meter bar's width.
    MeterWidth,
    /// Width of the meter scale column with its right-aligned tick labels.
    MeterScaleWidth,
    /// Height of the peak-hold line on a meter bar.
    PeakHold,
    /// Height of the accent bar across the top of the selected rack column.
    AccentBar,
    /// Width of the accent bar on the left edge of the selected preview slot.
    SlotMarker,
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
            Self::Hairline | Self::SmallRadius => 1,
            Self::GainTrack | Self::PeakHold | Self::AccentBar => 2,
            Self::LoadBarWidth | Self::Radius | Self::SlotMarker => 3,
            Self::LiveDot | Self::GainThumbWidth | Self::MeterWidth => 6,
            Self::GainTick => 10,
            Self::Caption => 18,
            Self::Line | Self::GainThumbHeight | Self::LoadBarHeight => 16,
            Self::JackHeight => 20,
            Self::MeterScaleWidth => 22,
            Self::RouteRow => 24,
            Self::Control | Self::RackHeader => 26,
            Self::ScenePad => 28,
            Self::JackWidth | Self::FaultLine => 30,
            Self::Foot => 46,
            Self::Head => 52,
            Self::PreviewMinHeight => 60,
            Self::MeterHeight => 92,
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
