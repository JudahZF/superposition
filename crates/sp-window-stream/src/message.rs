//! Message types for both directions and their body layouts.

use std::sync::Arc;

use crate::wire::{Body, Reader, WireError, Writer};

/// `b"SPW1"`, the first field of every [`ClientMessage::Hello`].
pub const PROTOCOL_MAGIC: u32 = u32::from_le_bytes(*b"SPW1");
/// Bumped on any incompatible wire change. Peers must match exactly.
pub const PROTOCOL_VERSION: u16 = 1;
/// Largest window width or height the stream carries.
pub const MAX_WINDOW_DIMENSION: u16 = 8192;
/// Largest tile width or height. The streamer uses [`crate::TILE_SIZE`].
pub const MAX_TILE_DIMENSION: u16 = 256;
/// Tiles per [`FrameUpdate`]: one full window of minimum-size tiles is never needed in one
/// message because senders split frames at [`crate::MAX_MESSAGE_BYTES`].
pub const MAX_TILES_PER_UPDATE: usize = 16_384;
/// Windows in one [`ServerMessage::WindowList`].
pub const MAX_WINDOWS: usize = 1024;
/// Window title bytes; the streamer truncates longer titles at a character boundary.
pub const MAX_TITLE_BYTES: usize = 512;
/// Client name bytes in [`ClientMessage::Hello`].
pub const MAX_CLIENT_NAME_BYTES: usize = 64;
/// UTF-8 bytes in one [`InputEvent::Text`].
pub const MAX_TEXT_BYTES: usize = 256;
/// Error message bytes.
pub const MAX_ERROR_BYTES: usize = 512;

/// A window ID: an X window ID on Linux, an `AppKit` window number from a macOS worker. Both are
/// unique across the processes of one display.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WindowId(pub u32);

/// A connection ID assigned by the streamer, unique for its lifetime.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientId(pub u32);

/// A pixel rectangle in window coordinates.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: u16,
    /// Top edge.
    pub y: u16,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
}

impl Rect {
    /// Exclusive right edge.
    pub fn right(self) -> u32 {
        u32::from(self.x) + u32::from(self.width)
    }
    /// Exclusive bottom edge.
    pub fn bottom(self) -> u32 {
        u32::from(self.y) + u32::from(self.height)
    }
    /// The smallest rectangle that holds both.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let width = self.right().max(other.right()) - u32::from(x);
        let height = self.bottom().max(other.bottom()) - u32::from(y);
        // Both inputs fit in u16 coordinates, so the union's extent does too.
        Self {
            x,
            y,
            width: width.try_into().unwrap_or(u16::MAX),
            height: height.try_into().unwrap_or(u16::MAX),
        }
    }
}

/// What a client asks to be when it says hello.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// Receives frames only.
    Viewer,
    /// Receives frames and asks for input control without taking it from a current holder.
    Controller,
}

/// How a window appears on the private X server, which has no window manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowKind {
    /// An ordinary top-level window: an editor, installer or dialog.
    Normal,
    /// An override-redirect window: a menu, tooltip or drop-down. Show it above
    /// [`WindowInfo::transient_for`] (or the last focused window) at its absolute position.
    Popup,
}

/// One streamed top-level window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowInfo {
    /// X window ID.
    pub id: WindowId,
    /// Normal or popup.
    pub kind: WindowKind,
    /// `_NET_WM_NAME`, else `WM_NAME`; may be empty.
    pub title: String,
    /// Left edge in root coordinates. macOS workers send 0: only popups need a position.
    pub x: i16,
    /// Top edge in root coordinates; 0 from macOS workers.
    pub y: i16,
    /// Width in pixels, `1..=MAX_WINDOW_DIMENSION`.
    pub width: u16,
    /// Height in pixels, `1..=MAX_WINDOW_DIMENSION`.
    pub height: u16,
    /// The owning worker's rack: `_SUPERPOSITION_RACK` (`CARDINAL`, format 32) on X.
    pub rack: Option<u32>,
    /// The editor's slot: `_SUPERPOSITION_SLOT` (`CARDINAL`, format 32) on X.
    pub slot: Option<u32>,
    /// `_NET_WM_PID` of the owning process.
    pub pid: Option<u32>,
    /// `WM_TRANSIENT_FOR`, when it names another streamed window.
    pub transient_for: Option<WindowId>,
}

