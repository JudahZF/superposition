#![forbid(unsafe_code)]
// Every fallible function returns this crate's self-describing error types.
#![allow(clippy::missing_errors_doc)]
//! Streams plug-in editor windows to egui clients.
//!
//! On Linux the streamer (`sp-window-streamer`) sends every mapped top-level window of
//! Superposition's private X server as lossless damage tiles. On macOS each rack worker streams
//! its own editor windows with [`WindowSource`]: it connects to the socket the app names in
//! [`EDITOR_STREAM_ENV`] and serves the same session there. Clients send input back. The byte
//! stream is transport-neutral.
//!
//! # Framing
//!
//! Each message is a little-endian `u32` body length (at most [`MAX_MESSAGE_BYTES`]) followed by
//! the body. The body starts with a one-byte tag; the tag space is per direction
//! ([`ClientMessage`], [`ServerMessage`]). Integers are little-endian, strings and byte fields
//! carry a `u32` length and their own bound, optional values carry a 0/1 presence byte. Decoders
//! reject unknown tags, out-of-range values, oversized fields and trailing bytes without
//! allocating more than the body already holds.
//!
//! # Session
//!
//! 1. The client sends [`ClientMessage::Hello`] with [`PROTOCOL_VERSION`] and a [`Role`].
//! 2. The streamer replies with `Welcome`, `WindowList`, one `ControlStatus` (`Granted` or
//!    `Denied` for a controller hello, else `Current`), then a keyframe per window, and from then
//!    on `WindowOpened`/`WindowChanged`/`WindowClosed` and `Frame`s.
//! 3. One client holds input control. A controller hello takes free control; a later one gets
//!    `Denied` until it sends `RequestControl { takeover: true }`, which notifies everyone.
//!    Viewers only receive. A client that falls behind skips frames and gets a keyframe later.
//!
//! # Pixels
//!
//! Tiles are [`TileEncoding::ZstdBgrx`]; [`WindowStreamClient`] decodes them into opaque RGBA8
//! framebuffers and reports dirty rectangles for incremental texture uploads.

mod client;
mod message;
mod source;
mod tile;
mod wire;

/// macOS: the environment variable that gives a rack worker the app's editor-stream socket. The
/// worker connects to it and serves its own editor windows; the app speaks first, as a client.
pub const EDITOR_STREAM_ENV: &str = "SUPERPOSITION_EDITOR_STREAM";

pub use client::{
    ApplyError, ScrollAccumulator, StreamedWindow, WindowStreamClient, keysym_for_char,
};
pub use message::{
    ClientId, ClientMessage, ControlReason, ErrorCode, FrameUpdate, InputEvent,
    MAX_CLIENT_NAME_BYTES, MAX_ERROR_BYTES, MAX_TEXT_BYTES, MAX_TILE_DIMENSION,
    MAX_TILES_PER_UPDATE, MAX_TITLE_BYTES, MAX_WINDOW_DIMENSION, MAX_WINDOWS, PROTOCOL_MAGIC,
    PROTOCOL_VERSION, PointerButton, Rect, Role, ServerMessage, Tile, TileEncoding, WindowId,
    WindowInfo, WindowKind,
};
pub use source::{SOURCE_CLIENT, SourceAction, WindowSource};
pub use tile::{TILE_SIZE, TileEncoder, changed_tiles, tile_grid};
pub use wire::{MAX_MESSAGE_BYTES, WireError, WireMessage};
