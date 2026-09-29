//! The setup page: Audio, MIDI, Plug-ins, and Diagnostics. It takes the column area; head and
//! foot stay. Device choices lock while the engine runs.

use eframe::egui::{self, Align2, Frame, Margin, Pos2, Rect, Response, Sense, Ui, Vec2};
use sp_model::MAX_RACKS;
use sp_ui::components::StateToken;
use sp_ui::design::{ColorToken, Layout, Spacing, TypographyRole};

use super::style::{
    Mark, action, choice_row, color, hairline, line, note, paint_text, px, sp, title,
};
use super::{LiveRackApp, SetupTab, show::latency_label};
#[cfg(target_os = "macos")]
use sp_audio_io::{AudioFormat, AudioRouteConfig};

const TABS: [(SetupTab, &str); 4] = [
    (SetupTab::Audio, "Audio"),
    (SetupTab::Midi, "MIDI"),
    (SetupTab::Plugins, "Plug-ins"),
    (SetupTab::Diagnostics, "Diagnostics"),
];
const BUFFER_CHOICES: [u32; 4] = [32, 64, 128, 256];

/// Table column widths in 16 px lines.
const NARROW: f32 = 3.0;
const MEDIUM: f32 = 8.0;
const WIDE: f32 = 16.0;

fn columns(lines: &[f32]) -> Vec<f32> {
    lines.iter().map(|lines| lines * line()).collect()
}

