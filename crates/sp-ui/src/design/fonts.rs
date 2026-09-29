//! Embedded application-UI font payloads.
//!
//! IBM Plex Mono is licensed under the SIL Open Font License 1.1; the license text ships
//! alongside the payloads in `assets/fonts/`. Production release assets remain gated by the
//! release manifest (see `docs/brand-assets.md`).

/// IBM Plex Mono Regular (400).
pub const PLEX_MONO_REGULAR: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Regular.ttf");
/// IBM Plex Mono Medium (500).
pub const PLEX_MONO_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/IBMPlexMono-Medium.ttf");
