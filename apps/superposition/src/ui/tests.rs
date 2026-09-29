use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;
use sp_model::{
    MAX_SCENE_PARAMETER_VALUES, NormalizedParameters, NormalizedValue, ParameterId,
    PluginDescriptor, PluginFingerprint, PluginIdentity, PluginInstanceId, PluginSlot,
};
use sp_session::SessionController;

use super::{AUTOSAVE_INTERVAL, LiveRackApp, Modal, Overlay, mirror_update_matches};
#[cfg(target_os = "macos")]
use super::{ProductControl, SystemStatusState};
use crate::MirroredParameterUpdate;

fn unique_root(label: &str) -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "superposition-{label}-{}-{unique}",
        std::process::id()
    ))
}

fn empty_app(label: &str) -> LiveRackApp {
    LiveRackApp::new(
        SessionController::empty(unique_root(label)),
        false,
        Err("helpers unavailable in UI test".to_owned()),
    )
}

fn plugin_slot(id: &str, name: &str, parameters: usize) -> PluginSlot {
    PluginSlot {
        id: PluginInstanceId(id.to_owned()),
        plugin: PluginDescriptor {
            identity: PluginIdentity {
                vendor: "test".to_owned(),
                name: name.to_owned(),
                unique_id: name.to_owned(),
            },
            fingerprint: PluginFingerprint {
                algorithm: "sha256".to_owned(),
                digest: "test".to_owned(),
                plugin_version: "1".to_owned(),
            },
        },
        bypassed: false,
        sidechain: None,
        parameters: NormalizedParameters {
            values: (0..parameters)
                .map(|index| (ParameterId(index.to_string()), NormalizedValue(0.5)))
                .collect(),
        },
    }
}

fn large_plugin_scene_app() -> LiveRackApp {
    let mut app = empty_app("scene");
    app.add_rack();
    app.controller.document_mut().model.racks[0]
        .slots
        .push(plugin_slot("test-slot", "test", 713));
    app
}

/// Runs one headless UI frame with the given input events.
fn run_frame(ctx: &egui::Context, app: &mut LiveRackApp, events: Vec<egui::Event>) {
    let input = egui::RawInput {
        events,
        ..egui::RawInput::default()
    };
    let _ = ctx.run(input, |ctx| app.frame(ctx));
}

fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