impl LiveRackApp {
    pub(super) fn draw_setup(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(color(ColorToken::Base)))
            .show(ctx, |ui| {
                let area = ui.max_rect();
                let rail = Rect::from_min_size(
                    area.min,
                    Vec2::new(px(Layout::SetupRailWidth), area.height()),
                );
                ui.painter().vline(
                    rail.right() - px(Layout::Hairline) / 2.0,
                    rail.y_range(),
                    hairline(ColorToken::Hairline),
                );
                let mut tabs = ui.new_child(
                    egui::UiBuilder::new().max_rect(rail.shrink2(Vec2::Y * sp(Spacing::S16))),
                );
                for (tab, label) in TABS {
                    if setup_tab(&mut tabs, label, self.setup_tab == tab).clicked() {
                        self.setup_tab = tab;
                    }
                }
                let page = Rect::from_min_max(Pos2::new(rail.right(), area.top()), area.max);
                let mut page_ui = ui.new_child(egui::UiBuilder::new().max_rect(page));
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(&mut page_ui, |ui| {
                        Frame::new()
                            .inner_margin(Margin::symmetric(
                                i8::try_from(Spacing::S24.pixels()).unwrap_or(i8::MAX),
                                i8::try_from(Spacing::S16.pixels()).unwrap_or(i8::MAX),
                            ))
                            .show(ui, |ui| {
                                ui.set_max_width(px(Layout::SetupPageWidth));
                                match self.setup_tab {
                                    SetupTab::Audio => self.draw_audio_setup(ui),
                                    SetupTab::Midi => self.draw_midi_setup(ui),
                                    SetupTab::Plugins => self.draw_plugin_setup(ui),
                                    SetupTab::Diagnostics => self.draw_diagnostics(ui),
                                }
                            });
                    });
            });
    }

    #[cfg(target_os = "macos")]
    #[allow(
        clippy::too_many_lines,
        reason = "output, input, and buffer choices read top to bottom"
    )]
    fn draw_audio_setup(&mut self, ui: &mut Ui) {
        let locked = self.online();
        if locked {
            note(
                ui,
                "Stop the engine to change devices or buffer size.",
                ColorToken::Warn,
            );
            ui.add_space(sp(Spacing::S8));
        }
        let previous_route = self.selected_route.clone();
        let previous_buffer = self.buffer_frames;
        section(ui, "Output device");
        let outputs: Vec<_> = self
            .audio_devices
            .iter()
            .filter(|device| device.capabilities.max_output_channels > 0)
            .map(|device| {
                (
                    device.info.id.clone(),
                    device.info.name.clone(),
                    device.capabilities.max_output_channels,
                )
            })
            .collect();
        for (id, name, channels) in outputs {
            let on = self
                .selected_route
                .as_ref()
                .is_some_and(|route| route.output == id);
            if choice_row(
                ui,
                Mark::Radio,
                on,
                &name,
                &format!("{channels} out"),
                !locked,
            )
            .clicked()
            {
                let input = self
                    .selected_route
                    .as_ref()
                    .and_then(|route| route.input.clone());
                self.selected_route = Some(AudioRouteConfig {
                    input,
                    output: id,
                    format: AudioFormat::product_stereo(self.buffer_frames)
                        .expect("product buffer size"),
                });
            }
        }
        section(ui, "Input device");
        let no_input = self
            .selected_route
            .as_ref()
            .is_none_or(|route| route.input.is_none());
        if choice_row(ui, Mark::Radio, no_input, "No audio input", "", !locked).clicked()
            && let Some(route) = &mut self.selected_route
        {
            route.input = None;
        }
        let inputs: Vec<_> = self
            .audio_devices
            .iter()
            .filter(|device| device.capabilities.max_input_channels > 0)
            .map(|device| {
                (
                    device.info.id.clone(),
                    device.info.name.clone(),
                    device.capabilities.max_input_channels,
                )
            })
            .collect();
        for (id, name, channels) in inputs {
            let on = self
                .selected_route
                .as_ref()
                .is_some_and(|route| route.input.as_ref() == Some(&id));
            if choice_row(
                ui,
                Mark::Radio,
                on,
                &name,
                &format!("{channels} in"),
                !locked,
            )
            .clicked()
                && let Some(route) = &mut self.selected_route
            {
                route.input = Some(id);
            }
        }
        if let Some(route) = &self.selected_route
            && route.input.as_ref() == Some(&route.output)
        {
            ui.add_space(sp(Spacing::S4));
            note(
                ui,
                "Input and output share a device. Use separate channels for live monitoring to avoid feedback.",
                ColorToken::Warn,
            );
        }
        section(ui, "Buffer at 48 kHz");
        let buffers = self
            .selected_route
            .as_ref()
            .map(|route| super::available_buffer_frames(&self.audio_devices, route))
            .unwrap_or_default();
        if !buffers.is_empty() && !buffers.contains(&self.buffer_frames) {
            self.buffer_frames = buffers[0];
        }
        for frames in BUFFER_CHOICES {
            let enabled = !locked && buffers.contains(&frames);
            if choice_row(
                ui,
                Mark::Radio,
                self.buffer_frames == frames,
                &format!("{frames} samples"),
                &latency_label(frames),
                enabled,
            )
            .clicked()
            {
                self.buffer_frames = frames;
            }
        }
        if self.selected_route.is_some() && buffers.is_empty() {
            note(
                ui,
                "These devices share no 48 kHz buffer size.",
                ColorToken::Warn,
            );
        }
        if self.selected_route.is_none()
            && let Some(saved) = &self.controller.document().model.audio_settings
        {
            let message = format!(
                "The saved output {} is unavailable. Choose a device.",
                saved.output.name
            );
            note(ui, &message, ColorToken::Warn);
        }
        ui.add_space(sp(Spacing::S10));
        if action(ui, "Refresh devices", None, !locked).clicked() {
            self.refresh_audio_devices();
        }
        if self.selected_route != previous_route || self.buffer_frames != previous_buffer {
            self.persist_audio_settings();
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn draw_audio_setup(&mut self, ui: &mut Ui) {
        note(
            ui,
            "The live audio engine requires Apple Silicon macOS.",
            ColorToken::Warn,
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "input choice, mappings table, and learn controls read top to bottom"
    )]
    fn draw_midi_setup(&mut self, ui: &mut Ui) {
        #[cfg(target_os = "macos")]
        {
            let locked = self.online();
            if locked {
                note(
                    ui,
                    "Stop the engine to change the MIDI input.",
                    ColorToken::Warn,
                );
                ui.add_space(sp(Spacing::S8));
            }
            section(ui, "MIDI input");
            if choice_row(
                ui,
                Mark::Radio,
                self.selected_midi.is_none(),
                "No MIDI input",
                "",
                !locked,
            )
            .clicked()
            {
                self.selected_midi = None;
            }
            let ports: Vec<_> = self
                .midi_ports
                .iter()
                .map(|port| (port.id.clone(), port.name.clone()))
                .collect();
            for (id, name) in ports {
                let on = self.selected_midi.as_ref() == Some(&id);
                if choice_row(ui, Mark::Radio, on, &name, "", !locked).clicked() {
                    self.selected_midi = Some(id);
                }
            }
            ui.add_space(sp(Spacing::S10));
            if action(ui, "Refresh MIDI", None, !locked).clicked() {
                self.refresh_midi_ports();
            }
        }
        section(ui, "Mappings");
        let widths = columns(&[NARROW, NARROW, WIDE * 2.0, MEDIUM]);
        table_row(
            ui,
            &widths,
            &[
                ("CC", ColorToken::Dim),
                ("Ch", ColorToken::Dim),
                ("Target", ColorToken::Dim),
                ("", ColorToken::Dim),
            ],
            ColorToken::Hairline,
        );
        let model = &self.controller.document().model;
        let rows: Vec<(String, String, String)> = model
            .midi_mappings
            .iter()
            .map(|mapping| {
                let target = model
                    .racks
                    .iter()
                    .find(|rack| rack.id == mapping.target.rack_id)
                    .and_then(|rack| {
                        let slot = rack
                            .slots
                            .iter()
                            .find(|slot| slot.id == mapping.target.slot_id)?;
                        Some(self.parameter_label(rack, slot, &mapping.target.parameter_id))
                    })
                    .unwrap_or_else(|| "Missing target".to_owned());
                (
                    mapping.source.controller.to_string(),
                    mapping.source.channel.to_string(),
                    target,
                )
            })
            .collect();
        let mut removed = None;
        for (index, (controller, channel, target)) in rows.iter().enumerate() {
            let cells = table_row(
                ui,
                &widths,
                &[
                    (controller, ColorToken::Info),
                    (channel, ColorToken::Text),
                    (target, ColorToken::Text),
                    ("", ColorToken::Text),
                ],
                ColorToken::HoverFill,
            );
            if cell_action(ui, cells[3], ("remove_mapping", index), "Remove").clicked() {
                removed = Some(index);
            }
        }
        if let Some(index) = removed {
            self.remove_midi_mapping(index);
        }
        if rows.is_empty() {
            super::style::text(
                ui,
                "No mappings yet.",
                TypographyRole::Body,
                ColorToken::Dim,
            );
        }
        ui.add_space(sp(Spacing::S10));
        #[cfg(target_os = "macos")]
        if self.midi_learn_armed {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = sp(Spacing::S20);
                super::style::text(
                    ui,
                    "Learn armed: touch a control in a plug-in editor, then move one CC",
                    TypographyRole::Body,
                    ColorToken::Warn,
                );
                if action(ui, "Cancel", None, true).clicked() {
                    self.cancel_midi_learn();
                    self.set_status("MIDI Learn cancelled");
                }
            });
        } else if action(ui, "Learn new mapping", None, true).clicked() {
            self.arm_midi_learn();
        }
        ui.add_space(sp(Spacing::S8));
        note(
            ui,
            "Armed mappings apply normalized values only. The parameter is whichever control was last touched in a plug-in editor.",
            ColorToken::Dim,
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "scan summary and the catalog table's three kinds of rows read top to bottom"
    )]
    fn draw_plugin_setup(&mut self, ui: &mut Ui) {
        let catalog = self
            .product
            .as_ref()
            .map_or(0, crate::ProductRuntime::catalog_count);
        let quarantined = self
            .product
            .as_ref()
            .map_or_else(|_| Vec::new(), crate::ProductRuntime::quarantined_plugins);
        let quarantined_count = self
            .product
            .as_ref()
            .map_or(0, crate::ProductRuntime::quarantined_count);
        let unavailable = self
            .product
            .as_ref()
            .map_or_else(|_| Vec::new(), crate::ProductRuntime::unavailable_plugins);
        let outdated = self
            .product
            .as_ref()
            .map_or(0, crate::ProductRuntime::stale_catalog_entries);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = sp(Spacing::S20);
            if action(ui, "Rescan plug-ins", None, true).clicked() {
                self.scan_plugins();
            }
            let last_scan = self
                .last_scan
                .and_then(|at| at.elapsed().ok())
                .map_or_else(String::new, |elapsed| {
                    format!("Last scan {}. ", super::show::elapsed_label(elapsed))
                });
            super::style::text(
                ui,
                format!("{last_scan}Catalog {catalog} bundles, quarantine {quarantined_count}."),
                TypographyRole::Body,
                ColorToken::Dim,
            );
        });
        if outdated > 0 {
            ui.add_space(sp(Spacing::S8));
            note(
                ui,
                &format!(
                    "{outdated} catalog entries predate sidechain detection. Rescan to find plug-ins with a sidechain input."
                ),
                ColorToken::Warn,
            );
        }
        section(ui, "Catalog");
        let widths = columns(&[WIDE, MEDIUM * 1.5, WIDE, MEDIUM]);
        table_row(
            ui,
            &widths,
            &[
                ("Plug-in", ColorToken::Dim),
                ("Vendor", ColorToken::Dim),
                ("State", ColorToken::Dim),
                ("", ColorToken::Dim),
            ],
            ColorToken::Hairline,
        );
        let plugins: Vec<(String, String)> = self
            .catalog_plugins
            .iter()
            .map(|plugin| {
                (
                    plugin.descriptor.identity.name.clone(),
                    plugin.descriptor.identity.vendor.clone(),
                )
            })
            .collect();
        for (name, vendor) in &plugins {
            table_row(
                ui,
                &widths,
                &[
                    (name, ColorToken::Text),
                    (vendor, ColorToken::Text),
                    ("OK", ColorToken::Dim),
                    ("", ColorToken::Dim),
                ],
                ColorToken::HoverFill,
            );
        }
        for entry in &unavailable {
            let state = format!("{}, not loadable", capitalized(entry.reason));
            table_row(
                ui,
                &widths,
                &[
                    (&entry.name, ColorToken::Text),
                    (&entry.vendor, ColorToken::Text),
                    (&state, ColorToken::Warn),
                    ("", ColorToken::Text),
                ],
                ColorToken::HoverFill,
            );
        }
        let mut retry = None;
        for (index, entry) in quarantined.iter().enumerate() {
            let state = format!("Quarantined, {} failures", entry.failures);
            let cells = table_row(
                ui,
                &widths,
                &[
                    (&entry.name, ColorToken::Text),
                    ("", ColorToken::Text),
                    (&state, ColorToken::Fault),
                    ("", ColorToken::Text),
                ],
                ColorToken::HoverFill,
            );
            if cell_action(ui, cells[3], ("retry", index), "Allow retry").clicked() {
                retry = Some(entry.fingerprint.clone());
            }
        }
        if let Some(fingerprint) = retry {
            self.retry_quarantined_plugin(&fingerprint);
        }
        if plugins.is_empty() && quarantined.is_empty() && unavailable.is_empty() {
            super::style::text(
                ui,
                "No plug-ins yet. Rescan to catalog installed VST3 bundles.",
                TypographyRole::Body,
                ColorToken::Dim,
            );
        }
        ui.add_space(sp(Spacing::S8));
        note(
            ui,
            "Allow retry clears only that plug-in's failure history. Other quarantines stay unchanged.",
            ColorToken::Dim,
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the rack table and engine rows read top to bottom"
    )]
    fn draw_diagnostics(&mut self, ui: &mut Ui) {
        section(ui, "Racks");
        let widths = columns(&[
            NARROW,
            MEDIUM,
            WIDE * 0.75,
            NARROW * 1.5,
            MEDIUM * 0.75,
            MEDIUM * 0.75,
            MEDIUM * 0.75,
            MEDIUM,
        ]);
        table_row(
            ui,
            &widths,
            &[
                ("#", ColorToken::Dim),
                ("Rack", ColorToken::Dim),
                ("Worker", ColorToken::Dim),
                ("Restarts", ColorToken::Dim),
                ("Misses", ColorToken::Dim),
                ("Rejections", ColorToken::Dim),
                ("Saved", ColorToken::Dim),
                ("", ColorToken::Dim),
            ],
            ColorToken::Hairline,
        );
        let racks: Vec<(String, bool)> = self
            .controller
            .document()
            .model
            .racks
            .iter()
            .map(|rack| (rack.name.clone(), !rack.slots.is_empty()))
            .collect();
        let saved = self.saved_at.map_or_else(
            || "--".to_owned(),
            |at| super::show::elapsed_label(at.elapsed()),
        );
        let mut retry = None;
        let mut preload = None;
        for (index, (name, has_slots)) in racks.iter().enumerate() {
            let product = self.product.as_ref().ok();
            let token = self.rack_token(index);
            let worker = match token {
                StateToken::Faulted => ("Recovery failed, dry", ColorToken::Fault),
                StateToken::Recovering => ("Recovering", ColorToken::Warn),
                _ if self.worker_running(index) => ("Healthy", ColorToken::Text),
                _ => ("Unloaded", ColorToken::Dim),
            };
            let restarts = product.map_or(0, |product| product.rack_restart_count(index));
            #[cfg(target_os = "macos")]
            let diagnostics = self
                .telemetry
                .as_ref()
                .and_then(|telemetry| telemetry.rack_diagnostics(index));
            #[cfg(target_os = "macos")]
            let (misses, rejections) = diagnostics.map_or_else(
                || ("--".to_owned(), "--".to_owned()),
                |diagnostics| {
                    (
                        diagnostics.deadline_misses.to_string(),
                        diagnostics.protocol_rejections.to_string(),
                    )
                },
            );
            #[cfg(not(target_os = "macos"))]
            let (misses, rejections) = ("--".to_owned(), "--".to_owned());
            let number = format!("{:02}", index + 1);
            let restarts_text = restarts.to_string();
            let cells = table_row(
                ui,
                &widths,
                &[
                    (&number, ColorToken::Dim),
                    (name, ColorToken::Text),
                    worker,
                    (
                        &restarts_text,
                        if restarts > 0 {
                            ColorToken::Warn
                        } else {
                            ColorToken::Text
                        },
                    ),
                    (&misses, ColorToken::Text),
                    (&rejections, ColorToken::Text),
                    (&saved, ColorToken::Text),
                    ("", ColorToken::Text),
                ],
                ColorToken::HoverFill,
            );
            let restart_failed = product.is_some_and(|product| {
                product.planned_maintenance_failed(index) || product.worker_recovery_failed(index)
            });
            if restart_failed {
                if cell_action(ui, cells[7], ("retry_rack", index), "Retry restart").clicked() {
                    retry = Some(index);
                }
            } else if !self.online()
                && *has_slots
                && !self.worker_running(index)
                && cell_action(ui, cells[7], ("preload", index), "Preload").clicked()
            {
                preload = Some(index);
            }
        }
        if let Some(rack) = retry {
            self.retry_rack_restart(rack);
        }
        if let Some(rack) = preload {
            self.selected_rack = rack;
            self.load_selected_rack();
        }
        if racks.is_empty() {
            super::style::text(ui, "No racks yet.", TypographyRole::Body, ColorToken::Dim);
        }
        let host_updates: Vec<String> = (0..racks.len())
            .filter(|&rack| {
                self.product
                    .as_ref()
                    .is_ok_and(|product| product.rack_restart_flags(rack) != 0)
            })
            .map(|rack| racks[rack].0.clone())
            .collect();
        if !host_updates.is_empty() {
            ui.add_space(sp(Spacing::S8));
            note(
                ui,
                &format!(
                    "{} requested a host update. That notification alone does not restart the rack.",
                    host_updates.join(", ")
                ),
                ColorToken::Warn,
            );
        }
        let over_dry_limit: Vec<String> = (0..racks.len().min(MAX_RACKS))
            .filter(|&rack| {
                self.product
                    .as_ref()
                    .ok()
                    .and_then(|product| product.rack_latency_samples(rack))
                    .is_some_and(|latency| {
                        usize::try_from(latency).unwrap_or(usize::MAX)
                            > sp_engine::MAX_DRY_DELAY_FRAMES
                    })
            })
            .map(|rack| racks[rack].0.clone())
            .collect();
        if !over_dry_limit.is_empty() {
            ui.add_space(sp(Spacing::S8));
            note(
                ui,
                &format!(
                    "{} exceed the {}-sample dry-delay limit, so their dry bypass and fallback are silent. Reduce plug-in latency to restore delayed dry audio.",
                    over_dry_limit.join(", "),
                    sp_engine::MAX_DRY_DELAY_FRAMES
                ),
                ColorToken::Warn,
            );
        }
        section(ui, "Engine");
        let frames = self.buffer_frames_value();
        let load = self.callback_load().map_or_else(
            || "--".to_owned(),
            |load| format!("{:.0} % of {}", load * 100.0, latency_label(frames)),
        );
        let misses = self
            .deadline_misses()
            .map_or_else(|| "--".to_owned(), |misses| misses.to_string());
        let aggregate = self.aggregate_label();
        let widths = columns(&[MEDIUM * 1.5, WIDE * 2.0]);
        for (label, value) in [
            ("Callback load", load.as_str()),
            ("Deadline misses", misses.as_str()),
            ("Aggregate device", aggregate),
        ] {
            table_row(
                ui,
                &widths,
                &[(label, ColorToken::Dim), (value, ColorToken::Text)],
                ColorToken::HoverFill,
            );
        }
        if let Some(message) = self
            .product
            .as_ref()
            .ok()
            .and_then(|product| product.diagnostics().last())
            .cloned()
        {
            ui.add_space(sp(Spacing::S10));
            note(ui, &format!("Last event: {message}"), ColorToken::Dim);
        }
    }

    /// Whether the engine needs a private aggregate for separate input and output devices.
    fn aggregate_label(&self) -> &'static str {
        #[cfg(target_os = "macos")]
        if let Some(route) = &self.selected_route
            && route
                .input
                .as_ref()
                .is_some_and(|input| *input != route.output)
        {
            return "Private aggregate, clocked by the output";
        }
        "Not needed"
    }
}

