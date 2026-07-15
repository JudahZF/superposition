//! Semantic icon identifiers independent of an icon font or renderer.

/// An icon selected by meaning rather than a concrete glyph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IconId {
    /// A healthy or available state.
    Online,
    /// A disconnected or unavailable state.
    Offline,
    /// A warning requiring attention.
    Warning,
    /// A failure or blocking issue.
    Error,
    /// A plug-in slot.
    Plugin,
    /// A scene trigger pad.
    Scene,
    /// An audio level meter.
    Meter,
}
