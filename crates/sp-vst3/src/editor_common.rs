//! Editor-window pieces shared by the X11 (Linux) and Win32 (Windows under Wine) windows: the
//! window's rack identity and its 320×200 PNG preview picture.

/// Which rack slot an editor window belongs to, and what the window is called.
#[derive(Clone, Copy, Debug)]
pub struct EditorWindowIdentity<'a> {
    /// Window title. On Linux it is also `WM_NAME` and `_NET_WM_NAME`; Wine sets those from it.
    pub title: &'a str,
    /// Rack index. X11 windows carry it as the `_SUPERPOSITION_RACK` `CARDINAL` property.
    pub rack: u32,
    /// Zero-based slot index in the rack, the `_SUPERPOSITION_SLOT` property.
    pub slot: u32,
}

// Editor preview pictures are exactly 320×200 pixels.
pub(crate) const PREVIEW_WIDTH: usize = 320;
pub(crate) const PREVIEW_HEIGHT: usize = 200;

/// Packed 8-bit RGB pixels, row by row.
#[derive(Debug, PartialEq)]
pub(crate) struct Pixels {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) rgb: Vec<u8>,
}

impl Pixels {
    /// Scales the image to cover `width`×`height`, centred, cropping the overflow. Each output
    /// pixel averages the source pixels it covers, so large editors do not alias.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "image dimensions are far below f64's exact integer range; source indices are \
                  clamped to the image"
    )]
    pub(crate) fn cover(&self, width: usize, height: usize) -> Self {
        let scale = (width as f64 / self.width as f64).max(height as f64 / self.height as f64);
        // Source pixels per output pixel, and the centred source rectangle that stays visible.
        let step = 1.0 / scale;
        let left = (self.width as f64 - width as f64 * step) / 2.0;
        let top = (self.height as f64 - height as f64 * step) / 2.0;
        let span = |start: f64, limit: usize| {
            let first = (start.max(0.0) as usize).min(limit - 1);
            let end = ((start + step).ceil() as usize).clamp(first + 1, limit);
            first..end
        };
        let mut rgb = Vec::with_capacity(width * height * 3);
        for row in 0..height {
            let rows = span(top + row as f64 * step, self.height);
            for column in 0..width {
                let columns = span(left + column as f64 * step, self.width);
                let mut sum = [0_usize; 3];
                for source_row in rows.clone() {
                    let line = &self.rgb[source_row * self.width * 3..][..self.width * 3];
                    for pixel in line[columns.start * 3..columns.end * 3].chunks_exact(3) {
                        for (total, &value) in sum.iter_mut().zip(pixel) {
                            *total += usize::from(value);
                        }
                    }
                }
                let count = rows.len() * columns.len();
                rgb.extend(sum.map(|total| u8::try_from(total / count).unwrap_or(u8::MAX)));
            }
        }
        Self { width, height, rgb }
    }
}

pub(crate) fn encode_png(pixels: &Pixels) -> Result<Vec<u8>, String> {
    let failed = |error: png::EncodingError| format!("could not encode editor picture: {error}");
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(
        &mut png,
        u32::try_from(pixels.width).map_err(|_| "editor picture is too wide")?,
        u32::try_from(pixels.height).map_err(|_| "editor picture is too tall")?,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(failed)?;
    writer.write_image_data(&pixels.rgb).map_err(failed)?;
    writer.finish().map_err(failed)?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::{Pixels, encode_png};

    const RED: [u8; 3] = [255, 0, 0];
    const BLUE: [u8; 3] = [0, 0, 255];

    fn image(width: usize, height: usize, color: impl Fn(usize, usize) -> [u8; 3]) -> Pixels {
        let rgb = (0..height)
            .flat_map(|row| (0..width).map(move |column| (row, column)))
            .flat_map(|(row, column)| color(column, row))
            .collect();
        Pixels { width, height, rgb }
    }

    fn pixel(pixels: &Pixels, column: usize, row: usize) -> [u8; 3] {
        let start = (row * pixels.width + column) * 3;
        pixels.rgb[start..start + 3].try_into().expect("RGB pixel")
    }

    /// A downscaled editor keeps its layout: each half of the picture keeps its colour.
    #[test]
    fn cover_downscales_with_area_averaging() {
        let source = image(640, 400, |column, _| if column < 320 { RED } else { BLUE });
        let picture = source.cover(320, 200);
        assert_eq!((picture.width, picture.height), (320, 200));
        assert_eq!(pixel(&picture, 0, 0), RED);
        assert_eq!(pixel(&picture, 159, 199), RED);
        assert_eq!(pixel(&picture, 160, 0), BLUE);
        assert_eq!(pixel(&picture, 319, 199), BLUE);
    }

    /// A tall editor fills the picture's width and shows its vertical centre, like the macOS
    /// picture.
    #[test]
    fn cover_centre_crops_the_overflowing_axis() {
        let source = image(
            100,
            400,
            |_, row| if (100..300).contains(&row) { RED } else { BLUE },
        );
        let picture = source.cover(320, 200);
        assert!(picture.rgb.chunks_exact(3).all(|rgb| rgb == RED));
    }

    #[test]
    fn small_editors_scale_up_to_the_picture() {
        let source = image(32, 20, |column, _| if column < 16 { RED } else { BLUE });
        let picture = source.cover(320, 200);
        assert_eq!(pixel(&picture, 150, 100), RED);
        assert_eq!(pixel(&picture, 170, 100), BLUE);
    }

    #[test]
    fn pictures_encode_as_320_by_200_rgb_png() {
        let png = encode_png(&image(320, 200, |_, _| RED)).expect("PNG");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(png[16..20], 320_u32.to_be_bytes());
        assert_eq!(png[20..24], 200_u32.to_be_bytes());
    }
}