/// How a tile's bytes are encoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TileEncoding {
    /// One zstd frame of `width * height` pixels, 4 bytes each in B, G, R, X order (X's native
    /// 32-bit `ZPixmap` on little-endian servers), rows tightly packed top to bottom. The fourth
    /// byte is ignored: decoders write opaque alpha.
    ZstdBgrx,
}

/// One changed rectangle of a window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tile {
    /// Where the tile lies in the window.
    pub rect: Rect,
    /// How [`Tile::data`] is encoded.
    pub encoding: TileEncoding,
    /// Encoded pixels; shared so a sender encodes once for every client.
    pub data: Arc<[u8]>,
}

/// Changed tiles of one window. One frame may span several messages that share a sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameUpdate {
    /// The window.
    pub window: WindowId,
    /// Per-window frame number; equal across the messages of one frame.
    pub sequence: u64,
    /// Full window width. A change resizes the client framebuffer.
    pub width: u16,
    /// Full window height.
    pub height: u16,
    /// This frame covers every tile of the window.
    pub keyframe: bool,
    /// The last message of this frame.
    pub complete: bool,
    /// Tiles inside `width` x `height`.
    pub tiles: Vec<Tile>,
}

/// Mouse buttons in X numbering. Scrolling uses [`InputEvent::Scroll`], not buttons 4 to 7.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerButton {
    /// Button 1.
    Left,
    /// Button 2.
    Middle,
    /// Button 3.
    Right,
    /// Button 8.
    Back,
    /// Button 9.
    Forward,
}

impl PointerButton {
    /// The X core button number.
    pub const fn x_button(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
            Self::Back => 8,
            Self::Forward => 9,
        }
    }
    const fn from_x_button(value: u8) -> Result<Self, WireError> {
        match value {
            1 => Ok(Self::Left),
            2 => Ok(Self::Middle),
            3 => Ok(Self::Right),
            8 => Ok(Self::Back),
            9 => Ok(Self::Forward),
            _ => Err(WireError::Invalid("pointer button")),
        }
    }
}

/// Input from the controller. Coordinates are window-local pixels and may lie outside the window
/// during a drag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputEvent {
    /// Move the pointer.
    PointerMove {
        /// Target window.
        window: WindowId,
        /// Window-local x.
        x: i16,
        /// Window-local y.
        y: i16,
    },
    /// Press or release a button at a position.
    PointerButton {
        /// Target window.
        window: WindowId,
        /// Window-local x.
        x: i16,
        /// Window-local y.
        y: i16,
        /// The button.
        button: PointerButton,
        /// Pressed or released.
        pressed: bool,
    },
    /// Scroll by whole wheel notches; positive is down and right.
    Scroll {
        /// Target window.
        window: WindowId,
        /// Window-local x.
        x: i16,
        /// Window-local y.
        y: i16,
        /// Horizontal notches.
        dx: i16,
        /// Vertical notches.
        dy: i16,
    },
    /// Press or release a key by X keysym. Modifiers are keys too (for example `Shift_L`).
    Key {
        /// Target window.
        window: WindowId,
        /// X keysym, never `NoSymbol` (0).
        keysym: u32,
        /// Pressed or released.
        pressed: bool,
    },
    /// Type committed text (IME output, paste). Send it instead of, not as well as, the keys
    /// that produced it.
    Text {
        /// Target window.
        window: WindowId,
        /// Non-empty UTF-8 text.
        text: String,
    },
    /// Raise the window and give it keyboard focus.
    Focus {
        /// Target window.
        window: WindowId,
    },
    /// Ask the window to close with `WM_DELETE_WINDOW`. The streamer never kills clients.
    Close {
        /// Target window.
        window: WindowId,
    },
}

impl InputEvent {
    /// The window the event targets.
    pub fn window(&self) -> WindowId {
        match *self {
            Self::PointerMove { window, .. }
            | Self::PointerButton { window, .. }
            | Self::Scroll { window, .. }
            | Self::Key { window, .. }
            | Self::Text { window, .. }
            | Self::Focus { window }
            | Self::Close { window } => window,
        }
    }
}

