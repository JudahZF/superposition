//! egui live-rack shell assembled from `sp-ui` component models and design tokens.

use crate::{CatalogPlugin, GenericParameter, ProductRuntime};
use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontId, Frame, Margin, RichText,
    Stroke, Ui, Vec2,
};
#[cfg(target_os = "macos")]
use sp_audio_io::{AudioEndpoint, AudioEndpointEvent, AudioFormat, AudioRouteConfig};
#[cfg(target_os = "macos")]
use sp_engine::PreparedGraph;
#[cfg(target_os = "macos")]
use sp_midi::{
    MidiCcMonitor, MidiLearnController, MidiMappingPublisher, MidiMappingTarget, MidiPortId,
    MidiPortInfo, MidirInput, map_controller_to_target,
};
use sp_model::{
    ChannelLayout, Endpoint, EndpointId, MAX_RACKS, MAX_SCENE_PARAMETER_VALUES, MAX_SLOTS_PER_RACK,
    MidiController, MidiMapping, MidiMappingId, NormalizedValue, ParameterAddress, ParameterId,
    PluginInstanceId, PluginSlot, Rack, RackGain, RackId, RackMute, RackTopology, Scene, SceneId,
    SceneParameterValue, SlotBypass, Source, SourceId,
};
use sp_session::CapturedPluginState;
use sp_session::SessionController;
use sp_ui::{
    component_gallery::ComponentGallery,
    components::{
        FaultBannerModel, FaultBannerState, ParameterControlState, PluginSlotState,
        PluginSlotStatus, RackCardState, RackCardStatus, ScenePadState, ScenePadStatus,
        SystemStatus, SystemStatusState, WorkerHealth, WorkerHealthState,
    },
    design::{
        BorderWidth, ButtonKind, ColorToken, Control, DARK, FontFamily as BrandFontFamily,
        FontWeight, Interaction, Layout, Radius, Spacing, Status, Surface, TextColor,
        TypographyRole, fonts,
    },
};

#[cfg(target_os = "macos")]
use sp_audio_io_macos::{
    MacOsAudioDevice, MacOsAudioEndpoint, ProductControl, ProductRenderer, ProductTelemetry,
    current_product_parameters, enumerate_devices, prepare_product_scenes, product_midi_mappings,
};

fn space(spacing: Spacing) -> f32 {
    f32::from(spacing.pixels())
}

fn dimension(layout: Layout) -> f32 {
    f32::from(layout.pixels())
}

fn text_size(role: TypographyRole) -> f32 {
    f32::from(role.typography().size())
}

fn font_family(family: BrandFontFamily, weight: FontWeight) -> egui::FontFamily {
    match family {
        BrandFontFamily::Monospace => egui::FontFamily::Monospace,
        BrandFontFamily::Interface | BrandFontFamily::Display => match weight {
            FontWeight::Regular => egui::FontFamily::Proportional,
            FontWeight::Medium => egui::FontFamily::Name("sora-medium".into()),
            FontWeight::Semibold => egui::FontFamily::Name("sora-semibold".into()),
            FontWeight::Bold => egui::FontFamily::Name("sora-bold".into()),
        },
    }
}

fn font_id(role: TypographyRole) -> FontId {
    let typography = role.typography();
    FontId::new(
        f32::from(typography.size()),
        font_family(typography.family(), typography.weight()),
    )
}

fn margin(horizontal: Spacing, vertical: Spacing) -> Margin {
    Margin::symmetric(
        i8::try_from(horizontal.pixels()).expect("spacing fits i8"),
        i8::try_from(vertical.pixels()).expect("spacing fits i8"),
    )
}

fn radius(radius: Radius) -> CornerRadius {
    CornerRadius::same(radius.pixels())
}

fn stroke(width: BorderWidth, color: Color32) -> Stroke {
    Stroke::new(f32::from(width.pixels()), color)
}

fn interaction_colors(interaction: Interaction) -> (Color32, Color32) {
    let colors = DARK.interaction_colors(interaction);
    (
        token_color(colors.foreground_token()),
        token_color(colors.background_token()),
    )
}

fn meter_fraction(peak: f32) -> f32 {
    if peak <= 0.0 || !peak.is_finite() {
        return 0.0;
    }
    ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Installs the bundled OFL Sora/Space Mono families and the token-driven widget style.
pub fn install_style(ctx: &egui::Context) {
    let mut definitions = FontDefinitions::default();
    for (name, bytes) in [
        ("sora", fonts::SORA_REGULAR),
        ("sora-medium", fonts::SORA_MEDIUM),
        ("sora-semibold", fonts::SORA_SEMIBOLD),
        ("sora-bold", fonts::SORA_BOLD),
        ("space-mono", fonts::SPACE_MONO_REGULAR),
    ] {
        definitions.font_data.insert(
            name.to_owned(),
            std::sync::Arc::new(FontData::from_static(bytes)),
        );
    }
    if let Some(family) = definitions
        .families
        .get_mut(&egui::FontFamily::Proportional)
    {
        family.insert(0, "sora".to_owned());
    }
    if let Some(family) = definitions.families.get_mut(&egui::FontFamily::Monospace) {
        family.insert(0, "space-mono".to_owned());
    }
    for name in ["sora-medium", "sora-semibold", "sora-bold"] {
        definitions.families.insert(
            egui::FontFamily::Name(name.into()),
            vec![name.to_owned(), "sora".to_owned()],
        );
    }
    ctx.set_fonts(definitions);

    let mut style = (*ctx.style()).clone();
    let canvas = token_color(ColorToken::Surface(Surface::Canvas));
    let panel = token_color(ColorToken::Surface(Surface::Panel));
    let raised = token_color(ColorToken::Surface(Surface::Raised));
    let steel = token_color(ColorToken::Surface(Surface::Steel));
    let primary = token_color(ColorToken::Text(TextColor::Primary));
    let secondary = token_color(ColorToken::Text(TextColor::Secondary));
    let focus = token_color(DARK.focus_color());
    let selected_boundary = token_color(DARK.selected_boundary_color());
    let (inactive_foreground, inactive_background) = interaction_colors(Interaction::Inactive);
    let (hovered_foreground, hovered_background) = interaction_colors(Interaction::Hovered);
    let (active_foreground, active_background) = interaction_colors(Interaction::Active);
    let (_, selected_background) = interaction_colors(Interaction::Selected);
    style.visuals = egui::Visuals::dark();
    style.spacing.item_spacing = Vec2::splat(space(Spacing::Sm));
    style.spacing.button_padding = Vec2::new(space(Spacing::Md), space(Spacing::Sm));
    style.spacing.interact_size.y = 28.0;
    let widget_radius = radius(Radius::Medium);
    style.visuals.widgets.noninteractive.corner_radius = widget_radius;
    style.visuals.widgets.inactive.corner_radius = widget_radius;
    style.visuals.widgets.hovered.corner_radius = widget_radius;
    style.visuals.widgets.active.corner_radius = widget_radius;
    style.visuals.widgets.open.corner_radius = widget_radius;
    style.visuals.slider_trailing_fill = true;
    style.visuals.panel_fill = panel;
    style.visuals.window_fill = raised;
    style.visuals.extreme_bg_color = canvas;
    style.visuals.faint_bg_color = panel;
    style.visuals.widgets.noninteractive.fg_stroke = stroke(BorderWidth::Thin, secondary);
    style.visuals.widgets.inactive.fg_stroke = stroke(BorderWidth::Thin, inactive_foreground);
    style.visuals.widgets.inactive.bg_fill = inactive_background;
    style.visuals.widgets.inactive.bg_stroke = stroke(BorderWidth::Thin, steel);
    style.visuals.widgets.hovered.fg_stroke = stroke(BorderWidth::Thin, hovered_foreground);
    style.visuals.widgets.hovered.bg_fill = hovered_background;
    style.visuals.widgets.hovered.bg_stroke = stroke(BorderWidth::Thin, focus);
    style.visuals.widgets.active.fg_stroke = stroke(BorderWidth::Thin, active_foreground);
    style.visuals.widgets.active.bg_fill = active_background;
    style.visuals.widgets.active.bg_stroke = stroke(BorderWidth::Thick, selected_boundary);
    style.visuals.selection.bg_fill = selected_background;
    style.visuals.selection.stroke = stroke(BorderWidth::Thick, selected_boundary);
    style.visuals.override_text_color = Some(primary);
    style
        .text_styles
        .insert(egui::TextStyle::Body, font_id(TypographyRole::Body));
    style
        .text_styles
        .insert(egui::TextStyle::Button, font_id(TypographyRole::Label));
    style
        .text_styles
        .insert(egui::TextStyle::Small, font_id(TypographyRole::BodySmall));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, font_id(TypographyRole::Section));
    style
        .text_styles
        .insert(egui::TextStyle::Monospace, font_id(TypographyRole::Code));
    ctx.set_style(style);
}

fn token_color(token: ColorToken) -> Color32 {
    let [r, g, b] = DARK.color(token).channels();
    Color32::from_rgb(r, g, b)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    LiveRack,
    ComponentGallery,
}

#[derive(Clone, Copy)]
enum RackAction {
    Select(usize),
    MoveUp,
    MoveDown,
    Remove,
    Add,
}

#[derive(Clone, Copy)]
enum SlotAction {
    Parameters(usize),
    Bypass(usize),
    Editor(usize),
    MoveUp(usize),
    MoveDown(usize),
    Remove(usize),
}

#[derive(Clone, Copy)]
struct Palette {
    canvas: Color32,
    panel: Color32,
    raised: Color32,
    steel: Color32,
    primary: Color32,
    secondary: Color32,
    on_accent: Color32,
    focus: Color32,
}

fn palette() -> Palette {
    Palette {
        canvas: token_color(ColorToken::Surface(Surface::Canvas)),
        panel: token_color(ColorToken::Surface(Surface::Panel)),
        raised: token_color(ColorToken::Surface(Surface::Raised)),
        steel: token_color(ColorToken::Surface(Surface::Steel)),
        primary: token_color(ColorToken::Text(TextColor::Primary)),
        secondary: token_color(ColorToken::Text(TextColor::Secondary)),
        on_accent: token_color(ColorToken::Text(TextColor::OnAccent)),
        focus: token_color(DARK.focus_color()),
    }
}

impl Palette {
    fn brand() -> Color32 {
        token_color(DARK.brand_foreground())
    }

    fn status(status: Status) -> Color32 {
        token_color(DARK.status_foreground(status))
    }

    fn interaction(interaction: Interaction) -> Color32 {
        token_color(DARK.interaction_colors(interaction).background_token())
    }
}

fn themed_button(text: impl Into<String>, kind: ButtonKind) -> egui::Button<'static> {
    let colors = DARK.control_colors(Control::Button(kind));
    egui::Button::new(
        RichText::new(text.into())
            .font(font_id(TypographyRole::Label))
            .color(token_color(colors.foreground_token())),
    )
    .fill(token_color(colors.background_token()))
    .corner_radius(radius(Radius::Medium))
}

fn section_header(ui: &mut Ui, palette: Palette, title: &str) {
    ui.add_space(space(Spacing::Md));
    ui.label(
        RichText::new(title)
            .font(font_id(TypographyRole::Meta))
            .color(palette.secondary),
    );
    ui.separator();
}

fn status_pill(ui: &mut Ui, palette: Palette, label: &str, color: Color32) {
    Frame::new()
        .fill(palette.raised)
        .stroke(stroke(BorderWidth::Thin, palette.steel))
        .corner_radius(radius(Radius::Pill))
        .inner_margin(margin(Spacing::Sm, Spacing::Xs))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = space(Spacing::Xs);
                ui.label(
                    RichText::new("●")
                        .color(color)
                        .size(text_size(TypographyRole::Meta)),
                );
                ui.label(
                    RichText::new(label)
                        .font(font_id(TypographyRole::Label))
                        .color(palette.primary),
                );
            });
        });
}