/// Software-renders the show screen in its main states to PNGs for visual review. Opt-in: set
/// `SUPERPOSITION_UI_SNAPSHOT` to an output directory. No window or GPU is used.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "writes PNGs; set SUPERPOSITION_UI_SNAPSHOT to an output directory"]
#[allow(
    clippy::too_many_lines,
    reason = "one fixture show, stepped through each state"
)]
fn render_show_screen_snapshots() {
    let directory = std::path::PathBuf::from(
        std::env::var("SUPERPOSITION_UI_SNAPSHOT").expect("snapshot directory"),
    );
    std::fs::create_dir_all(&directory).expect("snapshot directory");
    let (mut app, ctx) = snapshot_fixture();
    let mut textures = std::collections::HashMap::new();
    let mut snapshot = |app: &mut LiveRackApp, size, path: &std::path::Path| {
        snapshot(&ctx, app, &mut textures, size, path);
    };
    snapshot(&mut app, (1920.0, 1080.0), &directory.join("show-1920.png"));
    app.overlay = Overlay::Route {
        rack: 1,
        at: egui::pos2(215.0, 110.0),
    };
    snapshot(
        &mut app,
        (1920.0, 1080.0),
        &directory.join("route-popover.png"),
    );
    app.overlay = Overlay::SlotMenu {
        rack: 0,
        slot: 1,
        at: egui::pos2(120.0, 240.0),
    };
    snapshot(&mut app, (1920.0, 1080.0), &directory.join("slot-menu.png"));
    app.overlay = Overlay::Sidechain {
        rack: 0,
        slot: 1,
        at: egui::pos2(120.0, 240.0),
    };
    snapshot(
        &mut app,
        (1920.0, 1080.0),
        &directory.join("sidechain-popover.png"),
    );
    app.overlay = Overlay::Picker(super::Picker {
        rack: 3,
        query: String::new(),
        cursor: 0,
    });
    snapshot(&mut app, (1920.0, 1080.0), &directory.join("picker.png"));
    app.overlay = Overlay::None;
    app.capture_scene();
    snapshot(
        &mut app,
        (1920.0, 1080.0),
        &directory.join("scene-capture.png"),
    );
    app.scene_editor = None;
    app.fault = Some(
        "Rack 3 Gtr worker exited. Restarting it with the last captured plug-in state; the rack passes dry meanwhile."
            .to_owned(),
    );
    snapshot(
        &mut app,
        (1512.0, 982.0),
        &directory.join("laptop-fault.png"),
    );
    app.fault = None;
    app.screen = super::Screen::Setup;
    app.setup_tab = super::SetupTab::Diagnostics;
    snapshot(
        &mut app,
        (1920.0, 1080.0),
        &directory.join("setup-diagnostics.png"),
    );
    app.screen = super::Screen::Show;
    app.system.state = SystemStatusState::Offline;
    app.overlay = Overlay::Modal(Modal::Recovery);
    snapshot(
        &mut app,
        (1920.0, 1080.0),
        &directory.join("offline-recovery.png"),
    );
    app.system.state = SystemStatusState::Online;
    app.overlay = Overlay::None;
    for _ in 0..4 {
        app.add_rack();
    }
    app.select_rack(0);
    app.new_page(Some(0));
    for rack in [1, 3, 4] {
        app.toggle_rack_page(rack, 0);
    }
    app.new_page(Some(6));
    app.toggle_rack_page(2, 1);
    app.show_page(None);
    app.status_line = "Session loaded".to_owned();
    snapshot(
        &mut app,
        (1920.0, 1080.0),
        &directory.join("twelve-racks.png"),
    );
    app.show_page(Some(0));
    snapshot(&mut app, (1920.0, 1080.0), &directory.join("page.png"));
    app.overlay = Overlay::RackPages {
        rack: 0,
        at: egui::pos2(20.0, 140.0),
    };
    snapshot(
        &mut app,
        (1920.0, 1080.0),
        &directory.join("rack-pages.png"),
    );
    app.overlay = Overlay::None;
    app.show_page(None);
    app.controller.document_mut().model.pages.clear();
    app.controller.document_mut().model.racks.truncate(3);
    snapshot(
        &mut app,
        (1920.0, 1080.0),
        &directory.join("three-racks.png"),
    );
    let mut empty = empty_app("snapshot-empty");
    snapshot(&mut empty, (1920.0, 1080.0), &directory.join("empty.png"));
}

