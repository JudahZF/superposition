//! Paints an engine's frames into a local egui context and forwards local input.

use std::collections::HashMap;

use egui::{Context, LayerId, Shape, TextureHandle, TextureId};

use crate::{RemoteClient, protocol::WireMesh};

/// The client-side view of an engine UI: the engine's textures as local textures, and its last
/// frame's meshes.
pub struct RemoteView {
    textures: HashMap<TextureId, TextureHandle>,
    meshes: Vec<WireMesh>,
    /// The window size and scale last sent; input without events is sent only when they change.
    sent_layout: Option<(Option<egui::Rect>, Option<f32>)>,
}

impl Default for RemoteView {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteView {
    /// An empty view.
    #[must_use]
    pub fn new() -> Self {
        Self {
            textures: HashMap::new(),
            meshes: Vec::new(),
            sent_layout: None,
        }
    }

    /// Runs one local frame: sends this frame's input to the engine, applies received textures
    /// and the newest frame, and paints it. Call from `eframe::App::update`.
    ///
    /// The engine lays out for the window size and scale in this input, so both sides agree on
    /// coordinates.
    pub fn show(&mut self, ctx: &Context, client: &RemoteClient) {
        let input = ctx.input(|input| input.raw.clone());
        let layout = (input.screen_rect, input.viewport().native_pixels_per_point);
        // Idle frames send nothing, so an idle engine renders nothing.
        if !input.events.is_empty() || self.sent_layout != Some(layout) {
            self.sent_layout = Some(layout);
            client.send_input(input);
        }
        let (textures, frame) = client.take_received();
        for delta in textures {
            for (id, change) in delta.set {
                if let (Some(position), Some(handle)) = (change.pos, self.textures.get_mut(&id)) {
                    handle.set_partial(position, change.image, change.options);
                } else {
                    let handle =
                        ctx.load_texture(format!("engine-{id:?}"), change.image, change.options);
                    self.textures.insert(id, handle);
                }
            }
            for id in delta.free {
                self.textures.remove(&id);
            }
        }
        if let Some(frame) = frame {
            self.meshes = frame.meshes;
            ctx.set_cursor_icon(frame.cursor);
            if !frame.copied_text.is_empty() {
                ctx.copy_text(frame.copied_text);
            }
        }
        let painter = ctx.layer_painter(LayerId::background());
        for wire in &self.meshes {
            let Some(texture) = self.textures.get(&wire.mesh.texture_id) else {
                continue;
            };
            let mut mesh = wire.mesh.clone();
            mesh.texture_id = texture.id();
            painter
                .with_clip_rect(wire.clip_rect)
                .add(Shape::mesh(mesh));
        }
    }
}
