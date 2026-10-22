//! The serving side for a process that streams its own windows from whole pictures.
//!
//! A macOS rack worker has no window server it can watch for damage, so it pictures each of its
//! editor windows and hands over the whole frame. [`WindowSource`] keeps each window's last frame,
//! compares the next one tile by tile, and sends only the tiles that changed. It answers the
//! session messages for exactly one client and does no I/O.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

use crate::{
    message::{
        ClientId, ClientMessage, ControlReason, ErrorCode, FrameUpdate, InputEvent,
        MAX_TILES_PER_UPDATE, MAX_TITLE_BYTES, MAX_WINDOW_DIMENSION, PROTOCOL_VERSION, Role,
        ServerMessage, Tile, WindowId, WindowInfo,
    },
    tile::{TileEncoder, changed_tiles, tile_grid},
};

/// The ID of the one client a [`WindowSource`] serves.
pub const SOURCE_CLIENT: ClientId = ClientId(1);

/// Tile bytes per frame message, so a large keyframe arrives in parts.
const MAX_FRAME_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// What the owner does after [`WindowSource::receive`], besides sending the queued messages.
#[derive(Debug, Eq, PartialEq)]
pub enum SourceAction {
    /// Nothing more.
    None,
    /// Inject this input: the client holds control and the window is streamed.
    Input(InputEvent),
    /// Close the connection after sending the queued messages.
    Close,
}

struct Window {
    info: WindowInfo,
    sequence: u64,
    width: u16,
    height: u16,
    /// The last frame sent, BGRX; empty until the first frame of a session.
    pixels: Vec<u8>,
}

struct Session {
    controller: bool,
    /// Windows whose next frame must be a keyframe.
    stale: BTreeSet<WindowId>,
}

/// One process's windows, served to one client from whole BGRX frames.
pub struct WindowSource {
    windows: BTreeMap<WindowId, Window>,
    encoder: TileEncoder,
    session: Option<Session>,
}

