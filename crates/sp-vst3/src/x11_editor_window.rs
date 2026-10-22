//! Worker-owned X11 window for a native VST3 editor, and the editor's preview picture.
//!
//! The worker connects to the display in `DISPLAY`, which the supervisor points at its private
//! X server. Each window has its own connection, so closing a window releases all of its
//! server resources. The window carries properties the window streamer uses to map it to its
//! rack and slot.

use std::{cell::Cell, marker::PhantomData, rc::Rc};

use xcb::{Xid, x};

pub use crate::editor_common::EditorWindowIdentity;
use crate::editor_common::{PREVIEW_HEIGHT, PREVIEW_WIDTH, Pixels, encode_png};

/// Size of a window before its editor reports one.
const INITIAL_SIZE: u16 = 64;
/// ICCCM and EWMH atoms a window manager creates. The private display has no window manager,
/// and toolkits look some of these up only if they exist, often when the plug-in loads: JUCE
/// then sets a property named by atom 0, and Xlib's default error handler exits the worker.
const WINDOW_MANAGER_ATOMS: [&str; 39] = [
    "WM_PROTOCOLS",
    "WM_DELETE_WINDOW",
    "WM_TAKE_FOCUS",
    "WM_STATE",
    "WM_CHANGE_STATE",
    "_NET_SUPPORTED",
    "_NET_ACTIVE_WINDOW",
    "_NET_WORKAREA",
    "_NET_FRAME_EXTENTS",
    "_NET_WM_NAME",
    "_NET_WM_ICON",
    "_NET_WM_PID",
    "_NET_WM_PING",
    "_NET_WM_USER_TIME",
    "_NET_WM_MOVERESIZE",
    "_NET_WM_ALLOWED_ACTIONS",
    "_NET_WM_ACTION_CLOSE",
    "_NET_WM_ACTION_FULLSCREEN",
    "_NET_WM_ACTION_MINIMIZE",
    "_NET_WM_ACTION_RESIZE",
    "_NET_WM_STATE",
    "_NET_WM_STATE_ABOVE",
    "_NET_WM_STATE_DEMANDS_ATTENTION",
    "_NET_WM_STATE_FULLSCREEN",
    "_NET_WM_STATE_HIDDEN",
    "_NET_WM_STATE_MAXIMIZED_HORZ",
    "_NET_WM_STATE_MAXIMIZED_VERT",
    "_NET_WM_STATE_SKIP_TASKBAR",
    "_NET_WM_WINDOW_TYPE",
    "_NET_WM_WINDOW_TYPE_NORMAL",
    "_NET_WM_WINDOW_TYPE_DIALOG",
    "_NET_WM_WINDOW_TYPE_UTILITY",
    "_NET_WM_WINDOW_TYPE_COMBO",
    "_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
    "_NET_WM_WINDOW_TYPE_POPUP_MENU",
    "_NET_WM_WINDOW_TYPE_TOOLTIP",
    "_MOTIF_WM_HINTS",
    "_XEMBED",
    "_XEMBED_INFO",
];

/// Creates the atoms a window manager would on the display in `DISPLAY`. Call it before any
/// plug-in loads; the private server runs with `-noreset`, so the atoms outlive this
/// connection.
///
/// # Errors
/// Returns an error when the display is unreachable.
pub fn prepare_display() -> Result<(), String> {
    let (connection, _) = xcb::Connection::connect(None)
        .map_err(|error| format!("could not connect to the editor display: {error}"))?;
    intern_atoms(&connection, WINDOW_MANAGER_ATOMS)?;
    Ok(())
}

/// Top-level editor window that may only be created and operated on the worker main thread.
pub struct X11EditorWindow {
    connection: xcb::Connection,
    window: x::Window,
    root: x::Window,
    screen_width: u16,
    screen_height: u16,
    wm_protocols: x::Atom,
    wm_delete_window: x::Atom,
    close_requested: Cell<bool>,
    closed: Cell<bool>,
}

/// The X11 parent window handed to VST3 `IPlugView::attached`. It cannot cross threads.
pub struct EditorParentView {
    window: u32,
    _main_thread: PhantomData<Rc<()>>,
}

