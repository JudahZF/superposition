//! egui live-rack shell assembled from `sp-ui` component models and design tokens.

use eframe::egui::{
    self, Color32, CornerRadius, Frame, Margin, ProgressBar, RichText, Stroke, Ui, Vec2,
};
#[cfg(target_os = "macos")]
use sp_audio_io::{AudioEndpoint, AudioFormat};
#[cfg(target_os = "macos")]
use sp_engine::PreparedGraph;
use sp_model::{
    ChannelLayout, Endpoint, EndpointId, MAX_RACKS, Rack, RackId, RackTopology, Source, SourceId,
};
use sp_session::SessionController;
use sp_ui::{
    component_gallery::ComponentGallery,
    components::{
        FaultBannerModel, FaultBannerState, ParameterControlState, PluginSlotState,
        PluginSlotStatus, RackCardState, RackCardStatus, ScenePadState, ScenePadStatus,
        SystemStatus, SystemStatusState, WorkerHealth,
    },
    design::{Accent, ColorToken, DARK, Spacing, Surface, TextColor},
};

#[cfg(target_os = "macos")]
use sp_audio_io_macos::{MacOsAudioEndpoint, ProductRenderer};

fn space(spacing: Spacing) -> f32 {
    f32::from(spacing.pixels())
}

/// Installs Sora/Space Mono when present, otherwise keeps egui defaults (documented alpha fallback).
pub fn install_fonts(ctx: &egui::Context) {
    // Alpha: licensed font files are not bundled yet; egui's default proportional/monospace
    // families stand in until production assets land (see docs/brand-assets.md).
    let mut style = (*ctx.style()).clone();
    let canvas = token_color(ColorToken::Surface(Surface::Canvas));
    let panel = token_color(ColorToken::Surface(Surface::Panel));
    let raised = token_color(ColorToken::Surface(Surface::Raised));
    let steel = token_color(ColorToken::Surface(Surface::Steel));
    let primary = token_color(ColorToken::Text(TextColor::Primary));
    let secondary = token_color(ColorToken::Text(TextColor::Secondary));
    let cyan = token_color(ColorToken::Accent(Accent::Cyan));
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = panel;
    style.visuals.window_fill = raised;
    style.visuals.extreme_bg_color = canvas;
    style.visuals.faint_bg_color = panel;
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, secondary);
    style.visuals.widgets.inactive.bg_fill = raised;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, steel);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, cyan);
    style.visuals.widgets.active.bg_stroke = Stroke::new(2.0, cyan);
    style.visuals.selection.bg_fill = steel;
    style.visuals.selection.stroke = Stroke::new(2.0, cyan);
    style.visuals.override_text_color = Some(primary);
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
struct Palette {
    canvas: Color32,
    panel: Color32,
    raised: Color32,
    steel: Color32,
    primary: Color32,
    secondary: Color32,
    cyan: Color32,
    blue: Color32,
    lime: Color32,
}

fn palette() -> Palette {
    Palette {
        canvas: token_color(ColorToken::Surface(Surface::Canvas)),
        panel: token_color(ColorToken::Surface(Surface::Panel)),
        raised: token_color(ColorToken::Surface(Surface::Raised)),
        steel: token_color(ColorToken::Surface(Surface::Steel)),
        primary: token_color(ColorToken::Text(TextColor::Primary)),
        secondary: token_color(ColorToken::Text(TextColor::Secondary)),
        cyan: token_color(ColorToken::Accent(Accent::Cyan)),
        blue: token_color(ColorToken::Accent(Accent::Blue)),
        lime: token_color(ColorToken::Accent(Accent::Lime)),
    }
}

/// Root egui application state for the live-rack surface.
pub struct LiveRackApp {
    controller: SessionController,
    racks: Vec<RackCardState>,
    slots: Vec<PluginSlotState>,
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
    #[cfg(target_os = "macos")]
    audio: MacOsAudioEndpoint,
}

impl LiveRackApp {
    /// Creates the shell from session + presentation models.
    pub fn new(controller: SessionController, recovery_offered: bool) -> Self {
        let mut app = Self {
            controller,
            racks: Vec::new(),
            slots: Vec::new(),
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
            status_line: "Engine stopped · 48 kHz · 128 frames".to_owned(),
            screen: if std::env::var_os("SUPERPOSITION_COMPONENT_GALLERY").is_some() {
                Screen::ComponentGallery
            } else {
                Screen::LiveRack
            },
            gallery: ComponentGallery::fixtures(),
            current_scene: None,
            #[cfg(target_os = "macos")]
            audio: MacOsAudioEndpoint::new(),
        };
        app.refresh_models();
        app
    }

    fn set_status(&mut self, message: &str) {
        message.clone_into(&mut self.status_line);
    }