/// Eight racks resembling a show, with generated editor pictures and live meters.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_lines, reason = "one fixture show, built in order")]
fn snapshot_fixture() -> (LiveRackApp, egui::Context) {
    const RACKS: [(&str, &[&str]); 8] = [
        ("Vox", &["Pro-Q 3", "Pro-C 2", "Soothe2", "VintageVerb"]),
        ("Keys", &["Saturn 2", "Pro-L 2"]),
        ("Gtr", &["Amp Room", "Pro-Q 3", "Supermassive"]),
        ("Bass", &["Pro-C 2", "Bass Rider"]),
        ("Drums", &["Ozone 11", "Pro-L 2"]),
        ("FX", &["Supermassive"]),
        (
            "Synth",
            &[
                "Pro-Q 3",
                "Saturn 2",
                "Pro-C 2",
                "Soothe2",
                "VintageVerb",
                "Supermassive",
                "Pro-L 2",
                "Amp Room",
            ],
        ),
        ("Click", &["Pro-L 2"]),
    ];
    let ctx = egui::Context::default();
    super::install_style(&ctx);
    let mut app = empty_app("snapshot");
    for (index, (name, plugins)) in RACKS.iter().enumerate() {
        app.add_rack();
        let rack = &mut app.controller.document_mut().model.racks[index];
        (*name).clone_into(&mut rack.name);
        for (slot, plugin) in plugins.iter().enumerate() {
            rack.slots
                .push(plugin_slot(&format!("{name}-{slot}"), plugin, 4));
        }
    }
    let model = &mut app.controller.document_mut().model;
    model.racks[0].gain_db = sp_model::GainDb::new(-3.0).expect("gain");
    model.racks[3].bypassed = true;
    model.racks[3].slots[0].bypassed = true;
    model.racks[4].gain_db = sp_model::GainDb::new(-6.0).expect("gain");
    model.racks[5].muted = true;
    model.racks[5].gain_db = sp_model::GainDb::new(-12.0).expect("gain");
    model.racks[0].slots[1].sidechain = Some(sp_model::SlotSidechain::RackOutput(
        model.racks[4].id.clone(),
    ));
    model.racks[6].slots[2].sidechain = Some(sp_model::SlotSidechain::PhysicalInput(
        sp_model::PhysicalChannels::Stereo { left: 6, right: 7 },
    ));
    app.catalog_plugins = model
        .racks
        .iter()
        .flat_map(|rack| &rack.slots)
        .filter(|slot| slot.plugin.identity.name != "Ozone 11")
        .map(|slot| crate::CatalogPlugin {
            descriptor: slot.plugin.clone(),
            parameters: slot.parameters.clone(),
            sidechain_capable: slot.plugin.identity.name.starts_with("Pro-"),
        })
        .collect();
    app.catalog_plugins
        .sort_by(|a, b| a.descriptor.identity.name.cmp(&b.descriptor.identity.name));
    app.catalog_plugins
        .dedup_by(|a, b| a.descriptor.identity.name == b.descriptor.identity.name);
    for (index, name) in ["Intro", "Verse", "Chorus", "Bridge", "Outro"]
        .into_iter()
        .enumerate()
    {
        let scene = LiveRackApp::snapshot_scene(
            &app.controller.document().model,
            sp_model::SceneId(format!("scene-{index}")),
            name.to_owned(),
            250,
            &[],
        )
        .expect("scene");
        app.controller.document_mut().model.scenes.push(scene);
    }
    app.current_scene = Some(2);
    app.selected_rack = 0;
    app.selected_slot = Some(1);
    app.system.state = SystemStatusState::Online;
    app.test_workers_running = true;
    app.status_line = "Session loaded".to_owned();
    app.load_history = [
        0.2, 0.25, 0.3, 0.24, 0.2, 0.26, 0.95, 0.24, 0.2, 0.21, 0.23, 0.25,
    ];
    for rack in 0..8 {
        #[allow(clippy::cast_precision_loss, reason = "eight racks")]
        let level = 0.25 + rack as f32 * 0.09;
        app.rack_input_meters[rack].update([level * 0.8, level * 0.7], false, Duration::ZERO);
        app.rack_output_meters[rack].update([level, level * 0.94], level > 0.85, Duration::ZERO);
    }
    let now_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("time fits");
    let slots: Vec<(String, String)> = app
        .controller
        .document()
        .model
        .racks
        .iter()
        .flat_map(|rack| &rack.slots)
        .map(|slot| (slot.id.0.clone(), slot.plugin.identity.name.clone()))
        .collect();
    for (index, (id, name)) in slots.into_iter().enumerate() {
        // Leave a few slots never opened.
        if index % 7 == 3 {
            continue;
        }
        let age_ms = [120_000, 3_600_000, 240_000, 9 * 86_400_000][index % 4];
        app.previews.insert(
            &ctx,
            sp_session::EditorPreviewFile {
                instance_id: id,
                png: editor_picture(&name),
                captured_at_unix_ms: now_ms - age_ms,
            },
        );
    }
    app.previews.mark_loaded();
    (app, ctx)
}

/// A stand-in editor picture: a coloured panel with a curve and bars, like a plug-in GUI.
#[cfg(target_os = "macos")]
fn editor_picture(name: &str) -> Vec<u8> {
    let seed = name.bytes().fold(7_u32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    let channel = |shift: u32, floor: u32| u8::try_from(floor + (seed >> shift) % 40).unwrap_or(0);
    let background = image::Rgba([channel(0, 20), channel(5, 24), channel(10, 30), 255]);
    let accent = image::Rgba([channel(3, 180), channel(8, 140), channel(13, 60), 255]);
    let mut picture = image::RgbaImage::from_pixel(320, 200, background);
    for x in 0..320_u32 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a curve inside a 200 px tall picture"
        )]
        let y = (110.0 + 40.0 * (f64::from(x) / 40.0 + f64::from(seed % 13)).sin()) as u32;
        for thickness in 0..3 {
            picture.put_pixel(x, (y + thickness).min(199), accent);
        }
        if x % 24 < 12 {
            let height = 20 + (x * 7 + seed) % 60;
            for y in 200 - height..196 {
                let pixel = picture.get_pixel_mut(x, y);
                pixel.0[0] = pixel.0[0].saturating_add(30);
                pixel.0[1] = pixel.0[1].saturating_add(30);
                pixel.0[2] = pixel.0[2].saturating_add(30);
            }
        }
    }
    for x in 0..320 {
        for y in 0..16 {
            picture.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
        }
    }
    let mut png = Vec::new();
    picture
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encode picture");
    png
}

