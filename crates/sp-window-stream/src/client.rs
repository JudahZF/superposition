//! Client-side state: one RGBA8 framebuffer per streamed window, plus input helpers.

use std::{collections::BTreeMap, io};

use thiserror::Error;

use crate::message::{
    ClientId, ControlReason, FrameUpdate, Rect, ServerMessage, TileEncoding, WindowId, WindowInfo,
};

/// Above this many dirty rectangles, [`StreamedWindow::take_dirty`] returns their bounding box.
const MAX_DIRTY_RECTS: usize = 64;

/// A server message the client state cannot apply.
#[derive(Debug, Error)]
pub enum ApplyError {
    /// A frame or update names a window that was never opened.
    #[error("unknown window {0:?}")]
    UnknownWindow(WindowId),
    /// A tile does not decompress to exactly its rectangle's pixels.
    #[error("tile at {0:?} is corrupt")]
    CorruptTile(Rect),
}

/// One streamed window and its framebuffer.
pub struct StreamedWindow {
    info: WindowInfo,
    width: u16,
    height: u16,
    rgba: Vec<u8>,
    dirty: Vec<Rect>,
    complete: bool,
}

impl StreamedWindow {
    fn new(info: WindowInfo) -> Self {
        let mut window = Self {
            width: 0,
            height: 0,
            rgba: Vec::new(),
            dirty: Vec::new(),
            complete: false,
            info,
        };
        window.resize(window.info.width, window.info.height);
        window
    }

    fn resize(&mut self, width: u16, height: u16) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        self.width = width;
        self.height = height;
        self.rgba = [0, 0, 0, 255].repeat(usize::from(width) * usize::from(height));
        self.dirty = vec![Rect {
            x: 0,
            y: 0,
            width,
            height,
        }];
    }

    /// Latest window metadata.
    pub fn info(&self) -> &WindowInfo {
        &self.info
    }

    /// Framebuffer `[width, height]` in pixels, as egui's `ColorImage` expects.
    pub fn size(&self) -> [usize; 2] {
        [self.width.into(), self.height.into()]
    }

    /// Opaque RGBA8 pixels, rows top to bottom, `width * 4` bytes per row.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// The last applied frame message ended its frame. Uploading only then avoids showing a frame
    /// that is still arriving in parts.
    pub fn frame_complete(&self) -> bool {
        self.complete
    }

    /// Rectangles changed since the last call, merged where they touch along rows and columns.
    /// After a resize this is the whole window, so the texture must be reallocated.
    pub fn take_dirty(&mut self) -> Vec<Rect> {
        let mut rects = std::mem::take(&mut self.dirty);
        rects.sort_unstable_by_key(|r| (r.y, r.x));
        rects.dedup();
        let mut merged: Vec<Rect> = Vec::with_capacity(rects.len());
        // Join horizontal neighbours of the same row band, then identical spans stacked
        // vertically. Tiles arrive on a grid, so this rebuilds large changed areas.
        for rect in rects {
            match merged.last_mut() {
                Some(last)
                    if last.y == rect.y
                        && last.height == rect.height
                        && last.right() == u32::from(rect.x) =>
                {
                    *last = last.union(rect);
                }
                _ => merged.push(rect),
            }
        }
        let mut stacked: Vec<Rect> = Vec::with_capacity(merged.len());
        for rect in merged {
            match stacked
                .iter_mut()
                .find(|s| s.x == rect.x && s.width == rect.width && s.bottom() == u32::from(rect.y))
            {
                Some(span) => *span = span.union(rect),
                None => stacked.push(rect),
            }
        }
        if stacked.len() > MAX_DIRTY_RECTS {
            let bounds = stacked.iter().copied().reduce(Rect::union);
            return bounds.into_iter().collect();
        }
        let contains = |outer: &Rect, inner: &Rect| {
            outer.x <= inner.x
                && outer.y <= inner.y
                && outer.right() >= inner.right()
                && outer.bottom() >= inner.bottom()
        };
        let mut kept: Vec<Rect> = Vec::with_capacity(stacked.len());
        for rect in stacked {
            if !kept.iter().any(|k| contains(k, &rect)) {
                kept.retain(|k| !contains(&rect, k));
                kept.push(rect);
            }
        }
        kept
    }

    /// Copies `rect` out of the framebuffer as tightly packed RGBA8, for a partial texture upload.
    /// `rect` must lie inside the window, as the rectangles from [`Self::take_dirty`] do.
    pub fn copy_rect(&self, rect: Rect) -> Vec<u8> {
        let stride = usize::from(self.width) * 4;
        let row_bytes = usize::from(rect.width) * 4;
        let mut out = Vec::with_capacity(row_bytes * usize::from(rect.height));
        for row in usize::from(rect.y)..usize::from(rect.y) + usize::from(rect.height) {
            let start = row * stride + usize::from(rect.x) * 4;
            out.extend_from_slice(&self.rgba[start..start + row_bytes]);
        }
        out
    }
}

/// Applies [`ServerMessage`]s to per-window framebuffers. Transport-neutral: read messages with
/// [`crate::WireMessage::read_from`] from any byte stream and pass them to [`Self::apply`].
pub struct WindowStreamClient {
    client: Option<ClientId>,
    controller: Option<ClientId>,
    windows: BTreeMap<WindowId, StreamedWindow>,
    decompressor: zstd::bulk::Decompressor<'static>,
    scratch: Vec<u8>,
}