impl EditorParentView {
    pub(crate) const fn window_id(&self) -> u32 {
        self.window
    }
}

impl X11EditorWindow {
    /// Creates a hidden editor window on the display in `DISPLAY`.
    ///
    /// # Errors
    /// Returns an error when the display is unreachable or rejects the window.
    pub fn new(identity: &EditorWindowIdentity<'_>) -> Result<Self, String> {
        let (connection, screen) = xcb::Connection::connect(None)
            .map_err(|error| format!("could not connect to the editor display: {error}"))?;
        let screen = usize::try_from(screen)
            .ok()
            .and_then(|screen| connection.get_setup().roots().nth(screen))
            .ok_or("editor display has no screen")?;
        let (root, screen_width, screen_height) = (
            screen.root(),
            screen.width_in_pixels(),
            screen.height_in_pixels(),
        );
        let black = screen.black_pixel();
        let window: x::Window = connection.generate_id();
        connection
            .send_and_check_request(&x::CreateWindow {
                // `CopyFromParent`, which is zero in every width.
                depth: 0,
                wid: window,
                parent: root,
                x: 0,
                y: 0,
                width: INITIAL_SIZE,
                height: INITIAL_SIZE,
                border_width: 0,
                class: x::WindowClass::InputOutput,
                visual: x::COPY_FROM_PARENT,
                value_list: &[x::Cw::BackPixel(black)],
            })
            .map_err(|error| format!("could not create editor window: {error}"))?;
        let [wm_protocols, wm_delete_window] = describe(&connection, window, identity)?;
        connection
            .flush()
            .map_err(|error| format!("could not configure editor window: {error}"))?;
        Ok(Self {
            connection,
            window,
            root,
            screen_width,
            screen_height,
            wm_protocols,
            wm_delete_window,
            close_requested: Cell::new(false),
            closed: Cell::new(false),
        })
    }

    /// The parent window accepted by VST3 `IPlugView::attached` as `X11EmbedWindowID`.
    ///
    /// # Errors
    /// Returns an error after the window has closed.
    pub fn content_view(&self) -> Result<EditorParentView, String> {
        if self.closed.get() {
            return Err("native editor window is closed".to_owned());
        }
        Ok(EditorParentView {
            window: self.window.resource_id(),
            _main_thread: PhantomData,
        })
    }

    /// Applies the plug-in's accepted content size.
    pub fn resize(&self, width: u32, height: u32) {
        self.configure(&[
            x::ConfigWindow::Width(width.max(1)),
            x::ConfigWindow::Height(height.max(1)),
        ]);
    }

    /// Shows the window above the others and gives it keyboard focus.
    pub fn focus(&self) {
        if self.closed.get() {
            return;
        }
        self.connection.send_request(&x::MapWindow {
            window: self.window,
        });
        self.configure(&[x::ConfigWindow::StackMode(x::StackMode::Above)]);
        self.connection.send_request(&x::SetInputFocus {
            revert_to: x::InputFocus::PointerRoot,
            focus: self.window,
            time: x::CURRENT_TIME,
        });
        let _ = self.connection.flush();
    }

    /// Takes a close request without closing the window or detaching its plug-in view. A
    /// `WM_DELETE_WINDOW` message, [`Self::request_close`], or a lost display requests a close.
    pub fn take_close_request(&self) -> bool {
        !self.closed.get()
            && (self.close_requested.replace(false) || self.connection.has_error().is_err())
    }

    /// Requests the same close as a window manager's `WM_DELETE_WINDOW` message.
    pub fn request_close(&self) {
        if !self.closed.get() {
            self.close_requested.set(true);
        }
    }

    /// Destroys the window after the plug-in view has detached.
    pub fn close(&self) {
        if !self.closed.replace(true) {
            self.connection.send_request(&x::DestroyWindow {
                window: self.window,
            });
            let _ = self.connection.flush();
        }
    }

