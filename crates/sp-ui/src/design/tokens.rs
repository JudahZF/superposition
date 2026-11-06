//! Foundational design tokens for the rack UI.
//!
//! Every chrome colour and fixed dimension is defined only in this module. The palette follows
//! T3 Code's dark theme: near-black surfaces, one blue accent for selection and primary actions,
//! and warn, fault, info, and success colours that always accompany a text label.

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

/// One of the chrome colours.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorToken {
    /// Window background.
    Base,
    /// Cards, rails, popovers, and the scene bar.
    Surface,
    /// Primary text and lit meter segments.
    Text,
    /// Labels, secondary text, and tick marks.
    Dim,
    /// Disabled text and decorative marks such as empty-slot outlines.
    Faint,
    /// Card borders and dividers.
    Hairline,
    /// An unlit meter segment, and the fill of tracks and chips.
    SegmentOff,
    /// Row and button hover.
    HoverFill,
    /// Selection rings, the active tab, the active scene, and primary buttons.
    Accent,
    /// Text on an accent fill.
    AccentText,
    /// The ready dot.
    Success,
    /// Loading and recovering states, mute lit, and route warnings.
    Warn,
    /// Behind lit warn controls.
    WarnTint,
    /// Missing and failed states, the top meter segments, and the fault banner.
    Fault,
    /// Fault banner background.
    FaultTint,
    /// Bypass lit, the sidechain marker, and mapped CCs.
    Info,
    /// Behind lit info controls and sidechain chips.
    InfoTint,
}

impl ColorToken {
    /// Every chrome colour.
    pub const ALL: [Self; 17] = [
        Self::Base,
        Self::Surface,
        Self::Text,
        Self::Dim,
        Self::Faint,
        Self::Hairline,
        Self::SegmentOff,
        Self::HoverFill,
        Self::Accent,
        Self::AccentText,
        Self::Success,
        Self::Warn,
        Self::WarnTint,
        Self::Fault,
        Self::FaultTint,
        Self::Info,
        Self::InfoTint,
    ];

    /// Resolves this token to its colour.
    #[must_use]
    pub const fn color(self) -> Color {
        match self {
            Self::Base => Color::rgb(0x0A, 0x0A, 0x0A),
            Self::Surface => Color::rgb(0x11, 0x11, 0x11),
            Self::Text => Color::rgb(0xF5, 0xF5, 0xF5),
            Self::Dim => Color::rgb(0x81, 0x81, 0x81),
            Self::Faint => Color::rgb(0x4D, 0x4D, 0x4D),
            Self::Hairline => Color::rgb(0x23, 0x23, 0x23),
            Self::SegmentOff => Color::rgb(0x26, 0x26, 0x26),
            Self::HoverFill => Color::rgb(0x1A, 0x1A, 0x1A),
            Self::Accent => Color::rgb(0x34, 0x6B, 0xF1),
            Self::AccentText => Color::rgb(0xFF, 0xFF, 0xFF),
            Self::Success => Color::rgb(0x10, 0xB9, 0x81),
            Self::Warn => Color::rgb(0xFE, 0x9A, 0x00),
            Self::WarnTint => Color::rgb(0x31, 0x21, 0x08),
            Self::Fault => Color::rgb(0xFB, 0x41, 0x4A),
            Self::FaultTint => Color::rgb(0x30, 0x12, 0x14),
            Self::Info => Color::rgb(0x3B, 0x82, 0xF6),
            Self::InfoTint => Color::rgb(0x11, 0x1C, 0x2D),
        }
    }
}

/// The spacing scale in logical pixels: a 4 px unit with 2 px half-steps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Spacing {
    /// 2 px: meter segment gaps and hairline offsets.
    S2 = 2,
    /// 4 px: the base unit.
    S4 = 4,
    /// 6 px: gaps between cards and between chips.
    S6 = 6,
    /// 8 px: card padding in dense places.
    S8 = 8,
    /// 10 px: button and popover padding.
    S10 = 10,
    /// 12 px: card padding and the window gutter.
    S12 = 12,
    /// 16 px.
    S16 = 16,
    /// 20 px.
    S20 = 20,
    /// 24 px: gaps between groups.
    S24 = 24,
}

