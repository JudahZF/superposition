//! The engine's current textures, so a client that joins late can start from full images.
//!
//! egui sends each texture change once. The server applies every delta here and gives a new
//! client one delta with every live texture in full, followed by later deltas.

use std::{collections::HashMap, sync::Arc};

use egui::{
    ImageData, TextureId, TexturesDelta,
    epaint::{ImageDelta, textures::TextureOptions},
};

#[derive(Default)]
pub(crate) struct TextureMirror {
    images: HashMap<TextureId, (ImageData, TextureOptions)>,
}

impl TextureMirror {
    pub(crate) fn apply(&mut self, delta: &TexturesDelta) {
        for (id, change) in &delta.set {
            match (change.pos, self.images.get_mut(id)) {
                (Some(position), Some((image, options))) => {
                    patch(image, &change.image, position);
                    *options = change.options;
                }
                _ => {
                    self.images
                        .insert(*id, (change.image.clone(), change.options));
                }
            }
        }
        for id in &delta.free {
            self.images.remove(id);
        }
    }

    /// Every live texture as a full image.
    pub(crate) fn snapshot(&self) -> TexturesDelta {
        TexturesDelta {
            set: self
                .images
                .iter()
                .map(|(id, (image, options))| (*id, ImageDelta::full(image.clone(), *options)))
                .collect(),
            free: Vec::new(),
        }
    }
}

fn patch(image: &mut ImageData, change: &ImageData, [left, top]: [usize; 2]) {
    match (image, change) {
        (ImageData::Color(image), ImageData::Color(change)) => {
            let image = Arc::make_mut(image);
            copy_rows(
                &mut image.pixels,
                image.size,
                &change.pixels,
                change.size,
                left,
                top,
            );
        }
        (ImageData::Font(image), ImageData::Font(change)) => {
            copy_rows(
                &mut image.pixels,
                image.size,
                &change.pixels,
                change.size,
                left,
                top,
            );
        }
        (image, change) => *image = change.clone(),
    }
}

fn copy_rows<T: Copy>(
    pixels: &mut [T],
    [width, height]: [usize; 2],
    patch: &[T],
    [patch_width, patch_height]: [usize; 2],
    left: usize,
    top: usize,
) {
    let columns = patch_width.min(width.saturating_sub(left));
    for row in 0..patch_height.min(height.saturating_sub(top)) {
        let destination = (top + row) * width + left;
        let source = row * patch_width;
        pixels[destination..destination + columns]
            .copy_from_slice(&patch[source..source + columns]);
    }
}

#[cfg(test)]
mod tests {
    use egui::{
        Color32, ColorImage, ImageData, TextureId, TextureOptions, TexturesDelta,
        epaint::ImageDelta,
    };

    use super::TextureMirror;

    fn image(size: [usize; 2], color: Color32) -> ImageData {
        ImageData::Color(ColorImage::new(size, color).into())
    }

    #[test]
    fn a_late_client_receives_patched_full_images() {
        let id = TextureId::Managed(3);
        let mut mirror = TextureMirror::default();
        mirror.apply(&TexturesDelta {
            set: vec![(
                id,
                ImageDelta::full(image([4, 4], Color32::BLACK), TextureOptions::default()),
            )],
            free: vec![],
        });
        mirror.apply(&TexturesDelta {
            set: vec![(
                id,
                ImageDelta::partial(
                    [1, 2],
                    image([2, 1], Color32::WHITE),
                    TextureOptions::default(),
                ),
            )],
            free: vec![],
        });
        let snapshot = mirror.snapshot();
        let [(snapshot_id, delta)] = snapshot.set.as_slice() else {
            panic!("one texture");
        };
        assert_eq!(*snapshot_id, id);
        assert!(delta.pos.is_none());
        let ImageData::Color(full) = &delta.image else {
            panic!("color image");
        };
        let white: Vec<usize> = (0..16)
            .filter(|i| full.pixels[*i] == Color32::WHITE)
            .collect();
        assert_eq!(white, [9, 10]);

        mirror.apply(&TexturesDelta {
            set: vec![],
            free: vec![id],
        });
        assert!(mirror.snapshot().set.is_empty());
    }
}