/// Renders frames until textures settle, then rasterizes the last one to `path`.
#[cfg(target_os = "macos")]
fn snapshot(
    ctx: &egui::Context,
    app: &mut LiveRackApp,
    textures: &mut std::collections::HashMap<egui::TextureId, egui::ColorImage>,
    (width, height): (f32, f32),
    path: &std::path::Path,
) {
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(width, height),
        )),
        ..egui::RawInput::default()
    };
    let mut output = ctx.run(input(), |ctx| app.frame(ctx));
    for _ in 0..2 {
        apply_textures(textures, &output.textures_delta);
        output = ctx.run(input(), |ctx| app.frame(ctx));
    }
    apply_textures(textures, &output.textures_delta);
    let primitives = ctx.tessellate(output.shapes, 1.0);

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "fixed test size"
    )]
    let (w, h) = (width as usize, height as usize);
    let mut pixels = vec![[0.0_f32; 4]; w * h];
    for clipped in &primitives {
        let egui::epaint::Primitive::Mesh(mesh) = &clipped.primitive else {
            continue;
        };
        let texture = textures.get(&mesh.texture_id);
        for triangle in mesh.indices.chunks_exact(3) {
            let v = [
                mesh.vertices[triangle[0] as usize],
                mesh.vertices[triangle[1] as usize],
                mesh.vertices[triangle[2] as usize],
            ];
            raster_triangle(&mut pixels, w, h, clipped.clip_rect, &v, texture);
        }
    }
    let mut image = image::RgbaImage::new(
        u32::try_from(w).expect("width"),
        u32::try_from(h).expect("height"),
    );
    for (index, pixel) in pixels.iter().enumerate() {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "0..=255"
        )]
        let rgba = pixel.map(|channel| (channel.clamp(0.0, 1.0) * 255.0).round() as u8);
        image.put_pixel(
            u32::try_from(index % w).expect("x"),
            u32::try_from(index / w).expect("y"),
            image::Rgba([rgba[0], rgba[1], rgba[2], 255]),
        );
    }
    image.save(path).expect("write snapshot");
}

#[cfg(target_os = "macos")]
fn apply_textures(
    textures: &mut std::collections::HashMap<egui::TextureId, egui::ColorImage>,
    delta: &egui::TexturesDelta,
) {
    for (id, update) in &delta.set {
        let image = match &update.image {
            egui::ImageData::Color(image) => (**image).clone(),
            egui::ImageData::Font(font) => egui::ColorImage {
                size: font.size,
                pixels: font.srgba_pixels(None).collect(),
            },
        };
        match update.pos {
            None => {
                textures.insert(*id, image);
            }
            Some([x, y]) => {
                let target = textures.get_mut(id).expect("patched texture exists");
                for row in 0..image.size[1] {
                    for column in 0..image.size[0] {
                        target.pixels[(y + row) * target.size[0] + x + column] =
                            image.pixels[row * image.size[0] + column];
                    }
                }
            }
        }
    }
}

