//! Tile compression for [`TileEncoding::ZstdBgrx`].

use std::{io, sync::Arc};

use crate::message::{Rect, Tile, TileEncoding};

/// Tile edge length the streamer uses. Edge tiles are smaller.
pub const TILE_SIZE: u16 = 64;

/// zstd level for tiles: the fastest standard level, since tiles are small and latency matters.
const ZSTD_LEVEL: i32 = 1;

/// Compresses BGRX tiles. Keep one per sender thread to reuse the zstd context.
pub struct TileEncoder {
    compressor: zstd::bulk::Compressor<'static>,
    scratch: Vec<u8>,
}

impl TileEncoder {
    /// A new encoder with its own zstd context.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            compressor: zstd::bulk::Compressor::new(ZSTD_LEVEL)?,
            scratch: Vec::new(),
        })
    }

    /// Compresses `rect` of a BGRX image whose rows are `stride` bytes apart. `image` must hold
    /// the rectangle; the caller owns the bounds.
    pub fn encode(&mut self, image: &[u8], stride: usize, rect: Rect) -> io::Result<Tile> {
        let row_bytes = usize::from(rect.width) * 4;
        self.scratch.clear();
        for row in 0..usize::from(rect.height) {
            let start = (usize::from(rect.y) + row) * stride + usize::from(rect.x) * 4;
            self.scratch
                .extend_from_slice(&image[start..start + row_bytes]);
        }
        let data = self.compressor.compress(&self.scratch)?;
        Ok(Tile {
            rect,
            encoding: TileEncoding::ZstdBgrx,
            data: Arc::from(data),
        })
    }
}

/// Splits a `width` x `height` area into [`TILE_SIZE`] tiles, row by row.
pub fn tile_grid(width: u16, height: u16) -> impl Iterator<Item = Rect> {
    (0..height).step_by(TILE_SIZE.into()).flat_map(move |y| {
        (0..width).step_by(TILE_SIZE.into()).map(move |x| Rect {
            x,
            y,
            width: TILE_SIZE.min(width - x),
            height: TILE_SIZE.min(height - y),
        })
    })
}

/// The [`tile_grid`] tiles in which two `width` x `height` images differ. Both images hold 4 bytes
/// per pixel with rows tightly packed; the caller checks their lengths.
pub fn changed_tiles<'a>(
    before: &'a [u8],
    after: &'a [u8],
    width: u16,
    height: u16,
) -> impl Iterator<Item = Rect> + 'a {
    let stride = usize::from(width) * 4;
    tile_grid(width, height).filter(move |rect| {
        let left = usize::from(rect.x) * 4;
        let row_bytes = usize::from(rect.width) * 4;
        let top = usize::from(rect.y);
        (top..top + usize::from(rect.height)).any(|row| {
            let start = row * stride + left;
            before[start..start + row_bytes] != after[start..start + row_bytes]
        })
    })
}
