//! The private X server: top-level window tracking, damage and pixel capture.
//!
//! There is no window manager, so every top-level window is a direct child of the root. The
//! streamer redirects all of them with Composite (automatic mode, so the server still paints the
//! screen for XTEST hit-testing) and reads each window's own pixmap. That keeps overlapping and
//! partly off-screen windows complete. Override-redirect windows (menus, tooltips) are streamed
//! as their own [`WindowKind::Popup`] windows with a best-guess parent.

use std::{borrow::Cow, collections::BTreeMap, sync::Arc, time::Instant};

use sp_window_stream::{
    MAX_TITLE_BYTES, MAX_WINDOW_DIMENSION, MAX_WINDOWS, Rect, Tile, TileEncoder, WindowId,
    WindowInfo, WindowKind, tile_grid,
};
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        Event,
        composite::{self, ConnectionExt as _, Redirect},
        damage::{self, ConnectionExt as _, ReportLevel},
        shm::{self, ConnectionExt as _},
        xfixes::{self, ConnectionExt as _},
        xproto::{
            AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, ImageFormat,
            ImageOrder, MapState, Pixmap, Window, WindowClass,
        },
        xtest::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
};

use crate::{XResult, shm::ShmSegment};

x11rb::atom_manager! {
    /// Atoms the streamer reads or sends.
    pub Atoms: AtomsCookie {
        UTF8_STRING,
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        _NET_WM_NAME,
        _NET_WM_PID,
        _SUPERPOSITION_RACK,
        _SUPERPOSITION_SLOT,
    }
}

/// A client-visible change in the window set.
pub enum Change {
    Opened(WindowInfo),
    Changed(WindowInfo),
    Closed(WindowId),
}

/// One streamed window and the last frame sent for it.
pub struct Tracked {
    pub info: WindowInfo,
    border: u16,
    damage: damage::Damage,
    pixmap: Pixmap,
    /// Last captured BGRX pixels, `width * 4` bytes per row.
    frame: Vec<u8>,
    grid: Vec<Rect>,
    /// The current encoded content of every grid cell, after the first capture.
    pub tiles: Vec<Tile>,
    damaged: bool,
    /// The next capture sends every tile (new or resized window).
    full: bool,
    pub sequence: u64,
    pub last_capture: Option<Instant>,
}

impl Tracked {
    pub fn needs_capture(&self) -> bool {
        self.damaged || self.full
    }

    /// Every tile is known, so a keyframe can be sent without capturing.
    pub fn has_frame(&self) -> bool {
        !self.tiles.is_empty() && !self.full
    }

    fn reset_frame(&mut self) {
        let (width, height) = (self.info.width, self.info.height);
        self.frame = vec![0; usize::from(width) * usize::from(height) * 4];
        self.grid = tile_grid(width, height).collect();
        self.tiles.clear();
        self.full = true;
    }
}

/// Tiles from one capture.
pub struct Captured {
    pub tiles: Vec<Tile>,
    pub keyframe: bool,
}

/// Where captures land: an MIT-SHM segment, or `GetImage` replies.
enum Grabber {
    Shm(Option<ShmSegment>),
    Core,
}