    fn save_session(&mut self) {
        match self.controller.save() {
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
        self.racks = session
            .racks
            .iter()
            .map(|rack| RackCardState {
                name: rack.name.clone(),
                status: if rack.slots.is_empty() {
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
                    .map(|slot| PluginSlotState {
                        name: slot.plugin.identity.name.clone(),
                        // Plug-ins are not launched until scanner/worker supervision supplies
                        // a verified runtime assignment.
                        status: PluginSlotStatus::Missing,
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
    fn toggle_engine(&mut self) {
        if self.audio.active_format().is_some() {
            match self.audio.stop() {
                Ok(()) => {
                    self.system.set_state(SystemStatusState::Offline);
                    self.set_status("Engine stopped · 48 kHz · 128 frames");
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
        self.system.set_state(SystemStatusState::Connecting);
        self.audio = MacOsAudioEndpoint::with_renderer(ProductRenderer::new(graph));
        let format = AudioFormat::product_stereo(128).expect("fixed product format is valid");
        match self.audio.start(format) {
            Ok(()) => {
                self.system.set_state(SystemStatusState::Online);
                self.set_status("Engine online · 48 kHz · 128 frames");
            }
            Err(error) => {
                self.system.set_state(SystemStatusState::Offline);
                self.show_fault(format!("CoreAudio could not start: {error}"));
            }
        }
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
            slots: Vec::new(),
        });
        self.selected_rack = index - 1;
        self.refresh_models();
        self.set_status("Rack added; choose a plug-in after scanning");
    }
}

impl eframe::App for LiveRackApp {
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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

        egui::TopBottomPanel::top("system_bar")
            .exact_height(52.0)
            .frame(
                Frame::new()
                    .fill(palette.panel)
                    .inner_margin(Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new("SUPERPOSITION")
                            .strong()
                            .size(18.0)
                            .color(palette.cyan),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(self.status_line.clone())
                            .color(palette.secondary)
                            .monospace(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Save").clicked() {
                            self.save_session();
                        }
                        if ui.button("Autosave").clicked() {
                            self.autosave_session();
                        }
                        let gallery_label = match self.screen {
                            Screen::LiveRack => "Gallery",
                            Screen::ComponentGallery => "Live rack",
                        };
                        if ui.button(gallery_label).clicked() {
                            self.screen = match self.screen {
                                Screen::LiveRack => Screen::ComponentGallery,
                                Screen::ComponentGallery => Screen::LiveRack,
                            };
                        }
                        let engine = match self.system.state {
                            SystemStatusState::Online => ("ENGINE", palette.lime),
                            SystemStatusState::Connecting => ("CONNECTING", palette.cyan),
                            SystemStatusState::Offline => ("OFFLINE", palette.secondary),
                        };
                        ui.label(RichText::new(engine.0).color(engine.1).strong());
                        ui.label(
                            RichText::new(format!("MIDI · worker {}", self.worker.state))
                                .color(palette.secondary)
                                .monospace(),
                        );
                    });
                });
            });

        if self.screen == Screen::ComponentGallery {
            draw_component_gallery(ctx, &mut self.gallery, palette);
            return;
        }

        egui::TopBottomPanel::bottom("scene_dock")
            .exact_height(64.0)
            .frame(
                Frame::new()
                    .fill(palette.panel)
                    .inner_margin(Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    let mut recalled_scene = None;
                    for (scene_index, scene) in self.scenes.iter().enumerate() {
                        let selected = matches!(scene.status, ScenePadStatus::Active);
                        let fill = if selected {
                            palette.blue
                        } else {
                            palette.raised
                        };
                        if ui
                            .add_enabled(
                                self.system.state == SystemStatusState::Online,
                                egui::Button::new(
                                    RichText::new(&scene.name).color(palette.primary),
                                )
                                .fill(fill)
                                .min_size(Vec2::new(96.0, 40.0))
                                .corner_radius(CornerRadius::same(8)),
                            )
                            .clicked()
                        {
                            recalled_scene = Some(scene_index);
                        }
                    }
                    if let Some(scene_index) = recalled_scene {
                        self.current_scene = Some(scene_index);
                        self.set_status("Scene recalled · parameter state only");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let action = if self.system.state == SystemStatusState::Online {
                            "Stop engine"
                        } else {
                            "Start engine"
                        };
                        if ui.button(action).clicked() {
                            self.toggle_engine();
                        }
                        ui.label(RichText::new("PARAMETER SCENES").color(palette.secondary));
                    });
                });
            });

        egui::SidePanel::left("rack_navigator")
            .exact_width(260.0)
            .frame(
                Frame::new()
                    .fill(palette.panel)
                    .inner_margin(Margin::same(12)),
            )
            .show(ctx, |ui| {
                ui.label(RichText::new("Racks").color(palette.secondary));
                if ui.button("Add rack").clicked() {
                    self.add_rack();
                }
                ui.add_space(space(Spacing::Sm));
                for (index, rack) in self.racks.iter().enumerate() {
                    let selected = index == self.selected_rack;
                    let stroke = if selected {
                        Stroke::new(2.0, palette.cyan)
                    } else {
                        Stroke::new(1.0, palette.steel)
                    };
                    let fill = if selected {
                        palette.raised
                    } else {
                        palette.panel
                    };
                    Frame::new()
                        .fill(fill)
                        .stroke(stroke)
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::same(10))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_label(
                                        selected,
                                        RichText::new(&rack.name).color(palette.primary),
                                    )
                                    .clicked()
                                {
                                    self.selected_rack = index;
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let color = match rack.status {
                                            RackCardStatus::Active => palette.lime,
                                            RackCardStatus::Bypassed => palette.secondary,
                                            RackCardStatus::Empty => palette.steel,
                                        };
                                        ui.label(RichText::new(rack.status.label()).color(color));
                                    },
                                );
                            });
                        });
                    ui.add_space(space(Spacing::Sm));
                }
            });

        if self.inspector_open {
            egui::SidePanel::right("inspector")
                .exact_width(320.0)
                .frame(
                    Frame::new()
                        .fill(palette.panel)
                        .inner_margin(Margin::same(12)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Inspector").color(palette.secondary));
                        if ui.button("Collapse").clicked() {
                            self.inspector_open = false;
                        }
                    });
                    ui.separator();
                    ui.label(RichText::new("Route").color(palette.primary));
                    ui.label(
                        RichText::new("Source → serial chain → output").color(palette.secondary),
                    );
                    ui.add_space(space(Spacing::Md));
                    ui.label(RichText::new("Worker").color(palette.primary));
                    ui.label(
                        RichText::new(self.worker.state.label())
                            .color(palette.lime)
                            .monospace(),
                    );
                    ui.add_space(space(Spacing::Md));
                    ui.label(RichText::new("MIDI Learn").color(palette.primary));
                    ui.label(
                        RichText::new("Armed mappings apply normalized values only.")
                            .color(palette.secondary),
                    );
                    ui.label(
                        RichText::new("Quarantine controls appear after scan results.")
                            .color(palette.secondary),
                    );
                });
        }

        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(palette.canvas)
                    .inner_margin(Margin::same(16)),
            )
            .show(ctx, |ui| {
                if matches!(self.fault.state, FaultBannerState::Visible) {
                    Frame::new()
                        .fill(palette.raised)
                        .stroke(Stroke::new(1.0, palette.steel))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&self.fault.message).color(palette.primary));
                                if ui.button("Dismiss").clicked() {
                                    self.acknowledge_recovery();
                                }
                            });
                        });
                    ui.add_space(space(Spacing::Md));
                }

                let rack_name = self
                    .racks
                    .get(self.selected_rack)
                    .map_or("Rack", |rack| rack.name.as_str());
                ui.label(RichText::new(rack_name).size(22.0).color(palette.primary));
                ui.label(
                    RichText::new("Serial plug-in chain")
                        .color(palette.secondary)
                        .size(13.0),
                );
                ui.add_space(space(Spacing::Md));

                for slot in &self.slots {
                    draw_slot(ui, slot, palette);
                    ui.add_space(space(Spacing::Sm));
                }

                if !self.inspector_open && ui.button("Show inspector").clicked() {
                    self.inspector_open = true;
                }
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        #[cfg(target_os = "macos")]
        let _ = self.audio.stop();
        let _ = self.controller.mark_clean_exit();
    }
}