    /// Moves the window's top-left corner to `left`, `top` pixels from the screen's top-left.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "positions are clamped to the X11 coordinate range first"
    )]
    pub fn set_top_left(&self, left: f64, top: f64) {
        let coordinate = |value: f64| value.round().clamp(-32_768.0, 32_767.0) as i32;
        self.configure(&[
            x::ConfigWindow::X(coordinate(left)),
            x::ConfigWindow::Y(coordinate(top)),
        ]);
    }

    /// Handles queued events for this window: a `WM_DELETE_WINDOW` message becomes a close
    /// request. The plug-in's own window has its own connection and run-loop handlers.
    pub fn pump_events(&self) {
        while let Ok(Some(event)) = self.connection.poll_for_event() {
            if let xcb::Event::X(x::Event::ClientMessage(message)) = event
                && message.window() == self.window
                && message.r#type() == self.wm_protocols
                && let x::ClientMessageData::Data32([atom, ..]) = message.data()
                && atom == self.wm_delete_window.resource_id()
            {
                self.request_close();
            }
        }
    }

    /// Pictures the editor's visible content as a 320×200 PNG, scaled to cover and
    /// centre-cropped. The X server's image of the window includes the plug-in's child window.
    ///
    /// # Errors
    /// Returns an error when the window is closed, unmapped, off-screen, in an unsupported
    /// pixel format, or PNG encoding fails.
    pub fn capture_preview_png(&self) -> Result<Vec<u8>, String> {
        if self.closed.get() {
            return Err("native editor window is closed".to_owned());
        }
        let failed = |error: xcb::Error| format!("could not picture editor window: {error}");
        let geometry = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::GetGeometry {
                drawable: x::Drawable::Window(self.window),
            }))
            .map_err(failed)?;
        let origin = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::TranslateCoordinates {
                src_window: self.window,
                dst_window: self.root,
                src_x: 0,
                src_y: 0,
            }))
            .map_err(failed)?;
        // GetImage rejects a rectangle that leaves the screen.
        let visible = |origin: i16, size: u16, screen: u16| {
            let start = i32::from(origin).max(0);
            let end = (i32::from(origin) + i32::from(size)).min(i32::from(screen));
            let offset = i16::try_from(start - i32::from(origin)).ok()?;
            let length = u16::try_from(end - start)
                .ok()
                .filter(|length| *length > 0)?;
            Some((offset, length))
        };
        let ((x, width), (y, height)) =
            visible(origin.dst_x(), geometry.width(), self.screen_width)
                .zip(visible(
                    origin.dst_y(),
                    geometry.height(),
                    self.screen_height,
                ))
                .ok_or("native editor window is off-screen")?;
        let image = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::GetImage {
                format: x::ImageFormat::ZPixmap,
                drawable: x::Drawable::Window(self.window),
                x,
                y,
                width,
                height,
                plane_mask: u32::MAX,
            }))
            .map_err(failed)?;
        let setup = self.connection.get_setup();
        let bits_per_pixel = setup
            .pixmap_formats()
            .iter()
            .find(|format| format.depth() == image.depth())
            .map(x::Format::bits_per_pixel)
            .ok_or("editor window has an unknown pixel depth")?;
        if bits_per_pixel != 32 {
            return Err(format!(
                "editor window pixels use an unsupported {bits_per_pixel}-bit format"
            ));
        }
        let pixels = pixels_from_zpixmap(
            image.data(),
            usize::from(width),
            usize::from(height),
            setup.image_byte_order() == x::ImageOrder::LsbFirst,
        )?;
        encode_png(&pixels.cover(PREVIEW_WIDTH, PREVIEW_HEIGHT))
    }

    fn configure(&self, values: &[x::ConfigWindow]) {
        if self.closed.get() {
            return;
        }
        self.connection.send_request(&x::ConfigureWindow {
            window: self.window,
            value_list: values,
        });
        let _ = self.connection.flush();
    }
}

impl Drop for X11EditorWindow {
    fn drop(&mut self) {
        self.close();
    }
}