/// Fills one textured, vertex-coloured triangle with premultiplied-alpha blending in
/// gamma space, matching egui's default renderer closely enough for visual review.
#[cfg(target_os = "macos")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::many_single_char_names,
    reason = "pixel coordinates in a fixed test raster"
)]
fn raster_triangle(
    pixels: &mut [[f32; 4]],
    width: usize,
    height: usize,
    clip: egui::Rect,
    v: &[egui::epaint::Vertex; 3],
    texture: Option<&egui::ColorImage>,
) {
    let (a, b, c) = (v[0].pos, v[1].pos, v[2].pos);
    let area = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    if area.abs() < 1e-6 {
        return;
    }
    let min_x = a.x.min(b.x).min(c.x).max(clip.min.x).max(0.0).floor() as usize;
    let max_x = (a.x.max(b.x).max(c.x).min(clip.max.x).ceil() as usize).min(width);
    let min_y = a.y.min(b.y).min(c.y).max(clip.min.y).max(0.0).floor() as usize;
    let max_y = (a.y.max(b.y).max(c.y).min(clip.max.y).ceil() as usize).min(height);
    let rgba = |color: egui::Color32| color.to_array().map(|channel| f32::from(channel) / 255.0);
    let colors = v.map(|vertex| rgba(vertex.color));
    for y in min_y..max_y {
        for x in min_x..max_x {
            let point = egui::pos2(x as f32 + 0.5, y as f32 + 0.5);
            let w0 = ((b.x - point.x) * (c.y - point.y) - (b.y - point.y) * (c.x - point.x)) / area;
            let w1 = ((c.x - point.x) * (a.y - point.y) - (c.y - point.y) * (a.x - point.x)) / area;
            let w2 = 1.0 - w0 - w1;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let mut color = [0.0; 4];
            for channel in 0..4 {
                color[channel] =
                    colors[0][channel] * w0 + colors[1][channel] * w1 + colors[2][channel] * w2;
            }
            if let Some(texture) = texture {
                let u = v[0].uv.x * w0 + v[1].uv.x * w1 + v[2].uv.x * w2;
                let t = v[0].uv.y * w0 + v[1].uv.y * w1 + v[2].uv.y * w2;
                let tx = ((u * texture.size[0] as f32) as usize).min(texture.size[0] - 1);
                let ty = ((t * texture.size[1] as f32) as usize).min(texture.size[1] - 1);
                let texel = rgba(texture.pixels[ty * texture.size[0] + tx]);
                for channel in 0..4 {
                    color[channel] *= texel[channel];
                }
            }
            let destination = &mut pixels[y * width + x];
            for channel in 0..3 {
                destination[channel] = color[channel] + destination[channel] * (1.0 - color[3]);
            }
        }
    }
}

/// A reconnected device often returns with a new `CoreAudio` ID; the name must still match.
#[cfg(target_os = "macos")]
#[test]
fn saved_route_finds_a_returned_device_by_name() {
    use sp_audio_io::{AudioDeviceCapabilities, AudioDeviceId, AudioDeviceInfo};
    use sp_model::{AudioDeviceSelection, AudioDeviceSettings};

    let device = |id: &str, name: &str| super::MacOsAudioDevice {
        info: AudioDeviceInfo {
            id: AudioDeviceId::new(id),
            name: name.to_owned(),
            max_output_channels: 2,
        },
        capabilities: AudioDeviceCapabilities {
            max_input_channels: 2,
            max_output_channels: 2,
            is_default_input: false,
            is_default_output: false,
        },
        supported_buffer_frames: vec![128],
    };
    let settings = AudioDeviceSettings {
        input: Some(AudioDeviceSelection {
            id: "coreaudio:10".to_owned(),
            name: "Stage Interface".to_owned(),
        }),
        output: AudioDeviceSelection {
            id: "coreaudio:10".to_owned(),
            name: "Stage Interface".to_owned(),
        },
        buffer_frames: 128,
    };

    assert!(
        super::saved_route(&[], &settings).is_none(),
        "missing device waits"
    );
    let returned = [device("coreaudio:42", "Stage Interface")];
    let route = super::saved_route(&returned, &settings).expect("device returned");
    assert_eq!(route.output.as_str(), "coreaudio:42");
    assert_eq!(
        route.input.as_ref().map(AudioDeviceId::as_str),
        Some("coreaudio:42")
    );
    assert_eq!(route.format.max_frames_per_callback, 128);
}

#[cfg(target_os = "macos")]
#[test]
fn reconnect_waits_for_its_interval_and_yields_to_manual_control() {
    let mut app = large_plugin_scene_app();
    let now = Instant::now();

    // Not due yet: nothing changes.
    app.reconnect_at = Some(now + Duration::from_secs(1));
    app.poll_reconnect(now);
    assert_eq!(app.reconnect_at, Some(now + Duration::from_secs(1)));
    assert_eq!(app.system.state, SystemStatusState::Offline);

    // Due, but the session has no saved route: stay offline and retry later.
    app.poll_reconnect(now + Duration::from_secs(1));
    assert_eq!(app.system.state, SystemStatusState::Offline);
    assert!(
        app.reconnect_at
            .is_some_and(|due| due > now + Duration::from_secs(1))
    );

    // A manual start already brought audio back: the automatic attempt ends.
    app.system.state = SystemStatusState::Online;
    app.poll_reconnect(now + Duration::from_secs(5));
    assert_eq!(app.reconnect_at, None);
}