impl WindowSource {
    /// No windows and no client, with its own zstd context.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            windows: BTreeMap::new(),
            encoder: TileEncoder::new()?,
            session: None,
        })
    }

    /// A client said hello and has not disconnected. Only then do frames matter.
    pub fn streaming(&self) -> bool {
        self.session.is_some()
    }

    /// Forgets the client and the last frames. The window list stays.
    pub fn disconnect(&mut self) {
        self.session = None;
        for window in self.windows.values_mut() {
            window.pixels = Vec::new();
        }
    }

    /// Answers one client message into `out`.
    pub fn receive(
        &mut self,
        message: ClientMessage,
        out: &mut Vec<ServerMessage>,
    ) -> SourceAction {
        let Some(session) = self.session.as_mut() else {
            return match message {
                ClientMessage::Hello { version, role, .. } if version == PROTOCOL_VERSION => {
                    let controller = role == Role::Controller;
                    self.session = Some(Session {
                        controller,
                        stale: self.windows.keys().copied().collect(),
                    });
                    out.push(ServerMessage::Welcome {
                        version,
                        client: SOURCE_CLIENT,
                    });
                    out.push(ServerMessage::WindowList(
                        self.windows.values().map(|w| w.info.clone()).collect(),
                    ));
                    out.push(ServerMessage::ControlStatus {
                        holder: controller.then_some(SOURCE_CLIENT),
                        reason: if controller {
                            ControlReason::Granted
                        } else {
                            ControlReason::Current
                        },
                    });
                    SourceAction::None
                }
                ClientMessage::Hello { .. } => {
                    out.push(error(
                        ErrorCode::UnsupportedVersion,
                        format!("this source speaks version {PROTOCOL_VERSION}"),
                    ));
                    SourceAction::Close
                }
                _ => {
                    out.push(error(ErrorCode::Malformed, "expected Hello"));
                    SourceAction::Close
                }
            };
        };
        match message {
            ClientMessage::Hello { .. } => out.push(error(ErrorCode::Malformed, "repeated Hello")),
            ClientMessage::RequestControl { .. } => {
                if !session.controller {
                    session.controller = true;
                    out.push(ServerMessage::ControlStatus {
                        holder: Some(SOURCE_CLIENT),
                        reason: ControlReason::Granted,
                    });
                }
            }
            ClientMessage::ReleaseControl => {
                if session.controller {
                    session.controller = false;
                    out.push(ServerMessage::ControlStatus {
                        holder: None,
                        reason: ControlReason::Released,
                    });
                }
            }
            ClientMessage::RequestKeyframe { window: None } => {
                session.stale = self.windows.keys().copied().collect();
            }
            ClientMessage::RequestKeyframe {
                window: Some(window),
            } => {
                if self.windows.contains_key(&window) {
                    session.stale.insert(window);
                } else {
                    out.push(unknown_window(window));
                }
            }
            ClientMessage::Input(event) => {
                if !session.controller {
                    out.push(error(ErrorCode::NotController, "input needs control"));
                } else if !self.windows.contains_key(&event.window()) {
                    out.push(unknown_window(event.window()));
                } else {
                    return SourceAction::Input(event);
                }
            }
        }
        SourceAction::None
    }

    /// Replaces the window list and queues `WindowOpened`, `WindowChanged`, and `WindowClosed`
    /// for the client. Titles are shortened to [`MAX_TITLE_BYTES`]; windows outside
    /// `1..=MAX_WINDOW_DIMENSION` are not streamed.
    pub fn set_windows(
        &mut self,
        windows: impl IntoIterator<Item = WindowInfo>,
        out: &mut Vec<ServerMessage>,
    ) {
        let streaming = self.session.is_some();
        let mut next = BTreeMap::new();
        for mut info in windows {
            if !valid_size(info.width, info.height) {
                continue;
            }
            truncate_at_char(&mut info.title, MAX_TITLE_BYTES);
            let window = if let Some(mut window) = self.windows.remove(&info.id) {
                if window.info != info && streaming {
                    out.push(ServerMessage::WindowChanged(info.clone()));
                }
                window.info = info;
                window
            } else {
                if streaming {
                    out.push(ServerMessage::WindowOpened(info.clone()));
                }
                Window {
                    info,
                    sequence: 0,
                    width: 0,
                    height: 0,
                    pixels: Vec::new(),
                }
            };
            next.insert(window.info.id, window);
        }
        for &id in self.windows.keys() {
            if let Some(session) = self.session.as_mut() {
                session.stale.remove(&id);
                out.push(ServerMessage::WindowClosed(id));
            }
        }
        self.windows = next;
    }

    /// Takes a new whole frame of `window`: `width * height` BGRX pixels, rows tightly packed top
    /// to bottom. Queues the tiles that changed since the last frame, or every tile when the
    /// client needs a keyframe or the size changed. Without a client, or for a window that is not
    /// streamed (any more), the frame is dropped.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::InvalidInput`] when the size is out of range or `pixels` does not
    /// match it, and any zstd failure.
    pub fn update_frame(
        &mut self,
        window: WindowId,
        width: u16,
        height: u16,
        pixels: Vec<u8>,
        out: &mut Vec<ServerMessage>,
    ) -> io::Result<()> {
        let (Some(session), Some(state)) = (self.session.as_mut(), self.windows.get_mut(&window))
        else {
            return Ok(());
        };
        if !valid_size(width, height)
            || pixels.len() != usize::from(width) * usize::from(height) * 4
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame size does not match its pixels or is out of range",
            ));
        }
        let keyframe = session.stale.remove(&window)
            || (width, height) != (state.width, state.height)
            || state.pixels.is_empty();
        let stride = usize::from(width) * 4;
        let tiles: Vec<Tile> = if keyframe {
            tile_grid(width, height)
                .map(|rect| self.encoder.encode(&pixels, stride, rect))
                .collect::<io::Result<_>>()?
        } else {
            changed_tiles(&state.pixels, &pixels, width, height)
                .map(|rect| self.encoder.encode(&pixels, stride, rect))
                .collect::<io::Result<_>>()?
        };
        state.width = width;
        state.height = height;
        state.pixels = pixels;
        if tiles.is_empty() {
            return Ok(());
        }
        state.sequence += 1;
        let frame = |tiles, complete| {
            ServerMessage::Frame(FrameUpdate {
                window,
                sequence: state.sequence,
                width,
                height,
                keyframe,
                complete,
                tiles,
            })
        };
        let mut chunk: Vec<Tile> = Vec::new();
        let mut bytes = 0;
        for tile in tiles {
            if !chunk.is_empty()
                && (bytes + tile.data.len() > MAX_FRAME_CHUNK_BYTES
                    || chunk.len() == MAX_TILES_PER_UPDATE)
            {
                out.push(frame(std::mem::take(&mut chunk), false));
                bytes = 0;
            }
            bytes += tile.data.len();
            chunk.push(tile);
        }
        out.push(frame(chunk, true));
        Ok(())
    }
}

fn valid_size(width: u16, height: u16) -> bool {
    (1..=MAX_WINDOW_DIMENSION).contains(&width) && (1..=MAX_WINDOW_DIMENSION).contains(&height)
}

fn error(code: ErrorCode, message: impl Into<String>) -> ServerMessage {
    ServerMessage::Error {
        code,
        message: message.into(),
    }
}

fn unknown_window(window: WindowId) -> ServerMessage {
    error(
        ErrorCode::UnknownWindow,
        format!("window {} is not streamed", window.0),
    )
}

/// Shortens `text` to at most `max` bytes without splitting a character.
fn truncate_at_char(text: &mut String, max: usize) {
    if text.len() > max {
        let end = (0..=max)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text.truncate(end);
    }
}