impl WindowStreamClient {
    /// Empty state with its own zstd context.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            client: None,
            controller: None,
            windows: BTreeMap::new(),
            decompressor: zstd::bulk::Decompressor::new()?,
            scratch: Vec::new(),
        })
    }

    /// This connection's ID, once welcomed.
    pub fn client_id(&self) -> Option<ClientId> {
        self.client
    }

    /// The client holding input control, if any.
    pub fn controller(&self) -> Option<ClientId> {
        self.controller
    }

    /// This connection holds input control; only then does the streamer accept input.
    pub fn has_control(&self) -> bool {
        self.client.is_some() && self.client == self.controller
    }

    /// Streamed windows in ID order.
    pub fn windows(&self) -> impl Iterator<Item = &StreamedWindow> {
        self.windows.values()
    }

    /// One window.
    pub fn window(&self, id: WindowId) -> Option<&StreamedWindow> {
        self.windows.get(&id)
    }

    /// One window, for [`StreamedWindow::take_dirty`].
    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut StreamedWindow> {
        self.windows.get_mut(&id)
    }

    /// Updates state from one message. Errors and `Denied` control notices change nothing here;
    /// the caller shows them from the message itself.
    pub fn apply(&mut self, message: &ServerMessage) -> Result<(), ApplyError> {
        match message {
            ServerMessage::Welcome { client, .. } => self.client = Some(*client),
            ServerMessage::WindowList(list) => {
                self.windows = list
                    .iter()
                    .map(|info| (info.id, StreamedWindow::new(info.clone())))
                    .collect();
            }
            ServerMessage::WindowOpened(info) => {
                self.windows
                    .insert(info.id, StreamedWindow::new(info.clone()));
            }
            ServerMessage::WindowChanged(info) => {
                let window = self
                    .windows
                    .get_mut(&info.id)
                    .ok_or(ApplyError::UnknownWindow(info.id))?;
                window.info = info.clone();
            }
            ServerMessage::WindowClosed(id) => {
                self.windows.remove(id);
            }
            ServerMessage::Frame(frame) => self.apply_frame(frame)?,
            ServerMessage::ControlStatus { holder, reason } => {
                if *reason != ControlReason::Denied {
                    self.controller = *holder;
                }
            }
            ServerMessage::Error { .. } => {}
        }
        Ok(())
    }

    fn apply_frame(&mut self, frame: &FrameUpdate) -> Result<(), ApplyError> {
        let window = self
            .windows
            .get_mut(&frame.window)
            .ok_or(ApplyError::UnknownWindow(frame.window))?;
        window.resize(frame.width, frame.height);
        window.complete = frame.complete;
        let stride = usize::from(window.width) * 4;
        for tile in &frame.tiles {
            // The codec already checked that the rectangle lies inside the frame.
            let rect = tile.rect;
            let expected = usize::from(rect.width) * usize::from(rect.height) * 4;
            self.scratch.clear();
            self.scratch.reserve(expected);
            match tile.encoding {
                TileEncoding::ZstdBgrx => {
                    let written = self
                        .decompressor
                        .decompress_to_buffer(&tile.data[..], &mut self.scratch)
                        .map_err(|_| ApplyError::CorruptTile(rect))?;
                    if written != expected {
                        return Err(ApplyError::CorruptTile(rect));
                    }
                }
            }
            let row_bytes = usize::from(rect.width) * 4;
            for (row, src) in self.scratch.chunks_exact(row_bytes).enumerate() {
                let start = (usize::from(rect.y) + row) * stride + usize::from(rect.x) * 4;
                for (dst, bgrx) in window.rgba[start..start + row_bytes]
                    .chunks_exact_mut(4)
                    .zip(src.chunks_exact(4))
                {
                    dst.copy_from_slice(&[bgrx[2], bgrx[1], bgrx[0], 255]);
                }
            }
            window.dirty.push(rect);
        }
        Ok(())
    }
}

/// Turns fractional scroll deltas (for example egui points) into whole notches for
/// [`crate::InputEvent::Scroll`], keeping the remainder.
pub struct ScrollAccumulator {
    units_per_notch: f32,
    x: f32,
    y: f32,
}

impl ScrollAccumulator {
    /// `units_per_notch` input units make one wheel notch.
    pub fn new(units_per_notch: f32) -> Self {
        Self {
            units_per_notch,
            x: 0.0,
            y: 0.0,
        }
    }

    /// Adds a delta (positive is down and right) and returns whole notches, if any.
    #[allow(clippy::cast_possible_truncation)] // Clamped to i16 first.
    pub fn push(&mut self, dx: f32, dy: f32) -> Option<(i16, i16)> {
        self.x += dx / self.units_per_notch;
        self.y += dy / self.units_per_notch;
        let notches = |v: &mut f32| {
            let whole = v.trunc().clamp(f32::from(i16::MIN), f32::from(i16::MAX));
            *v -= whole;
            whole as i16
        };
        let (x, y) = (notches(&mut self.x), notches(&mut self.y));
        (x != 0 || y != 0).then_some((x, y))
    }
}

/// The X keysym that types `c`: Latin-1 characters map directly, newline and tab map to `Return`
/// and `Tab`, others use the Unicode keysym range.
pub fn keysym_for_char(c: char) -> u32 {
    match u32::from(c) {
        0x0a | 0x0d => 0xff0d,
        0x09 => 0xff09,
        code @ (0x20..=0x7e | 0xa0..=0xff) => code,
        code => 0x0100_0000 | code,
    }
}