#[cfg(target_os = "macos")]
#[test]
fn show_controls_guard_audio_and_recall_scenes_by_number() {
    let mut app = large_plugin_scene_app();
    let model = app.controller.document().model.clone();
    let scene = LiveRackApp::snapshot_scene(
        &model,
        sp_model::SceneId("scene-1".to_owned()),
        "Verse".to_owned(),
        0,
        &[],
    )
    .expect("scene snapshot");
    app.controller.document_mut().model.scenes.push(scene);
    let (control, receiver) = ProductControl::new();
    app.product_control = Some(control);
    app.system.state = SystemStatusState::Online;
    let ctx = egui::Context::default();
    super::install_style(&ctx);

    // A close request while online is cancelled and asks first.
    let close = egui::RawInput {
        viewports: std::iter::once((
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                events: vec![egui::ViewportEvent::Close],
                ..egui::ViewportInfo::default()
            },
        ))
        .collect(),
        ..egui::RawInput::default()
    };
    let output = ctx.run(close, |ctx| app.frame(ctx));
    assert_eq!(app.overlay, Overlay::Modal(Modal::Quit));
    assert!(
        output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| viewport
                .commands
                .contains(&egui::ViewportCommand::CancelClose)),
        "quit while online must be cancelled until confirmed"
    );

    // Escape keeps the show running.
    run_frame(
        &ctx,
        &mut app,
        vec![key(egui::Key::Escape, egui::Modifiers::NONE)],
    );
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.system.state, SystemStatusState::Online);

    // Key 1 recalls scene 1 through the live control queue.
    run_frame(
        &ctx,
        &mut app,
        vec![key(egui::Key::Num1, egui::Modifiers::NONE)],
    );
    assert_eq!(app.current_scene, Some(0));
    assert!(
        receiver.has_pending_command(),
        "scene trigger reached the engine queue"
    );
}

/// Escape closes overlays in the brief's order: popover, then fault line, then setup page.
#[test]
fn escape_closes_the_topmost_overlay_first() {
    let mut app = empty_app("escape");
    app.add_rack();
    app.screen = super::Screen::Setup;
    app.fault = Some("fault".to_owned());
    app.overlay = Overlay::Route {
        rack: 0,
        at: egui::Pos2::ZERO,
    };
    assert!(app.close_topmost());
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.fault.is_some());
    assert!(app.close_topmost());
    assert!(app.fault.is_none());
    assert_eq!(app.screen, super::Screen::Setup);
    assert!(app.close_topmost());
    assert_eq!(app.screen, super::Screen::Show);
    assert!(!app.close_topmost());
}

#[test]
fn added_racks_get_a_valid_route_with_an_input() {
    let mut app = empty_app("rack-route");

    app.add_rack();
    app.add_rack();

    let session = &app.controller.document().model;
    assert_eq!(session.racks.len(), 2);
    assert_eq!(session.racks[0].source_id.0, "input-1");
    assert_eq!(session.racks[0].endpoint_id.0, "output-1");
    assert!(
        session
            .racks
            .iter()
            .all(|rack| session.rack_routes[&rack.id].input.is_some()),
        "the UI never creates a rack without an audio input"
    );
    session.validate().expect("rack routes are valid");
}