/// Messages from a client to the streamer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientMessage {
    /// Must be the first message.
    Hello {
        /// [`PROTOCOL_VERSION`] of the client.
        version: u16,
        /// Requested role.
        role: Role,
        /// Display name for logs and takeover notices.
        name: String,
    },
    /// Ask for input control. Without `takeover` a current holder keeps it.
    RequestControl {
        /// Take control from the current holder, who is notified.
        takeover: bool,
    },
    /// Give up input control.
    ReleaseControl,
    /// Ask for every tile of one window, or of all windows.
    RequestKeyframe {
        /// The window, or `None` for all.
        window: Option<WindowId>,
    },
    /// Input; accepted from the controller only.
    Input(InputEvent),
}

/// Why [`ServerMessage::ControlStatus`] was sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlReason {
    /// Only to a new client after `Welcome`: the state when it joined.
    Current,
    /// The holder asked and nobody held control.
    Granted,
    /// The holder took control from the previous holder.
    TakenOver,
    /// The previous holder released control or disconnected.
    Released,
    /// Only to the requester: someone else holds control and no takeover was asked for.
    Denied,
}

/// Error categories in [`ServerMessage::Error`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    /// The client sent a malformed message; the streamer closes the connection.
    Malformed,
    /// The Hello version does not match; the streamer closes the connection.
    UnsupportedVersion,
    /// Input or a control-only request came from a client that does not hold control.
    NotController,
    /// The window is not streamed (any more).
    UnknownWindow,
    /// The request cannot be carried out, for example closing a window without `WM_DELETE_WINDOW`.
    Unsupported,
    /// The X server refused or failed the request.
    Internal,
}

/// Messages from the streamer to a client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerMessage {
    /// Reply to a matching Hello.
    Welcome {
        /// [`PROTOCOL_VERSION`] of the streamer.
        version: u16,
        /// The client's own ID, to compare with [`ServerMessage::ControlStatus::holder`].
        client: ClientId,
    },
    /// Every streamed window, sent after `Welcome`. Keyframes follow.
    WindowList(Vec<WindowInfo>),
    /// A window was mapped.
    WindowOpened(WindowInfo),
    /// A window moved, resized, or changed title or properties.
    WindowChanged(WindowInfo),
    /// A window was unmapped or destroyed.
    WindowClosed(WindowId),
    /// Changed tiles.
    Frame(FrameUpdate),
    /// Who holds input control, sent to every client on change.
    ControlStatus {
        /// The controller, if any.
        holder: Option<ClientId>,
        /// Why it changed.
        reason: ControlReason,
    },
    /// A request failed.
    Error {
        /// Category.
        code: ErrorCode,
        /// Human-readable detail.
        message: String,
    },
}

/// Smallest encoded tile header (rect, encoding, data length), used to bound tile counts.
const TILE_HEADER_BYTES: usize = 13;
/// Smallest encoded [`WindowInfo`], used to bound window counts.
const WINDOW_INFO_MIN_BYTES: usize = 4 + 1 + 4 + 8 + 4;

/// zstd's compress bound for raw tile bytes, the largest tile payload a decoder accepts.
pub(crate) fn max_tile_data(rect: Rect) -> usize {
    let raw = usize::from(rect.width) * usize::from(rect.height) * 4;
    raw + raw / 255 + 64
}

fn put_window_info(w: &mut Writer<'_>, info: &WindowInfo) -> Result<(), WireError> {
    w.u32(info.id.0);
    w.u8(match info.kind {
        WindowKind::Normal => 0,
        WindowKind::Popup => 1,
    });
    w.str("window title", &info.title, MAX_TITLE_BYTES)?;
    w.i16(info.x);
    w.i16(info.y);
    w.u16(info.width);
    w.u16(info.height);
    w.opt_u32(info.rack);
    w.opt_u32(info.slot);
    w.opt_u32(info.pid);
    w.opt_u32(info.transient_for.map(|id| id.0));
    check_window_size(info.width, info.height)
}

fn get_window_info(r: &mut Reader<'_>) -> Result<WindowInfo, WireError> {
    let id = WindowId(r.u32()?);
    let kind = match r.u8()? {
        0 => WindowKind::Normal,
        1 => WindowKind::Popup,
        _ => return Err(WireError::Invalid("window kind")),
    };
    let title = r.string("window title", MAX_TITLE_BYTES)?;
    let info = WindowInfo {
        id,
        kind,
        title,
        x: r.i16()?,
        y: r.i16()?,
        width: r.u16()?,
        height: r.u16()?,
        rack: r.opt_u32()?,
        slot: r.opt_u32()?,
        pid: r.opt_u32()?,
        transient_for: r.opt_u32()?.map(WindowId),
    };
    check_window_size(info.width, info.height)?;
    Ok(info)
}