impl Spacing {
    /// Returns this spacing token's logical-pixel value.
    #[must_use]
    pub const fn pixels(self) -> u8 {
        self as u8
    }
}

/// Corner radii in logical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Radius {
    /// 4 px: meter segments' parent tracks, small marks, and thumbnails.
    Small = 4,
    /// 6 px: buttons, chips, rows, and toggles.
    Control = 6,
    /// 10 px: cards and popovers.
    Card = 10,
    /// 14 px: the floating scene bar.
    Bar = 14,
    /// Fully rounded: pills, dots, and the scene pads.
    Pill = 255,
}

impl Radius {
    /// Returns this radius in logical pixels.
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
    /// Top bar height.
    TopBar,
    /// Height of buttons, tabs, and the rack selector.
    Control,
    /// Height of route and sidechain chips.
    Chip,
    /// Fault banner height.
    FaultBanner,
    /// Scene bar height.
    SceneBar,
    /// The rack selector in the top bar.
    RackSelectorWidth,
    /// Longest device, session, or status text shown in the top bar.
    TopBarTextWidth,
    /// Overview page rail width.
    PageRailWidth,
    /// One rack strip in the overview: thirteen across 1920 px with the page rail.
    StripWidth,
    /// The "Add rack" strip.
    AddRackWidth,
    /// One slot row in an overview strip.
    StripSlot,
    /// Rack view input strip width.
    InputStripWidth,
    /// Rack view output strip width.
    OutputStripWidth,
    /// Rack view chain width.
    ChainWidth,
    /// One slot row in the rack view chain.
    ChainSlot,
    /// Thumbnail width in a rack view chain row.
    ChainThumbWidth,
    /// Hot plug-ins rail width.
    HotRailWidth,
    /// One hot plug-in tile.
    HotTile,
    /// Editor pane header height.
    PaneHeader,
    /// Editor capture width.
    CaptureWidth,
    /// Editor capture height.
    CaptureHeight,
    /// Caption bar under a capture in the hover peek.
    Caption,
    /// Status and live dots.
    Dot,
    /// A meter column in the rack view.
    MeterWidth,
    /// A meter column in an overview strip.
    MeterNarrowWidth,
    /// Gain fader track width.
    FaderTrack,
    /// Gain fader thumb width.
    FaderThumbWidth,
    /// Gain fader thumb height.
    FaderThumbHeight,
    /// The 0 dB tick length beside a fader.
    FaderTick,
    /// Menu minimum width.
    MenuMinWidth,
    /// Route and sidechain popover width.
    PopoverWidth,
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
            Self::LoadBarWidth => 3,
            Self::FaderTrack => 4,
            Self::Dot => 6,
            Self::MeterNarrowWidth => 8,
            Self::FaderThumbHeight => 10,
            Self::MeterWidth => 12,
            Self::Caption | Self::FaderTick => 14,
            Self::Line | Self::LoadBarHeight => 16,
            Self::Chip => 18,
            Self::JackHeight => 20,
            Self::FaderThumbWidth => 24,
            Self::Control => 26,
            Self::StripSlot | Self::JackWidth => 30,
            Self::FaultBanner => 36,
            Self::PaneHeader => 40,
            Self::SceneBar => 44,
            Self::TopBar => 52,
            Self::ChainSlot | Self::HotTile => 56,
            Self::AddRackWidth => 64,
            Self::ChainThumbWidth => 72,
            Self::InputStripWidth => 92,
            Self::OutputStripWidth => 116,
            Self::StripWidth => 124,
            Self::HotRailWidth => 176,
            Self::PageRailWidth => 184,
            Self::RackSelectorWidth | Self::MenuMinWidth => 190,
            Self::CaptureHeight | Self::SetupRailWidth | Self::FilterFieldWidth => 200,
            Self::TopBarTextWidth => 220,
            Self::ChainWidth | Self::NameFieldWidth => 260,
            Self::CaptureWidth => 320,
            Self::PopoverWidth => 336,
            Self::ConfirmWidth => 460,
            Self::PickerWidth => 560,
            Self::SceneModalWidth => 640,
            Self::ModalMaxHeight => 760,
            Self::SetupPageWidth => 900,
        }
    }
}