/// Pages filter the columns in session order; moves step past hidden racks, new racks join
/// the shown page, and removed racks leave every page.
#[test]
fn pages_filter_racks_and_follow_rack_edits() {
    let mut app = empty_app("pages");
    for _ in 0..4 {
        app.add_rack();
    }
    app.select_rack(0);
    app.new_page(Some(0));
    app.toggle_rack_page(3, 0);
    assert_eq!(app.visible_racks(), [0, 3]);

    app.select_rack(3);
    app.move_selected_rack(-1);
    let ids: Vec<String> = app
        .controller
        .document()
        .model
        .racks
        .iter()
        .map(|rack| rack.id.0.clone())
        .collect();
    assert_eq!(ids, ["rack-4", "rack-1", "rack-2", "rack-3"]);
    assert_eq!(app.visible_racks(), [0, 1], "the page keeps both racks");

    app.add_rack();
    assert_eq!(
        app.visible_racks(),
        [0, 1, 4],
        "a rack added on a page joins it"
    );

    app.selected_rack = 0;
    app.remove_selected_rack();
    let model = &app.controller.document().model;
    assert!(model.pages[0].racks.iter().all(|id| id.0 != "rack-4"));
    model.validate().expect("pages drop removed racks");
    app.show_page(None);
    assert_eq!(app.visible_racks().len(), 4);
}