fn check_window_size(width: u16, height: u16) -> Result<(), WireError> {
    if (1..=MAX_WINDOW_DIMENSION).contains(&width) && (1..=MAX_WINDOW_DIMENSION).contains(&height) {
        Ok(())
    } else {
        Err(WireError::Invalid("window size"))
    }
}

fn check_tile(rect: Rect, width: u16, height: u16) -> Result<(), WireError> {
    let fits = (1..=MAX_TILE_DIMENSION).contains(&rect.width)
        && (1..=MAX_TILE_DIMENSION).contains(&rect.height)
        && rect.right() <= u32::from(width)
        && rect.bottom() <= u32::from(height);
    if fits {
        Ok(())
    } else {
        Err(WireError::Invalid("tile rectangle"))
    }
}

fn put_frame(w: &mut Writer<'_>, frame: &FrameUpdate) -> Result<(), WireError> {
    check_window_size(frame.width, frame.height)?;
    w.u32(frame.window.0);
    w.u64(frame.sequence);
    w.u16(frame.width);
    w.u16(frame.height);
    w.u8(u8::from(frame.keyframe) | (u8::from(frame.complete) << 1));
    w.count("frame tiles", frame.tiles.len(), MAX_TILES_PER_UPDATE)?;
    for tile in &frame.tiles {
        check_tile(tile.rect, frame.width, frame.height)?;
        w.u16(tile.rect.x);
        w.u16(tile.rect.y);
        w.u16(tile.rect.width);
        w.u16(tile.rect.height);
        w.u8(match tile.encoding {
            TileEncoding::ZstdBgrx => 1,
        });
        w.bytes("tile data", &tile.data, max_tile_data(tile.rect))?;
    }
    Ok(())
}

fn get_frame(r: &mut Reader<'_>) -> Result<FrameUpdate, WireError> {
    let window = WindowId(r.u32()?);
    let sequence = r.u64()?;
    let width = r.u16()?;
    let height = r.u16()?;
    check_window_size(width, height)?;
    let flags = r.u8()?;
    if flags & !0b11 != 0 {
        return Err(WireError::Invalid("frame flags"));
    }
    let count = r.count("frame tiles", MAX_TILES_PER_UPDATE, TILE_HEADER_BYTES)?;
    let mut tiles = Vec::with_capacity(count);
    for _ in 0..count {
        let rect = Rect {
            x: r.u16()?,
            y: r.u16()?,
            width: r.u16()?,
            height: r.u16()?,
        };
        check_tile(rect, width, height)?;
        let encoding = match r.u8()? {
            1 => TileEncoding::ZstdBgrx,
            _ => return Err(WireError::Invalid("tile encoding")),
        };
        let data = Arc::from(r.bytes("tile data", max_tile_data(rect))?);
        tiles.push(Tile {
            rect,
            encoding,
            data,
        });
    }
    Ok(FrameUpdate {
        window,
        sequence,
        width,
        height,
        keyframe: flags & 1 != 0,
        complete: flags & 2 != 0,
        tiles,
    })
}

mod tag {
    pub const HELLO: u8 = 1;
    pub const REQUEST_CONTROL: u8 = 2;
    pub const RELEASE_CONTROL: u8 = 3;
    pub const REQUEST_KEYFRAME: u8 = 4;
    pub const POINTER_MOVE: u8 = 16;
    pub const POINTER_BUTTON: u8 = 17;
    pub const SCROLL: u8 = 18;
    pub const KEY: u8 = 19;
    pub const TEXT: u8 = 20;
    pub const FOCUS: u8 = 21;
    pub const CLOSE: u8 = 22;

    pub const WELCOME: u8 = 1;
    pub const WINDOW_LIST: u8 = 2;
    pub const WINDOW_OPENED: u8 = 3;
    pub const WINDOW_CHANGED: u8 = 4;
    pub const WINDOW_CLOSED: u8 = 5;
    pub const FRAME: u8 = 6;
    pub const CONTROL_STATUS: u8 = 7;
    pub const ERROR: u8 = 8;
}