/// Sets the window's title, class, owner, rack and slot, and opts in to `WM_DELETE_WINDOW`.
/// Returns the `WM_PROTOCOLS` and `WM_DELETE_WINDOW` atoms.
fn describe(
    connection: &xcb::Connection,
    window: x::Window,
    identity: &EditorWindowIdentity<'_>,
) -> Result<[x::Atom; 2], String> {
    let [
        wm_protocols,
        wm_delete_window,
        net_wm_name,
        utf8_string,
        net_wm_pid,
        rack,
        slot,
    ] = intern_atoms(
        connection,
        [
            "WM_PROTOCOLS",
            "WM_DELETE_WINDOW",
            "_NET_WM_NAME",
            "UTF8_STRING",
            "_NET_WM_PID",
            "_SUPERPOSITION_RACK",
            "_SUPERPOSITION_SLOT",
        ],
    )?;
    let set = |property, r#type, data: &[u8]| {
        connection.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window,
            property,
            r#type,
            data,
        });
    };
    // `WM_NAME` is Latin-1 text; `_NET_WM_NAME` carries the exact UTF-8 title.
    let latin1: Vec<u8> = identity
        .title
        .chars()
        .map(|character| u8::try_from(character).unwrap_or(b'?'))
        .collect();
    set(x::ATOM_WM_NAME, x::ATOM_STRING, &latin1);
    set(net_wm_name, utf8_string, identity.title.as_bytes());
    set(
        x::ATOM_WM_CLASS,
        x::ATOM_STRING,
        b"sp-plugin-worker\0Superposition\0",
    );
    let cardinal = |property, value: u32| {
        connection.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window,
            property,
            r#type: x::ATOM_CARDINAL,
            data: &[value],
        });
    };
    cardinal(net_wm_pid, std::process::id());
    cardinal(rack, identity.rack);
    cardinal(slot, identity.slot);
    connection.send_request(&x::ChangeProperty {
        mode: x::PropMode::Replace,
        window,
        property: wm_protocols,
        r#type: x::ATOM_ATOM,
        data: &[wm_delete_window],
    });
    Ok([wm_protocols, wm_delete_window])
}

fn intern_atoms<const N: usize>(
    connection: &xcb::Connection,
    names: [&str; N],
) -> Result<[x::Atom; N], String> {
    let cookies = names.map(|name| {
        connection.send_request(&x::InternAtom {
            only_if_exists: false,
            name: name.as_bytes(),
        })
    });
    let mut atoms = [x::ATOM_NONE; N];
    for (atom, cookie) in atoms.iter_mut().zip(cookies) {
        *atom = connection
            .wait_for_reply(cookie)
            .map_err(|error| format!("could not intern editor window atom: {error}"))?
            .atom();
    }
    Ok(atoms)
}

/// Reads a 32-bit `ZPixmap` image of depth 24 or 32. Each pixel is `0xXXRRGGBB` in the
/// server's byte order, and rows have no padding.
fn pixels_from_zpixmap(
    data: &[u8],
    width: usize,
    height: usize,
    little_endian: bool,
) -> Result<Pixels, String> {
    let pixels = width * height;
    let data = data
        .get(..pixels * 4)
        .ok_or("editor window image is truncated")?;
    let mut rgb = Vec::with_capacity(pixels * 3);
    for pixel in data.chunks_exact(4) {
        let [blue, green, red] = if little_endian {
            [pixel[0], pixel[1], pixel[2]]
        } else {
            [pixel[3], pixel[2], pixel[1]]
        };
        rgb.extend([red, green, blue]);
    }
    Ok(Pixels { width, height, rgb })
}


#[cfg(test)]
mod tests {
    use super::pixels_from_zpixmap;

    #[test]
    fn zpixmap_pixels_decode_in_either_byte_order() {
        let little = [0x30, 0x20, 0x10, 0xff];
        let big = [0xff, 0x10, 0x20, 0x30];
        for (data, little_endian) in [(little, true), (big, false)] {
            let pixels = pixels_from_zpixmap(&data, 1, 1, little_endian).expect("one pixel");
            assert_eq!(pixels.rgb, [0x10, 0x20, 0x30]);
        }
        assert!(pixels_from_zpixmap(&little[..3], 1, 1, true).is_err());
    }

}