/// Paints a thin dBFS level meter: steel track, cyan fill, lime tip above −6 dBFS.
fn dbfs_meter(ui: &mut Ui, palette: Palette, fraction: f32) {
    const HOT_FRACTION: f32 = 0.9;
    let desired = Vec2::new(
        ui.available_width(),
        f32::from(Layout::MeterHeight.pixels()),
    );
    let (rect, _response) = ui.allocate_exact_size(desired, egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let corner = radius(Radius::Small);
    ui.painter().rect_filled(rect, corner, palette.steel);
    let fraction = fraction.clamp(0.0, 1.0);
    if fraction <= 0.0 {
        return;
    }
    let mut fill = rect;
    fill.set_right(rect.left() + rect.width() * fraction);
    ui.painter().rect_filled(fill, corner, Palette::brand());
    if fraction > HOT_FRACTION {
        let mut tip = fill;
        tip.set_left(rect.left() + rect.width() * HOT_FRACTION);
        ui.painter()
            .rect_filled(tip, corner, Palette::status(Status::Success));
    }
}

/// Root egui application state for the live-rack surface.
pub struct LiveRackApp {
    controller: SessionController,
    product: Result<ProductRuntime, String>,
    racks: Vec<RackCardState>,
    slots: Vec<PluginSlotState>,
    selected_slot: Option<usize>,
    parameters: Vec<GenericParameter>,
    catalog_plugins: Vec<CatalogPlugin>,
    selected_catalog_plugin: Option<usize>,
    scenes: Vec<ScenePadState>,
    selected_rack: usize,
    system: SystemStatus,
    worker: WorkerHealth,
    fault: FaultBannerModel,
    inspector_open: bool,
    status_line: String,
    screen: Screen,
    gallery: ComponentGallery,
    current_scene: Option<usize>,
    last_product_diagnostic: Option<String>,
    last_rack_latency: [u32; MAX_RACKS],
    #[cfg(target_os = "macos")]
    audio: MacOsAudioEndpoint,
    #[cfg(target_os = "macos")]
    audio_devices: Vec<MacOsAudioDevice>,
    #[cfg(target_os = "macos")]
    selected_route: Option<AudioRouteConfig>,
    #[cfg(target_os = "macos")]
    buffer_frames: u32,
    #[cfg(target_os = "macos")]
    midi_ports: Vec<MidiPortInfo>,
    #[cfg(target_os = "macos")]
    selected_midi: Option<MidiPortId>,
    #[cfg(target_os = "macos")]
    product_control: Option<ProductControl>,
    #[cfg(target_os = "macos")]
    midi_mapping_publisher: Option<MidiMappingPublisher>,
    #[cfg(target_os = "macos")]
    midi_learn: MidiLearnController,
    #[cfg(target_os = "macos")]
    midi_cc_monitor: Option<MidiCcMonitor>,
    #[cfg(target_os = "macos")]
    midi_learn_target: Option<MidiMappingTarget>,
    #[cfg(target_os = "macos")]
    telemetry: Option<std::sync::Arc<ProductTelemetry>>,
}

impl LiveRackApp {
    /// Creates the shell from session + presentation models.
    pub fn new(
        controller: SessionController,
        recovery_offered: bool,
        product: Result<ProductRuntime, String>,
    ) -> Self {
        #[cfg(target_os = "macos")]
        let audio = MacOsAudioEndpoint::new().allow_device_reconfiguration();
        #[cfg(target_os = "macos")]
        let audio_devices = duplex_devices().unwrap_or_default();
        #[cfg(target_os = "macos")]
        let buffer_frames = audio_devices
            .first()
            .and_then(|device| device.supported_buffer_frames.first())
            .copied()
            .unwrap_or(128);
        #[cfg(target_os = "macos")]
        let selected_route = audio_devices.first().map(|device| AudioRouteConfig {
            input: Some(device.info.id.clone()),
            output: device.info.id.clone(),
            format: AudioFormat::product_stereo(buffer_frames)
                .expect("discovery only returns product buffer sizes"),
        });
        #[cfg(target_os = "macos")]
        let midi_ports = MidirInput::enumerate_ports().unwrap_or_default();
        #[cfg(target_os = "macos")]
        let selected_midi = midi_ports.first().map(|port| port.id.clone());
        let catalog_plugins = product
            .as_ref()
            .map_or_else(|_| Vec::new(), ProductRuntime::catalog_plugins);
        let mut app = Self {
            controller,
            product,
            racks: Vec::new(),
            slots: Vec::new(),
            selected_slot: None,
            parameters: Vec::new(),
            selected_catalog_plugin: (!catalog_plugins.is_empty()).then_some(0),
            catalog_plugins,
            scenes: Vec::new(),
            selected_rack: 0,
            system: SystemStatus::new(SystemStatusState::Offline),
            worker: WorkerHealth {
                state: sp_ui::components::WorkerHealthState::Healthy,
            },
            fault: FaultBannerModel {
                state: if recovery_offered {
                    FaultBannerState::Visible
                } else {
                    FaultBannerState::Hidden
                },
                message: if recovery_offered {
                    "Unclean shutdown detected. Review the recovery package before going live."
                        .to_owned()
                } else {
                    String::new()
                },
            },
            inspector_open: true,
            status_line: format!("Engine stopped · 48 kHz · {buffer_frames} frames"),
            screen: if std::env::var_os("SUPERPOSITION_COMPONENT_GALLERY").is_some() {
                Screen::ComponentGallery
            } else {
                Screen::LiveRack
            },
            gallery: ComponentGallery::fixtures(),
            current_scene: None,
            last_product_diagnostic: None,
            last_rack_latency: [u32::MAX; MAX_RACKS],
            #[cfg(target_os = "macos")]
            audio,
            #[cfg(target_os = "macos")]
            audio_devices,
            #[cfg(target_os = "macos")]
            selected_route,
            #[cfg(target_os = "macos")]
            buffer_frames,
            #[cfg(target_os = "macos")]
            midi_ports,
            #[cfg(target_os = "macos")]
            selected_midi,
            #[cfg(target_os = "macos")]
            product_control: None,
            #[cfg(target_os = "macos")]
            midi_mapping_publisher: None,
            #[cfg(target_os = "macos")]
            midi_learn: MidiLearnController::new(),
            #[cfg(target_os = "macos")]
            midi_cc_monitor: None,
            #[cfg(target_os = "macos")]
            midi_learn_target: None,
            #[cfg(target_os = "macos")]
            telemetry: None,
        };
        app.refresh_models();
        app
    }

    fn set_status(&mut self, message: &str) {
        message.clone_into(&mut self.status_line);
    }

    fn save_session(&mut self) {
        let model = self.controller.document().model.clone();
        let captures = match self.product.as_mut() {
            Ok(product) => match product.capture_plugin_states(&model) {
                Ok(captures) => captures,
                Err(error) => {
                    self.show_fault(format!("Plug-in state capture failed: {error}"));
                    return;
                }
            },
            Err(_) => Vec::new(),
        };
        match self.controller.save_with_plugin_states(&captures) {
            Ok(()) => self.set_status("Session saved"),
            Err(error) => {
                self.fault = FaultBannerModel {
                    state: FaultBannerState::Visible,
                    message: format!("Save failed: {error}"),
                };
            }
        }
    }

    fn autosave_session(&mut self) {
        match self.controller.autosave() {
            Ok(()) => self.set_status("Autosave written"),
            Err(error) => {
                self.fault = FaultBannerModel {
                    state: FaultBannerState::Visible,
                    message: format!("Autosave failed: {error}"),
                };
            }
        }
    }

    fn acknowledge_recovery(&mut self) {
        self.controller.acknowledge_recovery_offer();
        self.fault.dismiss();
        self.set_status("Recovery offer dismissed");
    }

    fn refresh_models(&mut self) {
        let session = &self.controller.document().model;
        self.selected_rack = self
            .selected_rack
            .min(session.racks.len().saturating_sub(1));
        self.worker.state = if self
            .product
            .as_ref()
            .is_ok_and(|product| product.worker_recovering(self.selected_rack))
        {
            WorkerHealthState::Recovering
        } else if self
            .product
            .as_ref()
            .is_ok_and(|product| product.worker_running(self.selected_rack))
        {
            WorkerHealthState::Healthy
        } else {
            WorkerHealthState::Unavailable
        };
        self.racks = session
            .racks
            .iter()
            .enumerate()
            .map(|(rack_index, rack)| RackCardState {
                name: rack.name.clone(),
                status: if self
                    .product
                    .as_ref()
                    .is_ok_and(|product| product.worker_running(rack_index))
                {
                    RackCardStatus::Active
                } else if rack.slots.is_empty() {
                    RackCardStatus::Empty
                } else {
                    RackCardStatus::Bypassed
                },
            })
            .collect();
        self.slots = session
            .racks
            .get(self.selected_rack)
            .map(|rack| {
                rack.slots
                    .iter()
                    .enumerate()
                    .map(|(slot_index, slot)| PluginSlotState {
                        name: slot.plugin.identity.name.clone(),
                        status: if !self.product.as_ref().is_ok_and(|product| {
                            product.slot_running(self.selected_rack, slot_index)
                        }) {
                            PluginSlotStatus::Missing
                        } else if slot.bypassed {
                            PluginSlotStatus::Bypassed
                        } else {
                            PluginSlotStatus::Ready
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.scenes = session
            .scenes
            .iter()
            .enumerate()
            .map(|(scene_index, scene)| ScenePadState {
                name: scene.name.clone(),
                status: if self.current_scene == Some(scene_index) {
                    ScenePadStatus::Active
                } else {
                    ScenePadStatus::Idle
                },
            })
            .collect();
    }

    #[cfg(target_os = "macos")]
    #[allow(
        clippy::too_many_lines,
        reason = "engine start assembles every live handle in one deliberate sequence"
    )]
    fn toggle_engine(&mut self) {
        if self.audio.active_format().is_some() {
            match self.audio.stop() {
                Ok(()) => {
                    self.clear_live_handles();
                    self.system.set_state(SystemStatusState::Offline);
                    self.set_status(&format!(
                        "Engine stopped · 48 kHz · {} frames",
                        self.buffer_frames
                    ));
                }
                Err(error) => {
                    self.show_fault(format!("Audio engine did not stop cleanly: {error}"));
                }
            }
            return;
        }

        let graph = match PreparedGraph::compile(&self.controller.document().model) {
            Ok(graph) => graph,
            Err(error) => {
                self.show_fault(format!("Rack graph is not ready: {error}"));
                return;
            }
        };
        let banks = match self
            .product_mut()
            .and_then(|product| product.audio_bank_mappings())
        {
            Ok(banks) => banks,
            Err(error) => {
                self.show_fault(format!("Rack shared-memory setup failed: {error}"));
                return;
            }
        };
        let mut renderer = match ProductRenderer::with_rack_banks(graph, banks) {
            Ok(renderer) => renderer,
            Err(error) => {
                self.show_fault(format!("Rack dispatcher setup failed: {error}"));
                return;
            }
        };
        for (rack_index, rack) in self.controller.document().model.racks.iter().enumerate() {
            let dry_fallback = self
                .product
                .as_ref()
                .is_ok_and(|product| product.rack_dry_fallback_available(rack));
            renderer
                .mixer_mut()
                .set_dry_fallback_available(rack_index, dry_fallback);
            if let Some(latency) = self
                .product
                .as_ref()
                .ok()
                .and_then(|product| product.rack_latency_samples(rack_index))
            {
                renderer.mixer_mut().set_dry_delay_frames(
                    rack_index,
                    usize::try_from(latency).unwrap_or(usize::MAX),
                );
            }
            renderer
                .mixer_mut()
                .set_rack_gain(rack_index, 10.0_f32.powf(rack.gain_db.get() / 20.0));
            renderer.mixer_mut().set_rack_muted(rack_index, rack.muted);
            renderer
                .mixer_mut()
                .set_rack_bypassed(rack_index, rack.bypassed);
        }
        let mappings = product_midi_mappings(&self.controller.document().model);
        let scenes = prepare_product_scenes(&self.controller.document().model);
        let current_parameters = current_product_parameters(&self.controller.document().model);
        let (product_control, control_receiver) = ProductControl::new();
        let (mapping_publisher, mapping_receiver) = MidiMappingPublisher::new();
        renderer = renderer.with_live_control(
            control_receiver,
            mapping_receiver,
            mappings,
            scenes,
            current_parameters,
        );
        let telemetry = renderer.telemetry();
        let mut midi_cc_monitor = None;
        if let Some(selected) = self.selected_midi.as_ref() {
            let mut midi = MidirInput::new();
            if let Err(error) = midi.open_port(selected) {
                self.show_fault(format!("MIDI input could not open: {error}"));
                return;
            }
            midi_cc_monitor = Some(midi.cc_monitor());
            renderer = renderer.with_midi_input(midi);
        }
        self.system.set_state(SystemStatusState::Connecting);
        self.audio = MacOsAudioEndpoint::with_renderer(renderer).allow_device_reconfiguration();
        let Some(mut route) = self.selected_route.clone() else {
            self.system.set_state(SystemStatusState::Offline);
            self.show_fault("No stereo input/output device is available".to_owned());
            return;
        };
        route.format = AudioFormat::product_stereo(self.buffer_frames)
            .expect("fixed product buffer choice is valid");
        match self.audio.start_route(route.clone()) {
            Ok(()) => {
                self.product_control = Some(product_control);
                self.midi_mapping_publisher = Some(mapping_publisher);
                self.midi_learn = MidiLearnController::from_table(mappings);
                self.midi_cc_monitor = midi_cc_monitor;
                self.telemetry = Some(telemetry);
                self.system.set_state(SystemStatusState::Online);
                let device = self
                    .audio_devices
                    .iter()
                    .find(|device| device.info.id == route.output)
                    .map_or("CoreAudio", |device| device.info.name.as_str());
                self.set_status(&format!(
                    "Engine online · {device} · 48 kHz · {} frames",
                    self.buffer_frames
                ));
            }
            Err(error) => {
                self.system.set_state(SystemStatusState::Offline);
                self.show_fault(format!("CoreAudio could not start: {error}"));
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn refresh_audio_devices(&mut self) {
        match duplex_devices() {
            Ok(devices) => {
                self.audio_devices = devices;
                if self.selected_route.as_ref().is_none_or(|route| {
                    !self
                        .audio_devices
                        .iter()
                        .any(|device| device.info.id == route.output)
                }) {
                    self.selected_route = self.audio_devices.first().map(|device| {
                        self.buffer_frames = device.supported_buffer_frames[0];
                        AudioRouteConfig {
                            input: Some(device.info.id.clone()),
                            output: device.info.id.clone(),
                            format: AudioFormat::product_stereo(self.buffer_frames)
                                .expect("discovery only returns product buffer sizes"),
                        }
                    });
                } else if let Some(device) = self.audio_devices.iter().find(|device| {
                    self.selected_route
                        .as_ref()
                        .is_some_and(|route| route.output == device.info.id)
                }) && !device.supported_buffer_frames.contains(&self.buffer_frames)
                {
                    self.buffer_frames = device.supported_buffer_frames[0];
                }
                self.set_status("Audio device list refreshed");
            }
            Err(error) => self.show_fault(format!("Audio device discovery failed: {error}")),
        }
    }

    #[cfg(target_os = "macos")]
    fn refresh_midi_ports(&mut self) {
        match MidirInput::enumerate_ports() {
            Ok(ports) => {
                self.midi_ports = ports;
                if self
                    .selected_midi
                    .as_ref()
                    .is_none_or(|selected| !self.midi_ports.iter().any(|port| &port.id == selected))
                {
                    self.selected_midi = self.midi_ports.first().map(|port| port.id.clone());
                }
                self.set_status("MIDI port list refreshed");
            }
            Err(error) => self.show_fault(format!("MIDI discovery failed: {error}")),
        }
    }

    #[cfg(target_os = "macos")]
    fn poll_audio_event(&mut self) {
        let Some(event) = self.audio.poll_event() else {
            return;
        };
        let device = match event {
            AudioEndpointEvent::DeviceLost { device }
            | AudioEndpointEvent::DeviceConfigurationChanged { device } => device,
        };
        let _ = self.audio.stop();
        self.clear_live_handles();
        self.system.set_state(SystemStatusState::Offline);
        self.show_fault(format!(
            "Audio device {device} changed or disconnected. Output is muted; refresh the route before restarting."
        ));
    }

    #[cfg(not(target_os = "macos"))]
    fn toggle_engine(&mut self) {
        self.show_fault("The live audio engine requires Apple Silicon macOS".to_owned());
    }

    fn show_fault(&mut self, message: String) {
        self.fault = FaultBannerModel {
            state: FaultBannerState::Visible,
            message,
        };
    }

    #[cfg(target_os = "macos")]
    fn clear_live_handles(&mut self) {
        self.product_control = None;
        self.midi_mapping_publisher = None;
        self.midi_cc_monitor = None;
        self.telemetry = None;
        self.midi_learn.cancel();
        self.midi_learn_target = None;
        self.last_rack_latency = [u32::MAX; MAX_RACKS];
    }

    fn add_rack(&mut self) {
        let session = &self.controller.document().model;
        if session.racks.len() == MAX_RACKS {
            self.set_status("Rack limit reached (8)");
            return;
        }
        let mut index = 1;
        while session
            .racks
            .iter()
            .any(|rack| rack.id.0 == format!("rack-{index}"))
            || session
                .sources
                .iter()
                .any(|source| source.id.0 == format!("input-{index}"))
            || session
                .endpoints
                .iter()
                .any(|endpoint| endpoint.id.0 == format!("output-{index}"))
        {
            index += 1;
        }
        let source_id = SourceId(format!("input-{index}"));
        let endpoint_id = EndpointId(format!("output-{index}"));
        let document = self.controller.document_mut();
        document.model.sources.push(Source {
            id: source_id.clone(),
            name: format!("Input {index}"),
            layout: ChannelLayout::Stereo,
        });
        document.model.endpoints.push(Endpoint {
            id: endpoint_id.clone(),
            name: format!("Output {index}"),
            layout: ChannelLayout::Stereo,
        });
        document.model.racks.push(Rack {
            id: RackId(format!("rack-{index}")),
            name: format!("Rack {index}"),
            source_id,
            endpoint_id,
            topology: RackTopology::Serial,
            gain_db: sp_model::GainDb::default(),
            muted: false,
            bypassed: false,
            slots: Vec::new(),
        });
        self.selected_rack = self.controller.document().model.racks.len() - 1;
        self.rebuild_all_workers();
        self.set_status("Rack added; choose a plug-in after scanning");
    }

    fn move_selected_rack(&mut self, offset: isize) {
        let len = self.controller.document().model.racks.len();
        let Some(target) = self.selected_rack.checked_add_signed(offset) else {
            return;
        };
        if target >= len || target == self.selected_rack {
            return;
        }
        self.controller
            .document_mut()
            .model
            .racks
            .swap(self.selected_rack, target);
        self.selected_rack = target;
        self.rebuild_all_workers();
    }

    fn remove_selected_rack(&mut self) {
        let model = &mut self.controller.document_mut().model;
        if self.selected_rack >= model.racks.len() {
            return;
        }
        let rack = model.racks.remove(self.selected_rack);
        if model
            .racks
            .iter()
            .all(|remaining| remaining.source_id != rack.source_id)
        {
            model.sources.retain(|source| source.id != rack.source_id);
        }
        if model
            .racks
            .iter()
            .all(|remaining| remaining.endpoint_id != rack.endpoint_id)
        {
            model
                .endpoints
                .retain(|endpoint| endpoint.id != rack.endpoint_id);
        }
        model.scenes.iter_mut().for_each(|scene| {
            scene.gains.retain(|value| value.rack_id != rack.id);
            scene.mutes.retain(|value| value.rack_id != rack.id);
            scene.bypasses.retain(|value| value.rack_id != rack.id);
            scene
                .parameter_values
                .retain(|value| value.rack_id != rack.id);
        });
        model
            .midi_mappings
            .retain(|mapping| mapping.target.rack_id != rack.id);
        self.selected_rack = self.selected_rack.min(model.racks.len().saturating_sub(1));
        self.selected_slot = None;
        self.parameters.clear();
        self.rebuild_all_workers();
    }

    fn rebuild_all_workers(&mut self) {
        #[cfg(target_os = "macos")]
        if self.audio.active_format().is_some() {
            let _ = self.audio.stop();
            self.clear_live_handles();
            self.system.set_state(SystemStatusState::Offline);
        }
        let racks = self.controller.document().model.racks.clone();
        let saved_states = racks
            .iter()
            .map(|rack| {
                rack.slots
                    .iter()
                    .map(|slot| {
                        self.controller.load_plugin_state(&slot.id.0).map(|state| {
                            state.map(|(component, controller, metadata)| CapturedPluginState {
                                instance_id: slot.id.0.clone(),
                                component,
                                controller,
                                metadata,
                            })
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<Vec<_>, _>>();
        let Ok(saved_states) = saved_states else {
            self.show_fault("Could not read saved plug-in state while rebuilding racks".to_owned());
            return;
        };
        if self.product.is_err() {
            self.refresh_models();
            return;
        }
        let reload_error = {
            let product = self.product.as_mut().expect("product checked");
            product.unload_all_racks();
            racks.iter().zip(saved_states.iter()).enumerate().find_map(
                |(rack_index, (rack, states))| {
                    (!rack.slots.is_empty())
                        .then(|| product.load_rack(rack_index, rack, states).err())
                        .flatten()
                        .map(|error| (rack_index, error))
                },
            )
        };
        if let Some((rack_index, error)) = reload_error {
            self.show_fault(format!("Rack {} could not reload: {error}", rack_index + 1));
            return;
        }
        self.refresh_models();
        self.set_status("Rack order updated; restart the engine when ready");
    }

    fn snapshot_scene(&self, id: SceneId, name: String, transition_ms: u32) -> Scene {
        let model = &self.controller.document().model;
        Scene {
            id,
            name,
            gains: model
                .racks
                .iter()
                .map(|rack| RackGain {
                    rack_id: rack.id.clone(),
                    gain_db: rack.gain_db,
                })
                .collect(),
            mutes: model
                .racks
                .iter()
                .map(|rack| RackMute {
                    rack_id: rack.id.clone(),
                    muted: rack.muted,
                })
                .collect(),
            bypasses: model
                .racks
                .iter()
                .flat_map(|rack| {
                    rack.slots.iter().map(|slot| SlotBypass {
                        rack_id: rack.id.clone(),
                        slot_id: slot.id.clone(),
                        bypassed: slot.bypassed,
                    })
                })
                .take(MAX_SCENE_PARAMETER_VALUES)
                .collect(),
            parameter_values: model
                .racks
                .iter()
                .flat_map(|rack| {
                    rack.slots.iter().flat_map(|slot| {
                        slot.parameters.values.iter().map(|(parameter_id, value)| {
                            SceneParameterValue {
                                rack_id: rack.id.clone(),
                                slot_id: slot.id.clone(),
                                parameter_id: parameter_id.clone(),
                                value: *value,
                            }
                        })
                    })
                })
                .collect(),
            transition_ms,
        }
    }

    fn capture_scene(&mut self) {
        let sequence = self.controller.document().model.scenes.len() + 1;
        let scene = self.snapshot_scene(
            SceneId(format!("scene-{sequence}")),
            format!("Scene {sequence}"),
            250,
        );
        self.controller.document_mut().model.scenes.push(scene);
        self.current_scene = Some(sequence - 1);
        self.refresh_models();
        self.set_status("Scene captured from current rack and parameter values");
    }

    fn update_current_scene(&mut self) {
        let Some(index) = self.current_scene else {
            self.set_status("Recall or capture a scene before updating it");
            return;
        };
        let Some(existing) = self.controller.document().model.scenes.get(index) else {
            return;
        };
        let updated = self.snapshot_scene(
            existing.id.clone(),
            existing.name.clone(),
            existing.transition_ms,
        );
        self.controller.document_mut().model.scenes[index] = updated;
        self.refresh_models();
        self.set_status("Current scene updated");
    }

    #[cfg(target_os = "macos")]
    fn set_rack_controls(&mut self, gain_db: f32, muted: bool, bypassed: bool) {
        let rack_index = self.selected_rack;
        let Some(rack) = self
            .controller
            .document_mut()
            .model
            .racks
            .get_mut(rack_index)
        else {
            return;
        };
        rack.gain_db = sp_model::GainDb::new(gain_db).unwrap_or_default();
        rack.muted = muted;
        rack.bypassed = bypassed;
        let snapshot = rack.clone();
        if let Ok(product) = self.product.as_mut() {
            product.update_rack_snapshot(rack_index, &snapshot);
        }
        if self.system.state == SystemStatusState::Online {
            let Some(control) = self.product_control.as_mut() else {
                self.show_fault("Rack control queue is unavailable".to_owned());
                return;
            };
            let gain = 10.0_f32.powf(gain_db / 20.0);
            if !(control.set_rack_gain(rack_index, gain)
                && control.set_rack_muted(rack_index, muted)
                && control.set_rack_bypassed(rack_index, bypassed))
            {
                self.show_fault("Rack control queue is full".to_owned());
                return;
            }
        }
        self.set_status("Rack output controls updated");
    }

    fn scan_plugins(&mut self) {
        let result = self
            .product_mut()
            .and_then(|product| product.scan_standard_locations(true));
        match result {
            Ok(count) => {
                self.catalog_plugins = self
                    .product
                    .as_ref()
                    .map_or_else(|_| Vec::new(), ProductRuntime::catalog_plugins);
                self.selected_catalog_plugin = (!self.catalog_plugins.is_empty()).then_some(0);
                self.set_status(&format!("Scan complete · {count} bundles cataloged"));
            }
            Err(error) => self.show_fault(format!("Plug-in scan unavailable: {error}")),
        }
    }

    fn add_selected_plugin(&mut self) {
        let Some(plugin) = self
            .selected_catalog_plugin
            .and_then(|index| self.catalog_plugins.get(index))
            .cloned()
        else {
            self.show_fault("Scan and select a supported plug-in first".to_owned());
            return;
        };
        let Some(rack) = self
            .controller
            .document_mut()
            .model
            .racks
            .get_mut(self.selected_rack)
        else {
            return;
        };
        if rack.slots.len() >= MAX_SLOTS_PER_RACK {
            self.show_fault("Rack plug-in limit reached (8)".to_owned());
            return;
        }
        let mut sequence = rack.slots.len() + 1;
        let id = loop {
            let id = PluginInstanceId(format!("{}-slot-{sequence}", rack.id.0));
            if rack.slots.iter().all(|slot| slot.id != id) {
                break id;
            }
            sequence += 1;
        };
        rack.slots.push(PluginSlot {
            id,
            plugin: plugin.descriptor,
            bypassed: false,
            parameters: plugin.parameters,
        });
        self.selected_slot = Some(rack.slots.len() - 1);
        self.reload_selected_rack_after_edit();
    }

    fn move_slot(&mut self, from: usize, to: usize) {
        let Some(rack) = self
            .controller
            .document_mut()
            .model
            .racks
            .get_mut(self.selected_rack)
        else {
            return;
        };
        if from >= rack.slots.len() || to >= rack.slots.len() || from == to {
            return;
        }
        rack.slots.swap(from, to);
        self.selected_slot = Some(to);
        self.reload_selected_rack_after_edit();
    }

    fn remove_slot(&mut self, slot: usize) {
        let Some(rack) = self
            .controller
            .document_mut()
            .model
            .racks
            .get_mut(self.selected_rack)
        else {
            return;
        };
        if slot >= rack.slots.len() {
            return;
        }
        rack.slots.remove(slot);
        self.selected_slot = None;
        self.parameters.clear();
        self.reload_selected_rack_after_edit();
    }

    fn reload_selected_rack_after_edit(&mut self) {
        let empty = self
            .controller
            .document()
            .model
            .racks
            .get(self.selected_rack)
            .is_none_or(|rack| rack.slots.is_empty());
        if empty {
            #[cfg(target_os = "macos")]
            if self.audio.active_format().is_some() {
                if let Err(error) = self.audio.stop() {
                    self.show_fault(format!("Audio engine did not stop cleanly: {error}"));
                    return;
                }
                self.clear_live_handles();
            }
            if let Ok(product) = self.product.as_mut() {
                product.unload_rack(self.selected_rack);
            }
            self.refresh_models();
            self.set_status("Rack topology updated");
        } else {
            self.load_selected_rack();
        }
    }

    fn load_selected_rack(&mut self) {
        #[cfg(target_os = "macos")]
        if self.audio.active_format().is_some() {
            if let Err(error) = self.audio.stop() {
                self.show_fault(format!(
                    "Audio engine did not stop before the rack change: {error}"
                ));
                return;
            }
            self.clear_live_handles();
        }
        let Some(rack) = self
            .controller
            .document()
            .model
            .racks
            .get(self.selected_rack)
            .cloned()
        else {
            self.show_fault("Select a rack before loading a worker".to_owned());
            return;
        };
        let saved_states = match rack
            .slots
            .iter()
            .map(|slot| {
                self.controller.load_plugin_state(&slot.id.0).map(|state| {
                    state.map(|(component, controller, metadata)| CapturedPluginState {
                        instance_id: slot.id.0.clone(),
                        component,
                        controller,
                        metadata,
                    })
                })
            })
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(states) => states,
            Err(error) => {
                self.show_fault(format!("Saved plug-in state is unreadable: {error}"));
                return;
            }
        };
        let rack_index = self.selected_rack;
        match self
            .product_mut()
            .and_then(|product| product.load_rack(rack_index, &rack, &saved_states))
        {
            Ok(()) => {
                self.set_status("Rack worker ready · transactional topology installed");
                if !rack.slots.is_empty() {
                    self.select_parameters(0);
                }
            }
            Err(error) => self.show_fault(format!("Rack load failed: {error}")),
        }
    }

    fn slot_action(&mut self, action: SlotAction) {
        let rack_index = self.selected_rack;
        let result = match action {
            SlotAction::Parameters(slot) => {
                self.select_parameters(slot);
                return;
            }
            SlotAction::Editor(slot) => self
                .product_mut()
                .and_then(|product| product.open_native_editor(rack_index, slot)),
            SlotAction::Bypass(slot) => self
                .controller
                .document()
                .model
                .racks
                .get(rack_index)
                .and_then(|rack| rack.slots.get(slot))
                .map(|slot| !slot.bypassed)
                .ok_or_else(|| "slot is unavailable".to_owned())
                .and_then(|enabled| {
                    self.product_mut()
                        .and_then(|product| product.set_slot_bypass(rack_index, slot, enabled))?;
                    if let Some(model_slot) = self
                        .controller
                        .document_mut()
                        .model
                        .racks
                        .get_mut(rack_index)
                        .and_then(|rack| rack.slots.get_mut(slot))
                    {
                        model_slot.bypassed = enabled;
                    }
                    Ok(())
                }),
            SlotAction::MoveUp(slot) => {
                self.move_slot(slot, slot.saturating_sub(1));
                return;
            }
            SlotAction::MoveDown(slot) => {
                self.move_slot(slot, slot.saturating_add(1));
                return;
            }
            SlotAction::Remove(slot) => {
                self.remove_slot(slot);
                return;
            }
        };
        match result {
            Ok(()) if matches!(action, SlotAction::Editor(_)) => {
                self.set_status("Native editor toggled in its worker-owned window");
            }
            Ok(()) => self.set_status("Slot bypass toggled"),
            Err(error) => self.show_fault(if matches!(action, SlotAction::Editor(_)) {
                format!("Native editor unavailable: {error}")
            } else {
                format!("Bypass failed: {error}")
            }),
        }
    }

    fn select_parameters(&mut self, slot: usize) {
        let rack_index = self.selected_rack;
        match self
            .product
            .as_mut()
            .map_err(|error| error.clone())
            .and_then(|product| product.generic_parameters(rack_index, slot))
        {
            Ok(mut parameters) => {
                if let Some(model_slot) = self
                    .controller
                    .document()
                    .model
                    .racks
                    .get(self.selected_rack)
                    .and_then(|rack| rack.slots.get(slot))
                {
                    for parameter in &mut parameters {
                        if let Some(value) = model_slot
                            .parameters
                            .values
                            .get(&ParameterId(parameter.id.to_string()))
                        {
                            parameter.normalized = f64::from(value.0);
                        }
                    }
                }
                self.selected_slot = Some(slot);
                self.parameters = parameters;
                self.set_status("Generic parameter editor ready");
            }
            Err(error) => self.show_fault(format!("Parameters unavailable: {error}")),
        }
    }

    fn write_parameter(&mut self, parameter_index: usize) {
        let Some(slot) = self.selected_slot else {
            return;
        };
        let Some(parameter) = self.parameters.get(parameter_index) else {
            return;
        };
        let (id, normalized) = (parameter.id, parameter.normalized);
        let rack_index = self.selected_rack;
        match self
            .product_mut()
            .and_then(|product| product.write_parameter(rack_index, slot, id, normalized))
        {
            Ok(formatted) => {
                if let Some(parameter) = self.parameters.get_mut(parameter_index) {
                    parameter.formatted = formatted;
                }
                if let Some(model_slot) = self
                    .controller
                    .document_mut()
                    .model
                    .racks
                    .get_mut(self.selected_rack)
                    .and_then(|rack| rack.slots.get_mut(slot))
                {
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "normalized parameter values are bounded to 0.0..=1.0"
                    )]
                    let value = NormalizedValue(normalized as f32);
                    model_slot
                        .parameters
                        .values
                        .insert(ParameterId(id.to_string()), value);
                }
                self.set_status("Parameter updated");
            }
            Err(error) => self.show_fault(format!("Parameter write failed: {error}")),
        }
    }

    #[cfg(target_os = "macos")]
    fn arm_midi_learn(&mut self, parameter_index: usize) {
        let Some(slot) = self.selected_slot else {
            return;
        };
        let Some(parameter) = self.parameters.get(parameter_index) else {
            return;
        };
        if self.system.state != SystemStatusState::Online || self.midi_cc_monitor.is_none() {
            self.show_fault(
                "Start the engine with a MIDI input before arming MIDI Learn".to_owned(),
            );
            return;
        }
        let target = MidiMappingTarget {
            rack_index: self.selected_rack,
            slot_index: slot,
            parameter_id: parameter.id,
            minimum: 0.0,
            maximum: 1.0,
        };
        self.midi_learn.arm(target);
        self.midi_learn_target = Some(target);
        self.set_status("MIDI Learn armed · move one CC control");
    }

    #[cfg(target_os = "macos")]
    fn poll_midi_learn(&mut self) {
        let Some(event) = self
            .midi_cc_monitor
            .as_mut()
            .and_then(MidiCcMonitor::take_latest)
        else {
            return;
        };
        let learned = self.midi_learn.observe(event);
        let table = learned.unwrap_or_else(|| self.midi_learn.table());
        let channel = (event.bytes[0] & 0x0f) + 1;
        let controller = event.bytes[1];
        if let Some(mapped) = table.target_for(channel, controller)
            && let Some(rack) = self
                .controller
                .document_mut()
                .model
                .racks
                .get_mut(mapped.rack_index)
        {
            if let Some(slot) = rack.slots.get_mut(mapped.slot_index) {
                slot.parameters.values.insert(
                    ParameterId(mapped.parameter_id.to_string()),
                    NormalizedValue::new(map_controller_to_target(event.bytes[2], mapped))
                        .expect("mapped MIDI value is normalized"),
                );
            }
            let snapshot = rack.clone();
            if let Ok(product) = self.product.as_mut() {
                product.update_rack_snapshot(mapped.rack_index, &snapshot);
            }
        }
        if learned.is_none() {
            return;
        }
        let Some(target) = self.midi_learn_target.take() else {
            return;
        };
        let Some(rack) = self
            .controller
            .document()
            .model
            .racks
            .get(target.rack_index)
        else {
            return;
        };
        let Some(slot) = rack.slots.get(target.slot_index) else {
            return;
        };
        let mapping = MidiMapping {
            id: MidiMappingId(format!("midi-{channel}-{controller}")),
            source: MidiController {
                channel,
                controller,
            },
            target: ParameterAddress {
                rack_id: rack.id.clone(),
                slot_id: slot.id.clone(),
                parameter_id: ParameterId(target.parameter_id.to_string()),
            },
            minimum: NormalizedValue::new(target.minimum).expect("learn range is normalized"),
            maximum: NormalizedValue::new(target.maximum).expect("learn range is normalized"),
        };
        let model = &mut self.controller.document_mut().model;
        model.midi_mappings.retain(|existing| {
            existing.source.channel != channel || existing.source.controller != controller
        });
        model.midi_mappings.push(mapping);
        let published = self
            .midi_mapping_publisher
            .as_mut()
            .is_some_and(|publisher| publisher.publish(table).is_ok());
        if published {
            self.set_status("MIDI Learn captured and published");
        } else {
            self.show_fault(
                "MIDI mapping queue is full; restart the engine to apply it".to_owned(),
            );
        }
    }

    #[cfg(target_os = "macos")]
    fn recall_scene(&mut self, scene_index: usize) {
        let triggered = self
            .product_control
            .as_mut()
            .is_some_and(|control| control.trigger_scene(scene_index));
        if !triggered {
            self.show_fault("Scene trigger queue is unavailable".to_owned());
            return;
        }
        self.apply_scene_snapshot(scene_index);
    }

    #[cfg(target_os = "macos")]
    fn apply_scene_snapshot(&mut self, scene_index: usize) {
        let Some(scene) = self
            .controller
            .document()
            .model
            .scenes
            .get(scene_index)
            .cloned()
        else {
            return;
        };
        let model = &mut self.controller.document_mut().model;
        for gain in scene.gains {
            if let Some(rack) = model.racks.iter_mut().find(|rack| rack.id == gain.rack_id) {
                rack.gain_db = gain.gain_db;
            }
        }
        for mute in scene.mutes {
            if let Some(rack) = model.racks.iter_mut().find(|rack| rack.id == mute.rack_id) {
                rack.muted = mute.muted;
            }
        }
        for bypass in scene.bypasses {
            if let Some(slot) = model
                .racks
                .iter_mut()
                .find(|rack| rack.id == bypass.rack_id)
                .and_then(|rack| rack.slots.iter_mut().find(|slot| slot.id == bypass.slot_id))
            {
                slot.bypassed = bypass.bypassed;
            }
        }
        for parameter in scene.parameter_values {
            if let Some(slot) = model
                .racks
                .iter_mut()
                .find(|rack| rack.id == parameter.rack_id)
                .and_then(|rack| {
                    rack.slots
                        .iter_mut()
                        .find(|slot| slot.id == parameter.slot_id)
                })
            {
                slot.parameters
                    .values
                    .insert(parameter.parameter_id, parameter.value);
            }
        }
        if let Ok(product) = self.product.as_mut() {
            for (rack_index, rack) in model.racks.iter().enumerate() {
                product.update_rack_snapshot(rack_index, rack);
            }
        }
        self.current_scene = Some(scene_index);
        self.set_status("Scene recalled · deterministic parameter ramp active");
    }

    #[cfg(target_os = "macos")]
    fn poll_realtime_scene(&mut self) {
        let scene = self
            .telemetry
            .as_ref()
            .and_then(|telemetry| telemetry.current_scene());
        if let Some(scene) = scene
            && Some(scene) != self.current_scene
        {
            self.apply_scene_snapshot(scene);
        }
    }

    fn clear_quarantine(&mut self) {
        match self
            .product_mut()
            .and_then(ProductRuntime::clear_quarantine)
        {
            Ok(()) => self.set_status("Plug-in quarantine cleared"),
            Err(error) => self.show_fault(format!("Could not clear quarantine: {error}")),
        }
    }

    fn poll_product_diagnostic(&mut self) {
        let latest = self
            .product
            .as_ref()
            .ok()
            .and_then(|product| product.diagnostics().last())
            .cloned();
        if latest == self.last_product_diagnostic {
            return;
        }
        self.last_product_diagnostic.clone_from(&latest);
        if let Some(message) = latest
            && (message.contains("fault")
                || message.contains("fallback")
                || message.contains("failed"))
        {
            self.show_fault(message);
        }
    }

    #[cfg(target_os = "macos")]
    fn publish_latency_changes(&mut self) {
        if self.system.state != SystemStatusState::Online {
            return;
        }
        for rack in 0..MAX_RACKS {
            let Some(latency) = self
                .product
                .as_ref()
                .ok()
                .and_then(|product| product.rack_latency_samples(rack))
            else {
                continue;
            };
            if self.last_rack_latency[rack] == latency {
                continue;
            }
            if self
                .product_control
                .as_mut()
                .is_some_and(|control| control.set_rack_latency(rack, latency))
            {
                self.last_rack_latency[rack] = latency;
            }
        }
    }

    fn handle_live_keyboard(&mut self, ctx: &egui::Context) {
        if self.screen != Screen::LiveRack || ctx.wants_keyboard_input() {
            return;
        }
        let (up, down, activate, toggle, escape, shift) = ctx.input(|input| {
            (
                input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::ArrowDown),
                input.key_pressed(egui::Key::Enter),
                input.key_pressed(egui::Key::Space),
                input.key_pressed(egui::Key::Escape),
                input.modifiers.shift,
            )
        });
        let slot_count = self
            .controller
            .document()
            .model
            .racks
            .get(self.selected_rack)
            .map_or(0, |rack| rack.slots.len());
        if escape {
            self.selected_slot = None;
            self.parameters.clear();
        } else if let Some(slot) = self.selected_slot {
            if shift && up && slot > 0 {
                self.move_slot(slot, slot - 1);
            } else if shift && down && slot + 1 < slot_count {
                self.move_slot(slot, slot + 1);
            } else if up {
                self.select_parameters(slot.saturating_sub(1));
            } else if down && slot + 1 < slot_count {
                self.select_parameters(slot + 1);
            } else if activate {
                self.select_parameters(slot);
            } else if toggle {
                self.slot_action(SlotAction::Bypass(slot));
            }
        } else if down && !self.racks.is_empty() {
            self.selected_rack = (self.selected_rack + 1).min(self.racks.len() - 1);
        } else if up && !self.racks.is_empty() {
            self.selected_rack = self.selected_rack.saturating_sub(1);
        } else if activate && slot_count > 0 {
            self.select_parameters(0);
        }
    }

    fn product_mut(&mut self) -> Result<&mut ProductRuntime, String> {
        self.product.as_mut().map_err(|error| error.clone())
    }
}

impl eframe::App for LiveRackApp {
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Ok(product) = self.product.as_mut() {
            product.poll();
        }
        self.poll_product_diagnostic();
        #[cfg(target_os = "macos")]
        self.publish_latency_changes();
        #[cfg(target_os = "macos")]
        self.poll_audio_event();
        #[cfg(target_os = "macos")]
        self.poll_midi_learn();
        #[cfg(target_os = "macos")]
        self.poll_realtime_scene();
        if self.system.state == SystemStatusState::Online {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
        self.refresh_models();
        let palette = palette();
        if ctx.input(|input| input.modifiers.command && input.key_pressed(egui::Key::G)) {
            self.screen = match self.screen {
                Screen::LiveRack => Screen::ComponentGallery,
                Screen::ComponentGallery => Screen::LiveRack,
            };
        }
        if self.screen == Screen::ComponentGallery
            && ctx.input(|input| input.key_pressed(egui::Key::Escape))
        {
            self.screen = Screen::LiveRack;
        }
        self.handle_live_keyboard(ctx);

        egui::TopBottomPanel::top("system_bar")
            .exact_height(dimension(Layout::SystemBarHeight))
            .frame(
                Frame::new()
                    .fill(palette.panel)
                    .inner_margin(margin(Spacing::Md, Spacing::Sm)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new("SUPERPOSITION")
                            .font(font_id(TypographyRole::Brand))
                            .color(Palette::brand()),
                    );
                    ui.add_space(space(Spacing::Sm));
                    let (transport_label, transport_kind) = match self.system.state {
                        SystemStatusState::Online => ("Stop engine", ButtonKind::Secondary),
                        SystemStatusState::Connecting | SystemStatusState::Offline => {
                            ("Start engine", ButtonKind::Primary)
                        }
                    };
                    if ui
                        .add(
                            themed_button(transport_label, transport_kind).min_size(Vec2::new(
                                dimension(Layout::TransportButtonWidth),
                                dimension(Layout::TransportButtonHeight),
                            )),
                        )
                        .clicked()
                    {
                        self.toggle_engine();
                    }
                    let engine = match self.system.state {
                        SystemStatusState::Online => ("ONLINE", Palette::status(Status::Success)),
                        SystemStatusState::Connecting => {
                            ("CONNECTING", Palette::status(Status::Info))
                        }
                        SystemStatusState::Offline => ("OFFLINE", Palette::status(Status::Warning)),
                    };
                    status_pill(ui, palette, engine.0, engine.1);
                    ui.separator();
                    ui.label(
                        RichText::new(self.status_line.clone())
                            .font(font_id(TypographyRole::Code))
                            .color(palette.secondary),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(themed_button("Save", ButtonKind::Secondary))
                            .clicked()
                        {
                            self.save_session();
                        }
                        if ui
                            .add(themed_button("Autosave", ButtonKind::Quiet))
                            .clicked()
                        {
                            self.autosave_session();
                        }
                        if ui
                            .add(themed_button("Rescan plug-ins", ButtonKind::Quiet))
                            .clicked()
                        {
                            self.scan_plugins();
                        }
                        let gallery_label = match self.screen {
                            Screen::LiveRack => "Gallery",
                            Screen::ComponentGallery => "Live rack",
                        };
                        if ui
                            .add(themed_button(gallery_label, ButtonKind::Quiet))
                            .clicked()
                        {
                            self.screen = match self.screen {
                                Screen::LiveRack => Screen::ComponentGallery,
                                Screen::ComponentGallery => Screen::LiveRack,
                            };
                        }
                        ui.label(
                            RichText::new(format!("MIDI · worker {}", self.worker.state))
                                .font(font_id(TypographyRole::Code))
                                .color(palette.secondary),
                        );
                    });
                });
            });

        if self.screen == Screen::ComponentGallery {
            draw_component_gallery(ctx, &mut self.gallery, palette);
            return;
        }

        egui::TopBottomPanel::bottom("scene_dock")
            .exact_height(dimension(Layout::SceneDockHeight))
            .frame(
                Frame::new()
                    .fill(palette.panel)
                    .inner_margin(margin(Spacing::Md, Spacing::Sm)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new("SCENES")
                            .font(font_id(TypographyRole::Meta))
                            .color(palette.secondary),
                    );
                    ui.add_space(space(Spacing::Xs));
                    let mut recalled_scene = None;
                    for (scene_index, scene) in self.scenes.iter().enumerate() {
                        let selected = matches!(scene.status, ScenePadStatus::Active);
                        let fill = if selected {
                            Palette::interaction(Interaction::Selected)
                        } else {
                            palette.raised
                        };
                        if ui
                            .add_enabled(
                                self.system.state == SystemStatusState::Online,
                                egui::Button::new(
                                    RichText::new(&scene.name)
                                        .font(FontId::monospace(text_size(TypographyRole::Card)))
                                        .color(if selected {
                                            palette.on_accent
                                        } else {
                                            palette.primary
                                        }),
                                )
                                .fill(fill)
                                .stroke(stroke(
                                    if selected {
                                        BorderWidth::Thick
                                    } else {
                                        BorderWidth::Thin
                                    },
                                    if selected {
                                        palette.on_accent
                                    } else {
                                        palette.steel
                                    },
                                ))
                                .min_size(Vec2::new(
                                    dimension(Layout::SceneControlWidth),
                                    dimension(Layout::ScenePadHeight),
                                ))
                                .corner_radius(radius(Radius::Large)),
                            )
                            .clicked()
                        {
                            recalled_scene = Some(scene_index);
                        }
                    }
                    if let Some(scene_index) = recalled_scene {
                        #[cfg(target_os = "macos")]
                        self.recall_scene(scene_index);
                    }
                    let scene_editable = self.system.state == SystemStatusState::Offline;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(scene_index) = self.current_scene
                            && let Some(scene) = self
                                .controller
                                .document_mut()
                                .model
                                .scenes
                                .get_mut(scene_index)
                        {
                            ui.add_enabled(
                                scene_editable,
                                egui::DragValue::new(&mut scene.transition_ms)
                                    .range(0..=10_000)
                                    .suffix(" ms"),
                            )
                            .on_hover_text("Scene transition time");
                        }
                        if ui
                            .add_enabled(scene_editable, themed_button("Update", ButtonKind::Quiet))
                            .clicked()
                        {
                            self.update_current_scene();
                        }
                        if ui
                            .add_enabled(
                                scene_editable,
                                themed_button("Capture", ButtonKind::Secondary),
                            )
                            .clicked()
                        {
                            self.capture_scene();
                        }
                    });
                });
            });

        egui::SidePanel::left("rack_navigator")
            .exact_width(dimension(Layout::NavigatorWidth))
            .frame(
                Frame::new()
                    .fill(palette.panel)
                    .inner_margin(margin(Spacing::Md, Spacing::Md)),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("RACKS")
                        .font(font_id(TypographyRole::Meta))
                        .color(palette.secondary),
                );
                ui.add_space(space(Spacing::Sm));
                let mut rack_action = None;
                let rack_count = self.racks.len();
                for (index, rack) in self.racks.iter().enumerate() {
                    let selected = index == self.selected_rack;
                    let fill = if selected {
                        palette.raised
                    } else {
                        palette.panel
                    };
                    let status_color = match rack.status {
                        RackCardStatus::Active => Palette::status(Status::Success),
                        RackCardStatus::Bypassed | RackCardStatus::Empty => {
                            Palette::status(Status::Warning)
                        }
                    };
                    let card = Frame::new()
                        .fill(fill)
                        .stroke(stroke(BorderWidth::Thin, palette.steel))
                        .corner_radius(radius(Radius::Large))
                        .inner_margin(margin(Spacing::Md, Spacing::Md))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("●")
                                        .color(status_color)
                                        .size(text_size(TypographyRole::Meta)),
                                );
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(&rack.name)
                                                .font(font_id(TypographyRole::Card))
                                                .color(palette.primary),
                                        )
                                        .frame(false),
                                    )
                                    .clicked()
                                {
                                    rack_action = Some(RackAction::Select(index));
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            RichText::new(rack.status.label())
                                                .font(font_id(TypographyRole::Caption))
                                                .color(status_color),
                                        );
                                    },
                                );
                            });
                            #[cfg(target_os = "macos")]
                            if let Some(meter) = self
                                .telemetry
                                .as_ref()
                                .and_then(|telemetry| telemetry.rack_meter(index))
                            {
                                ui.add_space(space(Spacing::Xs));
                                dbfs_meter(
                                    ui,
                                    palette,
                                    meter_fraction(meter.peak[0].max(meter.peak[1])),
                                );
                            }
                            if selected {
                                ui.add_space(space(Spacing::Xs));
                                ui.horizontal(|ui| {
                                    if ui
                                        .add_enabled(
                                            index > 0,
                                            themed_button("↑", ButtonKind::Quiet),
                                        )
                                        .clicked()
                                    {
                                        rack_action = Some(RackAction::MoveUp);
                                    }
                                    if ui
                                        .add_enabled(
                                            index + 1 < rack_count,
                                            themed_button("↓", ButtonKind::Quiet),
                                        )
                                        .clicked()
                                    {
                                        rack_action = Some(RackAction::MoveDown);
                                    }
                                    if ui.add(themed_button("Remove", ButtonKind::Quiet)).clicked()
                                    {
                                        rack_action = Some(RackAction::Remove);
                                    }
                                });
                            }
                        });
                    if selected {
                        let rect = card.response.rect;
                        let bar = egui::Rect::from_min_max(
                            rect.left_top(),
                            egui::pos2(rect.left() + 3.0, rect.bottom()),
                        );
                        ui.painter()
                            .rect_filled(bar, radius(Radius::Small), palette.focus);
                    }
                    ui.add_space(space(Spacing::Sm));
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    if ui
                        .add_sized(
                            Vec2::new(
                                ui.available_width(),
                                dimension(Layout::TransportButtonHeight),
                            ),
                            themed_button("Add rack", ButtonKind::Quiet),
                        )
                        .clicked()
                    {
                        rack_action = Some(RackAction::Add);
                    }
                });
                match rack_action {
                    Some(RackAction::Select(index)) => self.selected_rack = index,
                    Some(RackAction::MoveUp) => self.move_selected_rack(-1),
                    Some(RackAction::MoveDown) => self.move_selected_rack(1),
                    Some(RackAction::Remove) => self.remove_selected_rack(),
                    Some(RackAction::Add) => self.add_rack(),
                    None => {}
                }
            });

        if self.inspector_open {
            egui::SidePanel::right("inspector")
                .exact_width(dimension(Layout::InspectorWidth))
                .frame(
                    Frame::new()
                        .fill(palette.panel)
                        .inner_margin(margin(Spacing::Md, Spacing::Md)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("INSPECTOR")
                                .font(font_id(TypographyRole::Meta))
                                .color(palette.secondary),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(themed_button("Collapse", ButtonKind::Quiet))
                                .clicked()
                            {
                                self.inspector_open = false;
                            }
                        });
                    });
                    section_header(ui, palette, "AUDIO");
                    #[cfg(target_os = "macos")]
                    {
                        let selected = self
                            .selected_route
                            .as_ref()
                            .and_then(|route| {
                                self.audio_devices
                                    .iter()
                                    .find(|device| device.info.id == route.output)
                            })
                            .map_or_else(
                                || "No duplex device".to_owned(),
                                |device| device.info.name.clone(),
                            );
                        let available_buffers = self
                            .selected_route
                            .as_ref()
                            .and_then(|route| {
                                self.audio_devices
                                    .iter()
                                    .find(|device| device.info.id == route.output)
                            })
                            .map_or_else(Vec::new, |device| device.supported_buffer_frames.clone());
                        ui.add_enabled_ui(self.system.state == SystemStatusState::Offline, |ui| {
                            egui::ComboBox::from_id_salt("audio_route")
                                .selected_text(selected)
                                .show_ui(ui, |ui| {
                                    for device in &self.audio_devices {
                                        let is_selected = self
                                            .selected_route
                                            .as_ref()
                                            .is_some_and(|route| route.output == device.info.id);
                                        if ui
                                            .selectable_label(is_selected, &device.info.name)
                                            .clicked()
                                        {
                                            self.buffer_frames = device.supported_buffer_frames[0];
                                            self.selected_route = Some(AudioRouteConfig {
                                                input: Some(device.info.id.clone()),
                                                output: device.info.id.clone(),
                                                format: AudioFormat::product_stereo(
                                                    self.buffer_frames,
                                                )
                                                .expect(
                                                    "discovery only returns product buffer sizes",
                                                ),
                                            });
                                        }
                                    }
                                });
                            if ui.button("Refresh devices").clicked() {
                                self.refresh_audio_devices();
                            }
                            ui.horizontal(|ui| {
                                ui.label("Buffer");
                                for frames in available_buffers {
                                    ui.radio_value(
                                        &mut self.buffer_frames,
                                        frames,
                                        frames.to_string(),
                                    );
                                }
                            });
                        });
                    }
                    ui.label(
                        RichText::new("Input 1–2 → rack workers → output 1–2")
                            .color(palette.secondary),
                    );
                    section_header(ui, palette, "MIDI");
                    #[cfg(target_os = "macos")]
                    ui.add_enabled_ui(self.system.state == SystemStatusState::Offline, |ui| {
                        let selected = self
                            .selected_midi
                            .as_ref()
                            .and_then(|selected| {
                                self.midi_ports.iter().find(|port| &port.id == selected)
                            })
                            .map_or_else(|| "No MIDI input".to_owned(), |port| port.name.clone());
                        egui::ComboBox::from_id_salt("midi_input")
                            .selected_text(selected)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(self.selected_midi.is_none(), "No MIDI input")
                                    .clicked()
                                {
                                    self.selected_midi = None;
                                }
                                for port in &self.midi_ports {
                                    let is_selected = self.selected_midi.as_ref() == Some(&port.id);
                                    if ui.selectable_label(is_selected, &port.name).clicked() {
                                        self.selected_midi = Some(port.id.clone());
                                    }
                                }
                            });
                        if ui.button("Refresh MIDI").clicked() {
                            self.refresh_midi_ports();
                        }
                    });
                    ui.label(
                        RichText::new("Armed mappings apply normalized values only.")
                            .font(font_id(TypographyRole::Caption))
                            .color(palette.secondary),
                    );
                    section_header(ui, palette, "PLUG-INS");
                    let selected_plugin = self
                        .selected_catalog_plugin
                        .and_then(|index| self.catalog_plugins.get(index))
                        .map_or("No supported plug-ins", |plugin| {
                            plugin.descriptor.identity.name.as_str()
                        });
                    egui::ComboBox::from_id_salt("plugin_browser")
                        .selected_text(selected_plugin)
                        .show_ui(ui, |ui| {
                            for (index, plugin) in self.catalog_plugins.iter().enumerate() {
                                ui.selectable_value(
                                    &mut self.selected_catalog_plugin,
                                    Some(index),
                                    format!(
                                        "{} · {}",
                                        plugin.descriptor.identity.name,
                                        plugin.descriptor.identity.vendor
                                    ),
                                );
                            }
                        });
                    if ui
                        .add(themed_button(
                            "Add plug-in to selected rack",
                            ButtonKind::Secondary,
                        ))
                        .clicked()
                    {
                        self.add_selected_plugin();
                    }
                    if ui
                        .add(themed_button(
                            "Load / preload selected rack",
                            ButtonKind::Quiet,
                        ))
                        .clicked()
                    {
                        self.load_selected_rack();
                    }
                    let catalog_count = self
                        .product
                        .as_ref()
                        .map_or(0, ProductRuntime::catalog_count);
                    let quarantined_count = self
                        .product
                        .as_ref()
                        .map_or(0, ProductRuntime::quarantined_count);
                    ui.label(
                        RichText::new(format!(
                            "Catalog · {catalog_count} bundles  ·  quarantine · {quarantined_count}"
                        ))
                        .font(font_id(TypographyRole::Code))
                        .color(palette.secondary),
                    );
                    if ui
                        .add(themed_button("Clear quarantine", ButtonKind::Quiet))
                        .clicked()
                    {
                        self.clear_quarantine();
                    }
                    section_header(ui, palette, "DIAGNOSTICS");
                    let worker_color = Palette::status(match self.worker.state {
                        WorkerHealthState::Healthy => Status::Success,
                        WorkerHealthState::Recovering => Status::Info,
                        WorkerHealthState::Unavailable => Status::Warning,
                    });
                    status_pill(ui, palette, self.worker.state.label(), worker_color);
                    if let Some(latency) = self
                        .product
                        .as_ref()
                        .ok()
                        .and_then(|product| product.rack_latency_samples(self.selected_rack))
                    {
                        ui.label(
                            RichText::new(format!("Latency · {latency} samples"))
                                .font(font_id(TypographyRole::Code))
                                .color(palette.secondary),
                        );
                    }
                    if self
                        .product
                        .as_ref()
                        .is_ok_and(|product| product.rack_restart_requested(self.selected_rack))
                    {
                        ui.label(
                            RichText::new("!  Plug-in requested a rack restart")
                                .color(Palette::status(Status::Warning)),
                        );
                    }
                    #[cfg(target_os = "macos")]
                    if let Some(diagnostics) = self
                        .telemetry
                        .as_ref()
                        .and_then(|telemetry| telemetry.rack_diagnostics(self.selected_rack))
                    {
                        ui.label(
                            RichText::new(format!(
                                "Blocks {} · misses {} · protocol {}",
                                diagnostics.completed,
                                diagnostics.deadline_misses,
                                diagnostics.protocol_rejections
                            ))
                            .font(font_id(TypographyRole::Code))
                            .color(palette.secondary),
                        );
                    }
                    if let Some(message) = self
                        .product
                        .as_ref()
                        .ok()
                        .and_then(|product| product.diagnostics().last())
                    {
                        ui.label(
                            RichText::new(message)
                                .font(font_id(TypographyRole::Code))
                                .color(palette.secondary),
                        );
                    }
                });
        }
        let mut requested_rack_controls = None;
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(palette.canvas)
                    .inner_margin(margin(Spacing::Lg, Spacing::Lg)),
            )
            .show(ctx, |ui| {
                if matches!(self.fault.state, FaultBannerState::Visible) {
                    Frame::new()
                        .fill(palette.raised)
                        .stroke(stroke(BorderWidth::Thick, palette.steel))
                        .corner_radius(radius(Radius::Large))
                        .inner_margin(margin(Spacing::Md, Spacing::Md))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("!  FAULT")
                                        .font(font_id(TypographyRole::Card))
                                        .color(Palette::status(Status::Error)),
                                );
                                ui.label(
                                    RichText::new(&self.fault.message)
                                        .color(Palette::status(Status::Error)),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add(themed_button("Dismiss", ButtonKind::Quiet))
                                            .clicked()
                                        {
                                            self.acknowledge_recovery();
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(space(Spacing::Md));
                }

                let rack_name = self
                    .racks
                    .get(self.selected_rack)
                    .map_or("Rack", |rack| rack.name.as_str());
                ui.label(
                    RichText::new(rack_name)
                        .font(font_id(TypographyRole::Section))
                        .color(palette.primary),
                );
                ui.label(
                    RichText::new("Serial plug-in chain")
                        .color(palette.secondary)
                        .font(font_id(TypographyRole::Supporting)),
                );
                #[cfg(target_os = "macos")]
                if let Some(rack) = self
                    .controller
                    .document()
                    .model
                    .racks
                    .get(self.selected_rack)
                {
                    let mut gain_db = rack.gain_db.get();
                    let mut muted = rack.muted;
                    let mut bypassed = rack.bypassed;
                    ui.horizontal(|ui| {
                        let gain_changed = ui
                            .add(
                                egui::Slider::new(&mut gain_db, -120.0..=24.0)
                                    .text("Gain dB")
                                    .fixed_decimals(1),
                            )
                            .changed();
                        let mute_changed = ui.checkbox(&mut muted, "Mute").changed();
                        let bypass_changed = ui.checkbox(&mut bypassed, "Rack bypass").changed();
                        if gain_changed || mute_changed || bypass_changed {
                            requested_rack_controls = Some((gain_db, muted, bypassed));
                        }
                    });
                }
                ui.add_space(space(Spacing::Md));

                let mut requested_slot_action = None;
                for (slot_index, slot) in self.slots.iter().enumerate() {
                    if slot_index > 0 {
                        chain_connector(ui, palette);
                    }
                    requested_slot_action = draw_slot(
                        ui,
                        slot,
                        slot_index,
                        self.slots.len(),
                        self.selected_slot == Some(slot_index),
                        palette,
                    )
                    .or(requested_slot_action);
                }
                if let Some(action) = requested_slot_action {
                    self.slot_action(action);
                }

                if let Some(slot) = self.selected_slot {
                    ui.add_space(space(Spacing::Lg));
                    ui.separator();
                    ui.label(
                        RichText::new(format!("Generic editor · Slot {}", slot + 1))
                            .font(font_id(TypographyRole::Section))
                            .color(palette.primary),
                    );
                    ui.add_space(space(Spacing::Sm));
                    let mut changed = None;
                    let mut learn_requested = None;
                    egui::ScrollArea::vertical()
                        .max_height(dimension(Layout::ParameterEditorHeight))
                        .show(ui, |ui| {
                            egui::Grid::new("generic_parameters")
                                .num_columns(4)
                                .spacing(Vec2::new(space(Spacing::Md), space(Spacing::Sm)))
                                .show(ui, |ui| {
                                    for (index, parameter) in self.parameters.iter_mut().enumerate()
                                    {
                                        ui.label(
                                            RichText::new(&parameter.name)
                                                .font(font_id(TypographyRole::Label))
                                                .color(palette.primary),
                                        );
                                        let mut slider =
                                            egui::Slider::new(&mut parameter.normalized, 0.0..=1.0)
                                                .show_value(false);
                                        if parameter.step_count > 0 {
                                            slider = slider
                                                .step_by(1.0 / f64::from(parameter.step_count));
                                        }
                                        let response = ui.add_enabled(!parameter.read_only, slider);
                                        if response.double_clicked() && !parameter.read_only {
                                            parameter.normalized = parameter.default_normalized;
                                            changed = Some(index);
                                        } else if response.changed() {
                                            changed = Some(index);
                                        }
                                        let value = if parameter.unit.is_empty() {
                                            parameter.formatted.clone()
                                        } else {
                                            format!("{} {}", parameter.formatted, parameter.unit)
                                        };
                                        ui.label(
                                            RichText::new(value)
                                                .font(font_id(TypographyRole::Code))
                                                .color(palette.primary),
                                        );
                                        ui.horizontal(|ui| {
                                            #[cfg(target_os = "macos")]
                                            {
                                                let meta = if parameter.read_only {
                                                    Some("READ ONLY")
                                                } else if parameter.discrete {
                                                    Some("DISCRETE")
                                                } else {
                                                    None
                                                };
                                                if let Some(meta) = meta {
                                                    ui.label(
                                                        RichText::new(meta)
                                                            .font(font_id(TypographyRole::Meta))
                                                            .color(palette.secondary),
                                                    );
                                                }
                                                if parameter.automatable
                                                    && ui
                                                        .add(themed_button(
                                                            "Learn",
                                                            ButtonKind::Quiet,
                                                        ))
                                                        .clicked()
                                                {
                                                    learn_requested = Some(index);
                                                }
                                            }
                                        });
                                        ui.end_row();
                                    }
                                });
                        });
                    if let Some(index) = changed {
                        self.write_parameter(index);
                    }
                    #[cfg(target_os = "macos")]
                    if let Some(index) = learn_requested {
                        self.arm_midi_learn(index);
                    }
                }

                if !self.inspector_open && ui.button("Show inspector").clicked() {
                    self.inspector_open = true;
                }
            });
        #[cfg(target_os = "macos")]
        if let Some((gain_db, muted, bypassed)) = requested_rack_controls {
            self.set_rack_controls(gain_db, muted, bypassed);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        #[cfg(target_os = "macos")]
        {
            let _ = self.audio.stop();
            self.clear_live_handles();
        }
        let _ = self.controller.mark_clean_exit();
    }
}