impl Body for ClientMessage {
    fn encode_body(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        match self {
            Self::Hello {
                version,
                role,
                name,
            } => {
                w.u8(tag::HELLO);
                w.u32(PROTOCOL_MAGIC);
                w.u16(*version);
                w.u8(match role {
                    Role::Viewer => 0,
                    Role::Controller => 1,
                });
                w.str("client name", name, MAX_CLIENT_NAME_BYTES)?;
            }
            Self::RequestControl { takeover } => {
                w.u8(tag::REQUEST_CONTROL);
                w.bool(*takeover);
            }
            Self::ReleaseControl => w.u8(tag::RELEASE_CONTROL),
            Self::RequestKeyframe { window } => {
                w.u8(tag::REQUEST_KEYFRAME);
                w.opt_u32(window.map(|id| id.0));
            }
            Self::Input(event) => put_input(w, event)?,
        }
        Ok(())
    }

    fn decode_body(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(match r.u8()? {
            tag::HELLO => {
                if r.u32()? != PROTOCOL_MAGIC {
                    return Err(WireError::BadMagic);
                }
                let version = r.u16()?;
                let role = match r.u8()? {
                    0 => Role::Viewer,
                    1 => Role::Controller,
                    _ => return Err(WireError::Invalid("role")),
                };
                Self::Hello {
                    version,
                    role,
                    name: r.string("client name", MAX_CLIENT_NAME_BYTES)?,
                }
            }
            tag::REQUEST_CONTROL => Self::RequestControl {
                takeover: r.bool()?,
            },
            tag::RELEASE_CONTROL => Self::ReleaseControl,
            tag::REQUEST_KEYFRAME => Self::RequestKeyframe {
                window: r.opt_u32()?.map(WindowId),
            },
            other => Self::Input(get_input(other, r)?),
        })
    }
}

fn put_input(w: &mut Writer<'_>, event: &InputEvent) -> Result<(), WireError> {
    match event {
        InputEvent::PointerMove { window, x, y } => {
            w.u8(tag::POINTER_MOVE);
            w.u32(window.0);
            w.i16(*x);
            w.i16(*y);
        }
        InputEvent::PointerButton {
            window,
            x,
            y,
            button,
            pressed,
        } => {
            w.u8(tag::POINTER_BUTTON);
            w.u32(window.0);
            w.i16(*x);
            w.i16(*y);
            w.u8(button.x_button());
            w.bool(*pressed);
        }
        InputEvent::Scroll {
            window,
            x,
            y,
            dx,
            dy,
        } => {
            w.u8(tag::SCROLL);
            w.u32(window.0);
            w.i16(*x);
            w.i16(*y);
            w.i16(*dx);
            w.i16(*dy);
        }
        InputEvent::Key {
            window,
            keysym,
            pressed,
        } => {
            if *keysym == 0 {
                return Err(WireError::Invalid("keysym"));
            }
            w.u8(tag::KEY);
            w.u32(window.0);
            w.u32(*keysym);
            w.bool(*pressed);
        }
        InputEvent::Text { window, text } => {
            if text.is_empty() {
                return Err(WireError::Invalid("text"));
            }
            w.u8(tag::TEXT);
            w.u32(window.0);
            w.str("text", text, MAX_TEXT_BYTES)?;
        }
        InputEvent::Focus { window } => {
            w.u8(tag::FOCUS);
            w.u32(window.0);
        }
        InputEvent::Close { window } => {
            w.u8(tag::CLOSE);
            w.u32(window.0);
        }
    }
    Ok(())
}

fn get_input(tag: u8, r: &mut Reader<'_>) -> Result<InputEvent, WireError> {
    let window = match tag {
        tag::POINTER_MOVE..=tag::CLOSE => WindowId(r.u32()?),
        other => return Err(WireError::UnknownTag(other)),
    };
    Ok(match tag {
        tag::POINTER_MOVE => InputEvent::PointerMove {
            window,
            x: r.i16()?,
            y: r.i16()?,
        },
        tag::POINTER_BUTTON => InputEvent::PointerButton {
            window,
            x: r.i16()?,
            y: r.i16()?,
            button: PointerButton::from_x_button(r.u8()?)?,
            pressed: r.bool()?,
        },
        tag::SCROLL => InputEvent::Scroll {
            window,
            x: r.i16()?,
            y: r.i16()?,
            dx: r.i16()?,
            dy: r.i16()?,
        },
        tag::KEY => {
            let keysym = r.u32()?;
            if keysym == 0 {
                return Err(WireError::Invalid("keysym"));
            }
            InputEvent::Key {
                window,
                keysym,
                pressed: r.bool()?,
            }
        }
        tag::TEXT => {
            let text = r.string("text", MAX_TEXT_BYTES)?;
            if text.is_empty() {
                return Err(WireError::Invalid("text"));
            }
            InputEvent::Text { window, text }
        }
        tag::FOCUS => InputEvent::Focus { window },
        _ => InputEvent::Close { window },
    })
}