fn draw_slot(ui: &mut Ui, slot: &PluginSlotState, palette: Palette) {
    let accent = match slot.status {
        PluginSlotStatus::Ready => palette.cyan,
        PluginSlotStatus::Loading => palette.blue,
        PluginSlotStatus::Bypassed | PluginSlotStatus::Missing | PluginSlotStatus::Faulted => {
            palette.secondary
        }
    };
    Frame::new()
        .fill(palette.raised)
        .stroke(Stroke::new(1.0, palette.steel))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&slot.name).color(palette.primary).size(15.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(slot.status.label()).color(accent));
                    let _ = ui.button("Bypass");
                    let _ = ui.button("Editor");
                });
            });
            if matches!(slot.status, PluginSlotStatus::Missing) {
                ui.label(
                    RichText::new("Placeholder — plug-in unavailable; opaque state not restored.")
                        .color(palette.secondary)
                        .size(12.0),
                );
            }
        });
}

#[allow(clippy::too_many_lines)]
fn draw_component_gallery(ctx: &egui::Context, gallery: &mut ComponentGallery, palette: Palette) {
    egui::CentralPanel::default()
        .frame(
            Frame::new()
                .fill(palette.canvas)
                .inner_margin(Margin::same(16)),
        )
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("COMPONENT GALLERY")
                                .size(12.0)
                                .color(palette.cyan)
                                .strong(),
                        );
                        ui.label(
                            RichText::new("Operational states, under pressure")
                                .size(26.0)
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
                                SystemStatusState::Online => palette.lime,
                                SystemStatusState::Connecting => palette.cyan,
                                SystemStatusState::Offline => palette.secondary,
                            };
                            status_chip(ui, system.state.label(), color, palette);
                        }
                        ui.separator();
                        for worker in &gallery.workers {
                            let color = match worker.state {
                                sp_ui::components::WorkerHealthState::Healthy => palette.lime,
                                sp_ui::components::WorkerHealthState::Recovering => palette.cyan,
                                sp_ui::components::WorkerHealthState::Unavailable => {
                                    palette.secondary
                                }
                            };
                            status_chip(ui, worker.state.label(), color, palette);
                        }
                    });
                });

                gallery_section(ui, "RACK CARDS", palette, |ui| {
                    ui.columns(3, |columns| {
                        for (column, rack) in columns.iter_mut().zip(&gallery.racks) {
                            let accent = match rack.status {
                                RackCardStatus::Active => palette.lime,
                                RackCardStatus::Bypassed => palette.blue,
                                RackCardStatus::Empty => palette.secondary,
                            };
                            Frame::new()
                                .fill(palette.raised)
                                .stroke(Stroke::new(1.0, palette.steel))
                                .corner_radius(CornerRadius::same(8))
                                .inner_margin(Margin::same(12))
                                .show(column, |ui| {
                                    ui.label(
                                        RichText::new(&rack.name)
                                            .size(15.0)
                                            .color(palette.primary)
                                            .strong(),
                                    );
                                    ui.label(
                                        RichText::new(format!("●  {}", rack.status.label()))
                                            .color(accent),
                                    );
                                    ui.add(
                                        ProgressBar::new(match rack.status {
                                            RackCardStatus::Active => 0.72,
                                            RackCardStatus::Bypassed => 0.18,
                                            RackCardStatus::Empty => 0.0,
                                        })
                                        .desired_width(ui.available_width()),
                                    );
                                });
                        }
                    });
                });

                gallery_section(ui, "PLUG-IN SLOTS", palette, |ui| {
                    for slot in &gallery.slots {
                        draw_slot(ui, slot, palette);
                        ui.add_space(space(Spacing::Sm));
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
                            columns[0].add(
                                ProgressBar::new(fraction)
                                    .text(format!(
                                        "{:>6.1} dBFS · {}",
                                        meter.peak_dbfs, meter.state
                                    ))
                                    .desired_width(columns[0].available_width()),
                            );
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
                                .size(11.0)
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
                                .selectable_label(gallery.segmented.selected == index, option)
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
                                    egui::Button::new(&scene.name)
                                        .selected(selected)
                                        .min_size(Vec2::new(92.0, 36.0)),
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
                        .stroke(Stroke::new(2.0, palette.steel))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("!  FAULT").color(palette.primary).strong());
                                ui.label(
                                    RichText::new(&gallery.fault.message).color(palette.primary),
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
            .size(11.0)
            .monospace()
            .color(palette.cyan),
    );
    ui.add_space(space(Spacing::Sm));
    Frame::new()
        .fill(palette.panel)
        .stroke(Stroke::new(1.0, palette.steel))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(12))
        .show(ui, contents);
    ui.add_space(space(Spacing::Lg));
}

fn status_chip(ui: &mut Ui, label: &str, color: Color32, palette: Palette) {
    Frame::new()
        .fill(palette.raised)
        .stroke(Stroke::new(1.0, palette.steel))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.label(RichText::new(format!("●  {label}")).color(color).strong());
        });
}

fn draw_parameter(ui: &mut Ui, parameter: &mut ParameterControlState, palette: Palette) {
    ui.horizontal(|ui| {
        ui.set_min_width(460.0);
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
        ui.label(RichText::new(kind).size(10.0).color(palette.blue));
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
        let root = std::env::temp_dir().join(format!("superposition-ui-{unique}"));
        let mut app = LiveRackApp::new(SessionController::empty(&root), false);

        app.add_rack();

        let session = &app.controller.document().model;
        assert_eq!(session.racks.len(), 1);
        assert_eq!(session.racks[0].source_id.0, "input-1");
        assert_eq!(session.racks[0].endpoint_id.0, "output-1");
        session.validate().expect("rack route is valid");
    }
}