/// Paints a short vertical connector between slot cards so the rack reads as a serial chain.
fn chain_connector(ui: &mut Ui, palette: Palette) {
    let desired = Vec2::new(ui.available_width(), space(Spacing::Md));
    let (rect, _response) = ui.allocate_exact_size(desired, egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let x = rect.left() + space(Spacing::Xl);
    ui.painter().line_segment(
        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
        stroke(BorderWidth::Thick, palette.steel),
    );
}

#[allow(clippy::too_many_lines)]
fn draw_slot(
    ui: &mut Ui,
    slot: &PluginSlotState,
    slot_index: usize,
    slot_count: usize,
    selected: bool,
    palette: Palette,
) -> Option<SlotAction> {
    let mut action = None;
    let accent = match slot.status {
        PluginSlotStatus::Ready => Palette::status(Status::Success),
        PluginSlotStatus::Loading => Palette::status(Status::Info),
        PluginSlotStatus::Bypassed | PluginSlotStatus::Missing | PluginSlotStatus::Faulted => {
            Palette::status(Status::Warning)
        }
    };
    let card_stroke = if selected {
        stroke(BorderWidth::Thick, palette.focus)
    } else {
        stroke(BorderWidth::Thin, palette.steel)
    };
    Frame::new()
        .fill(palette.raised)
        .stroke(card_stroke)
        .corner_radius(radius(Radius::Medium))
        .inner_margin(margin(Spacing::Md, Spacing::Sm))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                Frame::new()
                    .fill(palette.panel)
                    .corner_radius(radius(Radius::Small))
                    .inner_margin(margin(Spacing::Sm, Spacing::Xs))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("{:02}", slot_index + 1))
                                .font(FontId::monospace(text_size(TypographyRole::Label)))
                                .color(palette.secondary),
                        );
                    });
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(&slot.name)
                                .font(font_id(TypographyRole::Card))
                                .color(palette.primary),
                        )
                        .frame(false),
                    )
                    .on_hover_text("Open generic parameter editor")
                    .clicked()
                {
                    action = Some(SlotAction::Parameters(slot_index));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(slot_index > 0, themed_button("↑", ButtonKind::Quiet))
                        .clicked()
                    {
                        action = Some(SlotAction::MoveUp(slot_index));
                    }
                    if ui
                        .add_enabled(
                            slot_index + 1 < slot_count,
                            themed_button("↓", ButtonKind::Quiet),
                        )
                        .clicked()
                    {
                        action = Some(SlotAction::MoveDown(slot_index));
                    }
                    if ui.add(themed_button("Remove", ButtonKind::Quiet)).clicked() {
                        action = Some(SlotAction::Remove(slot_index));
                    }
                    if ui
                        .add_enabled(
                            !matches!(slot.status, PluginSlotStatus::Missing),
                            themed_button("Editor", ButtonKind::Quiet),
                        )
                        .clicked()
                    {
                        action = Some(SlotAction::Editor(slot_index));
                    }
                    if ui
                        .add_enabled(
                            !matches!(slot.status, PluginSlotStatus::Missing),
                            themed_button(
                                if matches!(slot.status, PluginSlotStatus::Bypassed) {
                                    "Enable"
                                } else {
                                    "Bypass"
                                },
                                ButtonKind::Quiet,
                            ),
                        )
                        .clicked()
                    {
                        action = Some(SlotAction::Bypass(slot_index));
                    }
                    status_pill(ui, palette, slot.status.label(), accent);
                });
            });
            if matches!(slot.status, PluginSlotStatus::Missing) {
                ui.label(
                    RichText::new("Placeholder — plug-in unavailable; opaque state not restored.")
                        .color(palette.secondary)
                        .font(font_id(TypographyRole::BodySmall)),
                );
            }
        });
    action
}