impl Grabber {
    /// Reads a `width` x `height` BGRX area of `pixmap` at (`x`, `y`). Falls back to
    /// `GetImage` for good if the server cannot create a shared segment.
    fn grab(
        &mut self,
        conn: &Arc<RustConnection>,
        pixmap: Pixmap,
        (x, y): (u16, u16),
        (width, height): (u16, u16),
    ) -> XResult<Cow<'_, [u8]>> {
        let (x, y) = (i16::try_from(x)?, i16::try_from(y)?);
        let len = usize::from(width) * usize::from(height) * 4;
        if let Self::Shm(segment) = self
            && segment.as_ref().is_none_or(|s| s.len() < len)
        {
            *segment = None;
            // Round up so small growth does not recreate the segment every time.
            match ShmSegment::create(conn, len.next_power_of_two().max(1 << 20)) {
                Ok(created) => *segment = Some(created),
                Err(error) => {
                    eprintln!("sp-window-streamer: MIT-SHM unavailable ({error}); using GetImage");
                    *self = Self::Core;
                }
            }
        }
        let format = ImageFormat::Z_PIXMAP;
        match self {
            Self::Shm(segment) => {
                let segment = segment.as_ref().expect("created above");
                conn.shm_get_image(
                    pixmap,
                    x,
                    y,
                    width,
                    height,
                    !0,
                    format.into(),
                    segment.seg(),
                    0,
                )?
                .reply()?;
                Ok(Cow::Borrowed(segment.bytes(len)))
            }
            Self::Core => {
                let reply = conn
                    .get_image(format, pixmap, x, y, width, height, !0)?
                    .reply()?;
                if reply.data.len() < len {
                    return Err("short GetImage reply".into());
                }
                Ok(Cow::Owned(reply.data))
            }
        }
    }
}

pub struct Display {
    pub conn: Arc<RustConnection>,
    pub root: Window,
    pub screen_width: u16,
    pub screen_height: u16,
    pub atoms: Atoms,
    /// Depths whose `ZPixmap` images use 32 bits per pixel, the only layout tiles carry.
    bgrx_depths: Vec<u8>,
    region: xfixes::Region,
    grabber: Grabber,
    pub windows: BTreeMap<WindowId, Tracked>,
    /// The normal window that last received input; popups without `WM_TRANSIENT_FOR` belong
    /// to it.
    pub front: Option<WindowId>,
}

impl Display {
    /// Connects, checks extensions, redirects top-level windows and selects root events.
    /// Composite, DAMAGE, XFIXES and XTEST are required; MIT-SHM is used when it can pass fds.
    pub fn open(name: Option<&str>) -> XResult<Self> {
        let (conn, screen) = x11rb::connect(name)?;
        let conn = Arc::new(conn);
        for (extension, label) in [
            (composite::X11_EXTENSION_NAME, "Composite"),
            (damage::X11_EXTENSION_NAME, "DAMAGE"),
            (xfixes::X11_EXTENSION_NAME, "XFIXES"),
            (xtest::X11_EXTENSION_NAME, "XTEST"),
        ] {
            if conn.extension_information(extension)?.is_none() {
                return Err(format!("the X server lacks the {label} extension").into());
            }
        }
        // Each extension must be told our version before first use.
        conn.composite_query_version(0, 4)?.reply()?;
        conn.xfixes_query_version(5, 0)?.reply()?;
        conn.damage_query_version(1, 1)?.reply()?;
        conn.xtest_get_version(2, 2)?.reply()?;
        let shm = conn
            .extension_information(shm::X11_EXTENSION_NAME)?
            .is_some()
            && conn
                .shm_query_version()?
                .reply()
                .is_ok_and(|v| (v.major_version, v.minor_version) >= (1, 2));
        let grabber = if shm {
            Grabber::Shm(None)
        } else {
            Grabber::Core
        };

        let setup = conn.setup();
        if setup.image_byte_order != ImageOrder::LSB_FIRST {
            return Err("the X server uses MSB-first images".into());
        }
        let bgrx_depths = setup
            .pixmap_formats
            .iter()
            .filter(|f| f.bits_per_pixel == 32)
            .map(|f| f.depth)
            .collect();
        let screen = &setup.roots[screen];
        let (root, screen_width, screen_height) =
            (screen.root, screen.width_in_pixels, screen.height_in_pixels);

        let atoms = Atoms::new(&*conn)?.reply()?;
        let region = conn.generate_id()?;
        conn.xfixes_create_region(region, &[])?;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()?;
        conn.composite_redirect_subwindows(root, Redirect::AUTOMATIC)?
            .check()?;
        Ok(Self {
            conn,
            root,
            screen_width,
            screen_height,
            atoms,
            bgrx_depths,
            region,
            grabber,
            windows: BTreeMap::new(),
            front: None,
        })
    }