#[test]
fn stale_parameter_feedback_does_not_match_replaced_slot_identity() {
    let mut app = large_plugin_scene_app();
    let model = &app.controller.document().model;
    let slot = &model.racks[0].slots[0];
    let update = MirroredParameterUpdate {
        rack_index: 0,
        slot_index: 0,
        rack_id: model.racks[0].id.clone(),
        slot_id: slot.id.clone(),
        fingerprint: slot.plugin.fingerprint.digest.clone(),
        class_id: slot.plugin.identity.unique_id.clone(),
        bank_generation: 2,
        parameter_id: 7,
        value: NormalizedValue(0.8),
    };
    assert!(mirror_update_matches(model, &update));
    app.controller.document_mut().model.racks[0].slots[0].id =
        PluginInstanceId("replacement".to_owned());
    assert!(!mirror_update_matches(
        &app.controller.document().model,
        &update
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn stale_native_observation_does_not_follow_a_newer_host_value() {
    let mut app = large_plugin_scene_app();
    let (control, _receiver) = ProductControl::new();
    app.product_control = Some(control);
    app.pending_parameter_observations.insert((0, 0, 7), 0.2);
    // The model's 0.5 is newer than this unsent observation.
    app.flush_parameter_observations();
    assert!(app.pending_parameter_observations.is_empty());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one session lifecycle covers sparse capture, update, and deletion references"
)]
fn scene_capture_and_update_keep_sparse_scope_for_large_plugins() {
    let mut app = large_plugin_scene_app();
    let before = app.controller.document().model.clone();

    app.capture_scene();

    assert_eq!(app.controller.document().model, before);
    let draft = app
        .scene_editor
        .take()
        .expect("capture opens a selection draft");
    assert!(draft.selected_scene().parameter_values.is_empty());
    let mut scope = draft.selected_scene();
    scope.parameter_values.push(sp_model::SceneParameterValue {
        rack_id: before.racks[0].id.clone(),
        slot_id: before.racks[0].slots[0].id.clone(),
        parameter_id: ParameterId("712".to_owned()),
        value: NormalizedValue(0.0),
        transition: sp_model::SceneParameterTransition::Ramp,
    });
    assert!(!app.commit_scene(scope.clone(), None));
    assert_eq!(app.controller.document().model, before);
    assert!(
        app.fault
            .as_deref()
            .is_some_and(|fault| fault.contains("workers are unavailable"))
    );
    let captured = LiveRackApp::snapshot_scene(
        &before,
        scope.id,
        scope.name,
        scope.transition_ms,
        &scope.parameter_values,
    )
    .expect("sparse model snapshot");
    app.controller.document_mut().model.scenes.push(captured);
    app.current_scene = Some(0);
    let model = &app.controller.document().model;
    assert_eq!(model.racks[0].slots[0].parameters.values.len(), 713);
    assert_eq!(model.scenes[0].parameter_values.len(), 1);
    assert_eq!(
        model.scenes[0].parameter_values[0].value,
        NormalizedValue(0.5)
    );
    assert_eq!(model.scenes[0].rack_bypasses.len(), 1);

    app.controller.document_mut().model.racks[0].slots[0]
        .parameters
        .values
        .insert(ParameterId("712".to_owned()), NormalizedValue(0.8));
    let model = &app.controller.document().model;
    let scope = &model.scenes[0];
    let updated = LiveRackApp::snapshot_scene(
        model,
        scope.id.clone(),
        scope.name.clone(),
        scope.transition_ms,
        &scope.parameter_values,
    )
    .expect("update keeps existing scope");
    assert_eq!(updated.parameter_values.len(), 1);
    assert_eq!(updated.parameter_values[0].parameter_id.0, "712");
    assert_eq!(updated.parameter_values[0].value, NormalizedValue(0.8));

    let mut too_many = updated.clone();
    too_many.parameter_values =
        vec![updated.parameter_values[0].clone(); MAX_SCENE_PARAMETER_VALUES + 1];
    let before = app.controller.document().model.clone();
    assert!(!app.commit_scene(too_many, Some(0)));
    assert_eq!(app.controller.document().model, before);
    assert!(
        app.fault
            .as_deref()
            .is_some_and(|fault| fault.contains("maximum is 256"))
    );
    app.controller
        .document_mut()
        .model
        .midi_mappings
        .push(sp_model::MidiMapping {
            id: sp_model::MidiMappingId("test-midi".to_owned()),
            source: sp_model::MidiController {
                channel: 1,
                controller: 1,
            },
            target: sp_model::ParameterAddress {
                rack_id: before.racks[0].id.clone(),
                slot_id: before.racks[0].slots[0].id.clone(),
                parameter_id: ParameterId("712".to_owned()),
            },
            minimum: NormalizedValue(0.0),
            maximum: NormalizedValue(1.0),
        });
    app.remove_slot(0);
    let model = &app.controller.document().model;
    model
        .validate()
        .expect("slot removal cleans scene and MIDI references");
    assert!(model.scenes[0].parameter_values.is_empty());
    assert!(model.scenes[0].bypasses.is_empty());
    assert!(model.midi_mappings.is_empty());
    app.remove_selected_rack();
    let model = &app.controller.document().model;
    model
        .validate()
        .expect("rack removal cleans scene and route references");
    assert!(model.scenes[0].rack_bypasses.is_empty());
}

/// A new capture preselects the first two automatable parameters of every plug-in.
#[test]
fn scene_capture_defaults_to_two_parameters_per_plugin() {
    let mut app = empty_app("scene-defaults");
    app.add_rack();
    app.controller.document_mut().model.racks[0]
        .slots
        .push(plugin_slot("defaults-slot", "test", 5));
    let metadata: Vec<sp_model::PluginParameterMetadata> = (0..5)
        .map(|id| sp_model::PluginParameterMetadata {
            id,
            name: format!("Parameter {id}"),
            short_name: String::new(),
            unit: String::new(),
            default_normalized: None,
            step_count: 0,
            automatable: id != 0,
            read_only: false,
            bypass: false,
        })
        .collect();
    let scene = LiveRackApp::snapshot_scene(
        &app.controller.document().model,
        sp_model::SceneId("scene-1".to_owned()),
        "Scene 1".to_owned(),
        250,
        &[],
    )
    .expect("scene");
    let editor =
        super::scenes::SceneEditor::new(&app.controller.document().model, scene, None, |_| {
            &metadata
        });
    let ids: Vec<String> = editor
        .selected_scene()
        .parameter_values
        .into_iter()
        .map(|value| value.parameter_id.0)
        .collect();
    assert_eq!(ids, ["1", "2"], "the first two automatable parameters");
}

#[test]
fn dirty_session_autosaves_on_deadline_and_reschedules() {
    let mut app = empty_app("autosave");
    app.add_rack();
    assert!(app.controller.is_dirty());
    let now = Instant::now();
    app.next_autosave_at = now
        .checked_sub(Duration::from_secs(1))
        .expect("past instant");

    app.autosave_if_due(now);

    assert!(!app.controller.is_dirty());
    assert!(app.next_autosave_at >= now + AUTOSAVE_INTERVAL);
}

#[cfg(target_os = "macos")]
#[test]
fn full_live_queue_does_not_commit_rack_controls_to_session() {
    let mut app = empty_app("rack-controls");
    app.add_rack();
    let before = app.controller.document().model.clone();
    let (mut control, _receiver) = ProductControl::new();
    while control.trigger_scene(usize::MAX) {}
    app.product_control = Some(control);
    app.system.set_state(SystemStatusState::Online);

    app.set_rack_controls(0, -12.0, true, true);

    assert_eq!(app.controller.document().model, before);
    assert!(
        app.fault
            .as_deref()
            .is_some_and(|fault| fault.contains("queue is full"))
    );
}