#[cfg(target_os = "macos")]
fn duplex_devices() -> Result<Vec<MacOsAudioDevice>, Box<dyn std::error::Error + Send + Sync>> {
    Ok(enumerate_devices()?
        .into_iter()
        .filter(|device| {
            device.capabilities.max_input_channels >= 2
                && device.capabilities.max_output_channels >= 2
                && !device.supported_buffer_frames.is_empty()
        })
        .collect())
}

#[allow(clippy::too_many_lines)]
fn draw_component_gallery(ctx: &egui::Context, gallery: &mut ComponentGallery, palette: Palette) {
    egui::CentralPanel::default()
        .frame(
            Frame::new()
                .fill(palette.canvas)
                .inner_margin(margin(Spacing::Lg, Spacing::Lg)),
        )
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("COMPONENT GALLERY")
                                .size(text_size(TypographyRole::BodySmall))
                                .color(Palette::brand())
                                .strong(),
                        );
                        ui.label(
                            RichText::new("Operational states, under pressure")
                                .size(text_size(TypographyRole::Gallery))
                                .color(palette.primary)
                                .strong(),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        ui.label(
                            RichText::new("⌘G TOGGLE  ·  ESC CLOSE")
                                .monospace()
                                .color(palette.secondary),
                        );
                    });
                });
                ui.add_space(space(Spacing::Lg));

                gallery_section(ui, "SYSTEM + WORKERS", palette, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for system in &gallery.systems {
                            let color = match system.state {
                                SystemStatusState::Online => Palette::status(Status::Success),
                                SystemStatusState::Connecting => Palette::status(Status::Info),
                                SystemStatusState::Offline => Palette::status(Status::Warning),
                            };
                            status_pill(ui, palette, system.state.label(), color);
                        }
                        ui.separator();
                        for worker in &gallery.workers {
                            let color = match worker.state {
                                sp_ui::components::WorkerHealthState::Healthy => {
                                    Palette::status(Status::Success)
                                }
                                sp_ui::components::WorkerHealthState::Recovering => {
                                    Palette::status(Status::Info)
                                }
                                sp_ui::components::WorkerHealthState::Unavailable => {
                                    Palette::status(Status::Warning)
                                }
                            };
                            status_pill(ui, palette, worker.state.label(), color);
                        }
                    });
                });

                gallery_section(ui, "RACK CARDS", palette, |ui| {
                    ui.columns(3, |columns| {
                        for (column, rack) in columns.iter_mut().zip(&gallery.racks) {
                            let accent = match rack.status {
                                RackCardStatus::Active => Palette::status(Status::Success),
                                RackCardStatus::Bypassed => Palette::status(Status::Info),
                                RackCardStatus::Empty => Palette::status(Status::Warning),
                            };
                            Frame::new()
                                .fill(palette.raised)
                                .stroke(stroke(BorderWidth::Thin, palette.steel))
                                .corner_radius(radius(Radius::Large))
                                .inner_margin(margin(Spacing::Md, Spacing::Md))
                                .show(column, |ui| {
                                    ui.label(
                                        RichText::new(&rack.name)
                                            .size(text_size(TypographyRole::Card))
                                            .color(palette.primary)
                                            .strong(),
                                    );
                                    ui.label(
                                        RichText::new(format!("●  {}", rack.status.label()))
                                            .color(accent),
                                    );
                                    ui.add_space(space(Spacing::Xs));
                                    dbfs_meter(
                                        ui,
                                        palette,
                                        match rack.status {
                                            RackCardStatus::Active => 0.72,
                                            RackCardStatus::Bypassed => 0.18,
                                            RackCardStatus::Empty => 0.0,
                                        },
                                    );
                                });
                        }
                    });
                });

                gallery_section(ui, "PLUG-IN SLOTS", palette, |ui| {
                    for (slot_index, slot) in gallery.slots.iter().enumerate() {
                        if slot_index > 0 {
                            chain_connector(ui, palette);
                        }
                        let _ = draw_slot(
                            ui,
                            slot,
                            slot_index,
                            gallery.slots.len(),
                            slot_index == 0,
                            palette,
                        );
                    }
                });

                gallery_section(ui, "METERS + CONTROLS", palette, |ui| {
                    ui.columns(2, |columns| {
                        columns[0].label(
                            RichText::new("StereoMeter")
                                .color(palette.secondary)
                                .monospace(),
                        );
                        for meter in &gallery.meters {
                            let fraction = ((meter.peak_dbfs + 60.0) / 60.0).clamp(0.0, 1.0);
                            columns[0].label(
                                RichText::new(format!(
                                    "{:>6.1} dBFS · {}",
                                    meter.peak_dbfs, meter.state
                                ))
                                .monospace()
                                .color(palette.primary),
                            );
                            dbfs_meter(&mut columns[0], palette, fraction);
                            columns[0].add_space(space(Spacing::Xs));
                        }

                        columns[1].label(
                            RichText::new("GainFader")
                                .color(palette.secondary)
                                .monospace(),
                        );
                        columns[1].horizontal(|ui| {
                            let response = ui.add(
                                egui::Slider::new(&mut gallery.gain.gain_db, -120.0..=24.0)
                                    .suffix(" dB")
                                    .fixed_decimals(1),
                            );
                            if response.double_clicked() {
                                gallery.gain.reset();
                            }
                            ui.checkbox(&mut gallery.gain.muted, "Mute");
                        });
                        columns[1].label(
                            RichText::new("Double-click resets · Shift for fine adjustment")
                                .size(text_size(TypographyRole::Caption))
                                .color(palette.secondary),
                        );
                    });
                });

                gallery_section(ui, "PARAMETERS", palette, |ui| {
                    for parameter in &mut gallery.parameters {
                        draw_parameter(ui, parameter, palette);
                    }
                });

                gallery_section(ui, "SEGMENTS + SCENES", palette, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(&gallery.segmented.label).color(palette.secondary));
                        for (index, option) in gallery.segmented.options.clone().iter().enumerate()
                        {
                            if ui
                                .selectable_label(
                                    gallery.segmented.selected == index,
                                    RichText::new(option).color(
                                        if gallery.segmented.selected == index {
                                            palette.on_accent
                                        } else {
                                            palette.primary
                                        },
                                    ),
                                )
                                .clicked()
                            {
                                gallery.segmented.select(index);
                            }
                        }
                        ui.separator();
                        for scene in &mut gallery.scenes {
                            let selected = scene.status == ScenePadStatus::Active;
                            if ui
                                .add(
                                    egui::Button::new(RichText::new(&scene.name).color(
                                        if selected {
                                            palette.on_accent
                                        } else {
                                            palette.primary
                                        },
                                    ))
                                    .selected(selected)
                                    .min_size(Vec2::new(
                                        dimension(Layout::GallerySceneControlWidth),
                                        dimension(Layout::GallerySceneControlHeight),
                                    )),
                                )
                                .clicked()
                            {
                                scene.activate();
                            }
                        }
                    });
                });

                gallery_section(ui, "FAULT BANNER", palette, |ui| {
                    Frame::new()
                        .fill(palette.raised)
                        .stroke(stroke(BorderWidth::Thick, palette.steel))
                        .corner_radius(radius(Radius::Large))
                        .inner_margin(margin(Spacing::Md, Spacing::Md))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("!  FAULT")
                                        .color(Palette::status(Status::Error))
                                        .strong(),
                                );
                                ui.label(
                                    RichText::new(&gallery.fault.message)
                                        .color(Palette::status(Status::Error)),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let _ = ui.button("Restart rack");
                                        let _ = ui.button("Keep dry bypass");
                                    },
                                );
                            });
                        });
                });
            });
        });
}