    /// Whether captures use MIT-SHM.
    pub fn uses_shm(&self) -> bool {
        matches!(self.grabber, Grabber::Shm(_))
    }

    /// Tracks windows that were mapped before the streamer started.
    pub fn scan(&mut self) -> XResult<Vec<Change>> {
        let children = self.conn.query_tree(self.root)?.reply()?.children;
        let mut changes = Vec::new();
        for window in children {
            self.watch_properties(window)?;
            let Ok(attributes) = self.conn.get_window_attributes(window)?.reply() else {
                continue;
            };
            if attributes.map_state == MapState::VIEWABLE
                && attributes.class == WindowClass::INPUT_OUTPUT
            {
                changes.extend(self.track(window, attributes.override_redirect)?);
            }
        }
        Ok(changes)
    }

    fn watch_properties(&self, window: Window) -> XResult<()> {
        let aux = ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE);
        self.conn.change_window_attributes(window, &aux)?;
        Ok(())
    }

    /// Applies one X event. Errors from requests on windows that vanished meanwhile are dropped.
    pub fn handle(&mut self, event: &Event) -> XResult<Option<Change>> {
        let root = self.root;
        Ok(match event {
            Event::CreateNotify(e) if e.parent == root => {
                self.watch_properties(e.window)?;
                None
            }
            Event::MapNotify(e) if e.event == root => self.track(e.window, e.override_redirect)?,
            Event::UnmapNotify(e) if e.event == root => self.untrack(e.window)?,
            Event::DestroyNotify(e) if e.event == root => self.untrack(e.window)?,
            Event::ReparentNotify(e) if e.event == root && e.parent != root => {
                self.untrack(e.window)?
            }
            Event::ConfigureNotify(e) if e.event == root => {
                let Some(tracked) = self.windows.get_mut(&WindowId(e.window)) else {
                    return Ok(None);
                };
                let (x, y) = content_origin(e.x, e.y, e.border_width);
                let resized = (tracked.info.width, tracked.info.height) != (e.width, e.height);
                if resized && !window_size_ok(e.width, e.height) {
                    return self.untrack(e.window);
                }
                if !resized && (tracked.info.x, tracked.info.y) == (x, y) {
                    return Ok(None);
                }
                tracked.info.x = x;
                tracked.info.y = y;
                tracked.border = e.border_width;
                if resized {
                    tracked.info.width = e.width;
                    tracked.info.height = e.height;
                    tracked.reset_frame();
                    // The server allocated a new backing pixmap for the new size.
                    self.conn.free_pixmap(tracked.pixmap)?;
                    tracked.pixmap = self.conn.generate_id()?;
                    self.conn
                        .composite_name_window_pixmap(e.window, tracked.pixmap)?;
                }
                Some(Change::Changed(tracked.info.clone()))
            }
            Event::PropertyNotify(e) if self.windows.contains_key(&WindowId(e.window)) => {
                let atoms = &self.atoms;
                let relevant = [
                    AtomEnum::WM_NAME.into(),
                    AtomEnum::WM_TRANSIENT_FOR.into(),
                    atoms._NET_WM_NAME,
                    atoms._NET_WM_PID,
                    atoms._SUPERPOSITION_RACK,
                    atoms._SUPERPOSITION_SLOT,
                ];
                if !relevant.contains(&e.atom) {
                    return Ok(None);
                }
                let id = WindowId(e.window);
                let old = &self.windows[&id].info;
                let mut info = old.clone();
                self.read_properties(&mut info)?;
                if info == *old {
                    return Ok(None);
                }
                self.windows.get_mut(&id).expect("checked above").info = info.clone();
                Some(Change::Changed(info))
            }
            Event::DamageNotify(e) => {
                if let Some(tracked) = self.windows.get_mut(&WindowId(e.drawable)) {
                    tracked.damaged = true;
                }
                None
            }
            _ => None,
        })
    }

    fn track(&mut self, window: Window, override_redirect: bool) -> XResult<Option<Change>> {
        let id = WindowId(window);
        if self.windows.contains_key(&id) || self.windows.len() >= MAX_WINDOWS {
            return Ok(None);
        }
        // The window may be gone again by now; that is not an error.
        let Ok(geometry) = self.conn.get_geometry(window)?.reply() else {
            return Ok(None);
        };
        if !window_size_ok(geometry.width, geometry.height)
            || !self.bgrx_depths.contains(&geometry.depth)
        {
            eprintln!(
                "sp-window-streamer: not streaming window {window:#x} ({}x{}, depth {})",
                geometry.width, geometry.height, geometry.depth
            );
            return Ok(None);
        }
        let (x, y) = content_origin(geometry.x, geometry.y, geometry.border_width);
        let mut info = WindowInfo {
            id,
            kind: if override_redirect {
                WindowKind::Popup
            } else {
                WindowKind::Normal
            },
            title: String::new(),
            x,
            y,
            width: geometry.width,
            height: geometry.height,
            rack: None,
            slot: None,
            pid: None,
            transient_for: None,
        };
        self.read_properties(&mut info)?;
        let damage = self.conn.generate_id()?;
        self.conn
            .damage_create(damage, window, ReportLevel::NON_EMPTY)?;
        let pixmap = self.conn.generate_id()?;
        self.conn.composite_name_window_pixmap(window, pixmap)?;
        let mut tracked = Tracked {
            info: info.clone(),
            border: geometry.border_width,
            damage,
            pixmap,
            frame: Vec::new(),
            grid: Vec::new(),
            tiles: Vec::new(),
            damaged: false,
            full: true,
            sequence: 0,
            last_capture: None,
        };
        tracked.reset_frame();
        self.windows.insert(id, tracked);
        Ok(Some(Change::Opened(info)))
    }

    fn untrack(&mut self, window: Window) -> XResult<Option<Change>> {
        let id = WindowId(window);
        let Some(tracked) = self.windows.remove(&id) else {
            return Ok(None);
        };
        // A destroyed window already freed its damage object; the error event is ignored.
        self.conn.damage_destroy(tracked.damage)?;
        self.conn.free_pixmap(tracked.pixmap)?;
        if self.front == Some(id) {
            self.front = None;
        }
        Ok(Some(Change::Closed(id)))
    }

    /// Re-reads title, rack, slot, pid and transient parent.
    fn read_properties(&self, info: &mut WindowInfo) -> XResult<()> {
        let conn = &*self.conn;
        let window = info.id.0;
        let get =
            |property, kind: u32, longs| conn.get_property(false, window, property, kind, 0, longs);
        let title_longs = u32::try_from(MAX_TITLE_BYTES.div_ceil(4))?;
        let net_name = get(self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING, title_longs)?;
        let name = get(AtomEnum::WM_NAME.into(), AtomEnum::ANY.into(), title_longs)?;
        let cardinal = AtomEnum::CARDINAL.into();
        let rack = get(self.atoms._SUPERPOSITION_RACK, cardinal, 1)?;
        let slot = get(self.atoms._SUPERPOSITION_SLOT, cardinal, 1)?;
        let pid = get(self.atoms._NET_WM_PID, cardinal, 1)?;
        let transient = get(
            AtomEnum::WM_TRANSIENT_FOR.into(),
            AtomEnum::WINDOW.into(),
            1,
        )?;

        let first_u32 = |reply: Result<x11rb::protocol::xproto::GetPropertyReply, _>| {
            reply
                .ok()
                .and_then(|r| r.value32().and_then(|mut v| v.next()))
        };
        let net_name = net_name.reply().ok().filter(|r| !r.value.is_empty());
        info.title = match net_name {
            Some(reply) => String::from_utf8_lossy(&reply.value).into_owned(),
            // WM_NAME is usually STRING, which is Latin-1.
            None => name
                .reply()
                .map(|r| r.value.iter().map(|&b| char::from(b)).collect())
                .unwrap_or_default(),
        };
        crate::truncate_at_char(&mut info.title, MAX_TITLE_BYTES);
        info.rack = first_u32(rack.reply());
        info.slot = first_u32(slot.reply());
        info.pid = first_u32(pid.reply());
        info.transient_for = first_u32(transient.reply())
            .map(WindowId)
            .filter(|parent| self.windows.contains_key(parent))
            .or(match info.kind {
                WindowKind::Popup => self.front,
                WindowKind::Normal => None,
            });
        Ok(())
    }

    /// Captures the damaged (or, after a resize, every) tile of a window and returns the tiles
    /// whose pixels changed.
    pub fn capture(
        &mut self,
        id: WindowId,
        encoder: &mut TileEncoder,
    ) -> XResult<Option<Captured>> {
        let Self {
            conn,
            region,
            grabber,
            windows,
            ..
        } = self;
        let Some(window) = windows.get_mut(&id) else {
            return Ok(None);
        };
        let keyframe = window.full;
        let mut damage = Vec::new();
        if window.damaged {
            // NON_EMPTY damage only reports again once the region is empty, so always subtract.
            if keyframe {
                conn.damage_subtract(window.damage, x11rb::NONE, x11rb::NONE)?;
            } else {
                conn.damage_subtract(window.damage, x11rb::NONE, *region)?;
                damage = conn.xfixes_fetch_region(*region)?.reply()?.rectangles;
            }
        }
        let dirty: Vec<usize> = (0..window.grid.len())
            .filter(|&i| keyframe || damage.iter().any(|&r| intersects(window.grid[i], r)))
            .collect();
        window.damaged = false;
        window.last_capture = Some(Instant::now());
        let Some(area) = dirty.iter().map(|&i| window.grid[i]).reduce(Rect::union) else {
            return Ok(None);
        };

        let border = window.border;
        let pixels = grabber.grab(
            conn,
            window.pixmap,
            (area.x + border, area.y + border),
            (area.width, area.height),
        )?;
        let frame_stride = usize::from(window.info.width) * 4;
        let area_stride = usize::from(area.width) * 4;
        let mut changed = Vec::new();
        for index in dirty {
            let tile = window.grid[index];
            let row_bytes = usize::from(tile.width) * 4;
            let mut differs = keyframe;
            for row in 0..usize::from(tile.height) {
                let src = (usize::from(tile.y - area.y) + row) * area_stride
                    + usize::from(tile.x - area.x) * 4;
                let dst = (usize::from(tile.y) + row) * frame_stride + usize::from(tile.x) * 4;
                let (src, dst) = (
                    &pixels[src..src + row_bytes],
                    &mut window.frame[dst..dst + row_bytes],
                );
                if src != dst {
                    dst.copy_from_slice(src);
                    differs = true;
                }
            }
            if differs {
                let fresh = encoder.encode(&window.frame, frame_stride, tile)?;
                if keyframe {
                    window.tiles.push(fresh.clone());
                } else {
                    window.tiles[index] = fresh.clone();
                }
                changed.push(fresh);
            }
        }
        window.full = false;
        if changed.is_empty() {
            return Ok(None);
        }
        window.sequence += 1;
        Ok(Some(Captured {
            tiles: changed,
            keyframe,
        }))
    }
}

fn content_origin(x: i16, y: i16, border: u16) -> (i16, i16) {
    let border = i16::try_from(border).unwrap_or(i16::MAX);
    (x.saturating_add(border), y.saturating_add(border))
}

fn window_size_ok(width: u16, height: u16) -> bool {
    (1..=MAX_WINDOW_DIMENSION).contains(&width) && (1..=MAX_WINDOW_DIMENSION).contains(&height)
}

fn intersects(tile: Rect, r: x11rb::protocol::xproto::Rectangle) -> bool {
    let (rx, ry) = (i32::from(r.x), i32::from(r.y));
    let (tx, ty) = (i32::from(tile.x), i32::from(tile.y));
    rx < tx + i32::from(tile.width)
        && tx < rx + i32::from(r.width)
        && ry < ty + i32::from(tile.height)
        && ty < ry + i32::from(r.height)
}
