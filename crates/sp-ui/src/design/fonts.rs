//! Embedded application-UI font payloads.
//!
//! Sora and Space Mono are licensed under the SIL Open Font License 1.1; the license
//! texts ship alongside the payloads in `assets/fonts/`. These bytes cover the app UI
//! only — production release assets remain gated by the release manifest (see
//! `docs/brand-assets.md`).

/// Sora Regular (400).
pub const SORA_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Sora-Regular.ttf");
/// Sora Medium (500).
pub const SORA_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Sora-Medium.ttf");
/// Sora `SemiBold` (600).
pub const SORA_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Sora-SemiBold.ttf");
/// Sora Bold (700).
pub const SORA_BOLD: &[u8] = include_bytes!("../../assets/fonts/Sora-Bold.ttf");
/// Space Mono Regular (400).
pub const SPACE_MONO_REGULAR: &[u8] = include_bytes!("../../assets/fonts/SpaceMono-Regular.ttf");