fn gallery_section(ui: &mut Ui, title: &str, palette: Palette, contents: impl FnOnce(&mut Ui)) {
    ui.label(
        RichText::new(title)
            .size(text_size(TypographyRole::Caption))
            .monospace()
            .color(Palette::brand()),
    );
    ui.add_space(space(Spacing::Sm));
    Frame::new()
        .fill(palette.panel)
        .stroke(stroke(BorderWidth::Thin, palette.steel))
        .corner_radius(radius(Radius::Large))
        .inner_margin(margin(Spacing::Md, Spacing::Md))
        .show(ui, contents);
    ui.add_space(space(Spacing::Lg));
}

fn draw_parameter(ui: &mut Ui, parameter: &mut ParameterControlState, palette: Palette) {
    ui.horizontal(|ui| {
        ui.set_min_width(dimension(Layout::ParameterRowMinWidth));
        ui.label(
            RichText::new(&parameter.name)
                .color(palette.primary)
                .strong(),
        );
        let response = ui.add_enabled(
            !parameter.read_only,
            egui::Slider::new(&mut parameter.normalized, 0.0..=1.0).show_value(false),
        );
        if response.double_clicked() {
            parameter.reset();
        }
        ui.label(
            RichText::new(&parameter.formatted)
                .monospace()
                .color(palette.secondary),
        );
        let kind = if parameter.read_only {
            "READ ONLY"
        } else if parameter.discrete {
            "DISCRETE"
        } else {
            "CONTINUOUS"
        };
        ui.label(
            RichText::new(kind)
                .size(text_size(TypographyRole::Meta))
                .color(Palette::status(Status::Info)),
        );
    });
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use sp_session::SessionController;

    use super::LiveRackApp;

    #[test]
    fn added_rack_creates_a_valid_serial_route() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let process = std::process::id();
        let root = std::env::temp_dir().join(format!("superposition-ui-{process}-{unique}"));
        let mut app = LiveRackApp::new(
            SessionController::empty(&root),
            false,
            Err("helpers unavailable in UI test".to_owned()),
        );

        app.add_rack();

        let session = &app.controller.document().model;
        assert_eq!(session.racks.len(), 1);
        assert_eq!(session.racks[0].source_id.0, "input-1");
        assert_eq!(session.racks[0].endpoint_id.0, "output-1");
        session.validate().expect("rack route is valid");
    }
}
