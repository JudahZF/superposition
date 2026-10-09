//! Embedded application-UI font payloads.
//!
//! Inter (rsms/inter v4.1) and IBM Plex Mono are both licensed under the SIL Open Font License
//! 1.1. The license texts, `OFL-Inter.txt` and `OFL-IBMPlexMono.txt`, ship alongside the payloads
//! in `assets/fonts/`. Production release assets remain gated by the release manifest (see
//! `docs/brand-assets.md`).

/// Inter Regular (400): the interface face.
pub const INTER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
/// Inter Medium (500): head readout values.
pub const INTER_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Inter-Medium.ttf");
/// Inter `SemiBold` (600): titles, state tokens, and the wordmark.
pub const INTER_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");
/// IBM Plex Mono Regular (400): the monospace family.
pub const PLEX_MONO_REGULAR: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf");
/// IBM Plex Mono Medium (500).
pub const PLEX_MONO_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Medium.ttf");