/// A setup rail tab: dim, text on hover, inverted while active.
fn setup_tab(ui: &mut Ui, label: &str, active: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), line() + sp(Spacing::S4)),
        Sense::click(),
    );
    let token = if active {
        ui.painter().rect_filled(rect, 0.0, color(ColorToken::Text));
        ColorToken::Base
    } else if response.hovered() {
        ColorToken::Text
    } else {
        ColorToken::Dim
    };
    paint_text(
        ui,
        Pos2::new(rect.left() + sp(Spacing::S20), rect.center().y),
        Align2::LEFT_CENTER,
        label,
        TypographyRole::Body,
        token,
        rect.width(),
    );
    super::style::focus_outline(ui, &response, rect);
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, active, label)
    });
    response
}

/// A section title with space above it.
fn section(ui: &mut Ui, text: &str) {
    ui.add_space(sp(Spacing::S16));
    title(ui, text);
    ui.add_space(sp(Spacing::S4));
}

/// One table row of text cells with a rule beneath it. Returns each cell's rect so callers can
/// place actions in them.
fn table_row(
    ui: &mut Ui,
    widths: &[f32],
    cells: &[(&str, ColorToken)],
    rule: ColorToken,
) -> Vec<Rect> {
    let height = line() + sp(Spacing::S4);
    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    ui.painter().hline(
        row.x_range(),
        row.bottom() - px(Layout::Hairline) / 2.0,
        hairline(rule),
    );
    let mut left = row.left();
    widths
        .iter()
        .zip(cells)
        .map(|(width, (text, token))| {
            let cell = Rect::from_min_size(Pos2::new(left, row.top()), Vec2::new(*width, height));
            left += width;
            paint_text(
                ui,
                cell.left_center(),
                Align2::LEFT_CENTER,
                *text,
                TypographyRole::Body,
                *token,
                (width - sp(Spacing::S8)).max(0.0),
            );
            cell
        })
        .collect()
}

/// A text action placed in a table cell.
fn cell_action(ui: &mut Ui, cell: Rect, id: impl std::hash::Hash, label: &str) -> Response {
    ui.push_id(id, |ui| {
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(cell)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        action(&mut child, label, None, true)
    })
    .inner
}

/// `Intel only` from `intel only`: the first letter upper case.
fn capitalized(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}
