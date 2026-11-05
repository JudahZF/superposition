//! Embedded application-UI font payloads.
//!
//! Inter and IBM Plex Mono are licensed under the SIL Open Font License 1.1; the license texts
//! ship alongside the payloads in `assets/fonts/`. The Inter files are static instances of
//! Inter 4.1's variable font. Production release assets remain gated by the release manifest
//! (see `docs/brand-assets.md`).

/// Inter Regular (400).
pub const INTER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
/// Inter semibold (600).
pub const INTER_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");
/// IBM Plex Mono Regular (400).
pub const PLEX_MONO_REGULAR: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf");