impl Body for ServerMessage {
    fn encode_body(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        match self {
            Self::Welcome { version, client } => {
                w.u8(tag::WELCOME);
                w.u16(*version);
                w.u32(client.0);
            }
            Self::WindowList(windows) => {
                w.u8(tag::WINDOW_LIST);
                w.count("window list", windows.len(), MAX_WINDOWS)?;
                for info in windows {
                    put_window_info(w, info)?;
                }
            }
            Self::WindowOpened(info) => {
                w.u8(tag::WINDOW_OPENED);
                put_window_info(w, info)?;
            }
            Self::WindowChanged(info) => {
                w.u8(tag::WINDOW_CHANGED);
                put_window_info(w, info)?;
            }
            Self::WindowClosed(id) => {
                w.u8(tag::WINDOW_CLOSED);
                w.u32(id.0);
            }
            Self::Frame(frame) => {
                w.u8(tag::FRAME);
                put_frame(w, frame)?;
            }
            Self::ControlStatus { holder, reason } => {
                w.u8(tag::CONTROL_STATUS);
                w.opt_u32(holder.map(|id| id.0));
                w.u8(match reason {
                    ControlReason::Current => 0,
                    ControlReason::Granted => 1,
                    ControlReason::TakenOver => 2,
                    ControlReason::Released => 3,
                    ControlReason::Denied => 4,
                });
            }
            Self::Error { code, message } => {
                w.u8(tag::ERROR);
                w.u8(match code {
                    ErrorCode::Malformed => 1,
                    ErrorCode::UnsupportedVersion => 2,
                    ErrorCode::NotController => 3,
                    ErrorCode::UnknownWindow => 4,
                    ErrorCode::Unsupported => 5,
                    ErrorCode::Internal => 6,
                });
                w.str("error message", message, MAX_ERROR_BYTES)?;
            }
        }
        Ok(())
    }

    fn decode_body(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(match r.u8()? {
            tag::WELCOME => Self::Welcome {
                version: r.u16()?,
                client: ClientId(r.u32()?),
            },
            tag::WINDOW_LIST => {
                let count = r.count("window list", MAX_WINDOWS, WINDOW_INFO_MIN_BYTES)?;
                let mut windows = Vec::with_capacity(count);
                for _ in 0..count {
                    windows.push(get_window_info(r)?);
                }
                Self::WindowList(windows)
            }
            tag::WINDOW_OPENED => Self::WindowOpened(get_window_info(r)?),
            tag::WINDOW_CHANGED => Self::WindowChanged(get_window_info(r)?),
            tag::WINDOW_CLOSED => Self::WindowClosed(WindowId(r.u32()?)),
            tag::FRAME => Self::Frame(get_frame(r)?),
            tag::CONTROL_STATUS => {
                let holder = r.opt_u32()?.map(ClientId);
                let reason = match r.u8()? {
                    0 => ControlReason::Current,
                    1 => ControlReason::Granted,
                    2 => ControlReason::TakenOver,
                    3 => ControlReason::Released,
                    4 => ControlReason::Denied,
                    _ => return Err(WireError::Invalid("control reason")),
                };
                Self::ControlStatus { holder, reason }
            }
            tag::ERROR => {
                let code = match r.u8()? {
                    1 => ErrorCode::Malformed,
                    2 => ErrorCode::UnsupportedVersion,
                    3 => ErrorCode::NotController,
                    4 => ErrorCode::UnknownWindow,
                    5 => ErrorCode::Unsupported,
                    6 => ErrorCode::Internal,
                    _ => return Err(WireError::Invalid("error code")),
                };
                Self::Error {
                    code,
                    message: r.string("error message", MAX_ERROR_BYTES)?,
                }
            }
            other => return Err(WireError::UnknownTag(other)),
        })
    }
}
