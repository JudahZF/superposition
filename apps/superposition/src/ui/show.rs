//! The frame around both rack views: the top bar, the fault banner, and the floating scene bar,
//! plus the rack controls the overview and the rack view share (meters, the gain fader, route
//! chips, and inline rename).

use eframe::egui::{
    self, Align, Align2, Frame, Layout as EguiLayout, Margin, Pos2, Rect, Response, Sense, Ui,
    UiBuilder, Vec2, WidgetInfo, WidgetType,
};
use sp_model::SlotSidechain;
use sp_ui::components::{Segment, SystemStatusState, led_segment};
use sp_ui::design::{ColorToken, Layout, Radius, Spacing, TypographyRole};

use super::style::{
    ButtonKind, ChipTone, action, button, chip_at, chip_width, color, focus_outline, font, galley,
    hairline, paint_text, pill, px, radius, segmented, sp, text,
};
use super::{LiveRackApp, Modal, Overlay, Screen, View, channels_label, gain_label};

/// Rack gain range, in dB.
pub(super) const GAIN_MIN_DB: f32 = -60.0;
pub(super) const GAIN_MAX_DB: f32 = 12.0;
/// Gain drag steps: coarse, and fine with Shift.
const GAIN_STEP_DB: f32 = 0.5;
const GAIN_FINE_STEP_DB: f32 = 0.1;
/// Meter column labels: rack input, then output left and right.
const METER_LABELS: [&str; 3] = ["in", "L", "R"];

/// Symmetric frame margins from spacing tokens.
pub(super) fn margin(horizontal: Spacing, vertical: Spacing) -> Margin {
    let pixels = |spacing: Spacing| i8::try_from(spacing.pixels()).unwrap_or(i8::MAX);
    Margin::symmetric(pixels(horizontal), pixels(vertical))
}

/// Vertical fraction of a gain on the −60 to +12 dB fader.
fn gain_fraction(gain_db: f32) -> f32 {
    ((gain_db - GAIN_MIN_DB) / (GAIN_MAX_DB - GAIN_MIN_DB)).clamp(0.0, 1.0)
}

/// Which route chips to draw.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum RouteChips {
    Input,
    Output,
    Both,
}

impl LiveRackApp {
    /// Shows a view of the racks, leaving the setup page.
    pub(super) fn show_view(&mut self, view: View) {
        self.screen = Screen::Show;
        self.view = view;
        self.scroll_to_selected = true;
    }

    /// Wordmark, session, rack selector, and tabs at the left; status, engine readouts, and the
    /// engine and save actions at the right.
    pub(super) fn draw_top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_bar")
            .exact_height(px(Layout::TopBar))
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(color(ColorToken::Base))
                    .inner_margin(margin(Spacing::S12, Spacing::S12)),
            )
            .show(ctx, |ui| {
                let rule = ui
                    .max_rect()
                    .expand2(Vec2::new(sp(Spacing::S12), sp(Spacing::S12)));
                ui.painter().hline(
                    rule.x_range(),
                    rule.bottom() - px(Layout::Hairline) / 2.0,
                    hairline(ColorToken::Hairline),
                );
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = sp(Spacing::S12);
                    text(
                        ui,
                        "Superposition",
                        TypographyRole::Wordmark,
                        ColorToken::Text,
                    );
                    self.draw_session_name(ui);
                    self.draw_rack_selector(ui);
                    let selected = match (self.screen, self.view) {
                        (Screen::Setup, _) => 2,
                        (Screen::Show, View::Overview) => 0,
                        (Screen::Show, View::Rack) => 1,
                    };
                    match segmented(ui, &["Overview", "Rack", "Setup"], selected) {
                        Some(0) => self.show_view(View::Overview),
                        Some(1) => self.show_view(View::Rack),
                        Some(_) => self.screen = Screen::Setup,
                        None => {}
                    }
                    ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = sp(Spacing::S8);
                        if button(ui, "Save", Some("⌘S"), ButtonKind::Primary, true).clicked() {
                            self.save_session();
                        }
                        let engine = if self.online() {
                            "Stop engine"
                        } else {
                            "Start engine"
                        };
                        if button(ui, engine, None, ButtonKind::Outline, true).clicked() {
                            self.request_engine_toggle();
                        }
                        ui.add_space(sp(Spacing::S8));
                        ui.spacing_mut().item_spacing.x = sp(Spacing::S12);
                        for (value, token) in self.readouts().into_iter().rev() {
                            text(ui, value, TypographyRole::Mono, token);
                        }
                        let (state, dot) = match self.system.state {
                            SystemStatusState::Online => ("Online", ColorToken::Success),
                            SystemStatusState::Connecting => ("Connecting", ColorToken::Warn),
                            SystemStatusState::Offline => ("Offline", ColorToken::Dim),
                        };
                        pill(
                            ui,
                            dot,
                            &format!("{state} · {}", self.output_device_name()),
                            px(Layout::TopBarTextWidth),
                        );
                        let message = self.saved_at.map_or_else(
                            || self.status_line.clone(),
                            |at| format!("Session saved, {}", elapsed_label(at.elapsed())),
                        );
                        let width = ui.available_width().min(px(Layout::TopBarTextWidth));
                        if width > sp(Spacing::S24) {
                            let galley =
                                galley(ui, message, TypographyRole::Label, ColorToken::Dim, width);
                            let (rect, _) = ui.allocate_exact_size(galley.size(), Sense::hover());
                            ui.painter()
                                .galley(rect.min, galley, color(ColorToken::Dim));
                        }
                    });
                });
            });
    }

    /// The session name, then `· unsaved` while it has unsaved changes.
    fn draw_session_name(&self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing.x = sp(Spacing::S4);
        let name = self.session_name();
        let width = px(Layout::TopBarTextWidth) / 2.0;
        let galley = galley(ui, name, TypographyRole::Strong, ColorToken::Text, width);
        let (rect, _) = ui.allocate_exact_size(galley.size(), Sense::hover());
        ui.painter()
            .galley(rect.min, galley, color(ColorToken::Text));
        if self.controller.is_dirty() {
            text(ui, "· unsaved", TypographyRole::Label, ColorToken::Dim);
        }
        ui.spacing_mut().item_spacing.x = sp(Spacing::S12);
    }

    /// Engine readouts in reading order: rate, buffer, latency, load, misses, and restarts when
    /// any worker restarted.
    fn readouts(&self) -> Vec<(String, ColorToken)> {
        let frames = self.buffer_frames_value();
        let rate = self.sample_rate_value();
        let mut readouts = vec![
            (format!("{} kHz", rate / 1000), ColorToken::Dim),
            (format!("{frames} smp"), ColorToken::Dim),
            (latency_label(frames, rate), ColorToken::Dim),
            (
                self.callback_load().map_or_else(
                    || "Load --".to_owned(),
                    |load| format!("Load {:.0} %", load * 100.0),
                ),
                ColorToken::Dim,
            ),
            (
                self.deadline_misses().map_or_else(
                    || "Misses --".to_owned(),
                    |misses| format!("Misses {misses}"),
                ),
                ColorToken::Dim,
            ),
        ];
        let racks = self.controller.document().model.racks.len();
        let restarts: u32 = self.product.as_ref().map_or(0, |product| {
            (0..racks)
                .map(|rack| product.rack_restart_count(rack))
                .sum()
        });
        if restarts > 0 {
            readouts.push((format!("Restarts {restarts}"), ColorToken::Warn));
        }
        readouts
    }

    /// `‹ 01 Vox ›`: arrows step through the shown racks, click opens the rack menu, and
    /// double-click renames. The rack view renames here.
    #[allow(
        clippy::too_many_lines,
        reason = "arrows, then the name or its rename field, in one control"
    )]
    fn draw_rack_selector(&mut self, ui: &mut Ui) {
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(px(Layout::RackSelectorWidth), px(Layout::Control)),
            Sense::hover(),
        );
        ui.painter().rect_stroke(
            rect,
            radius(Radius::Control),
            hairline(ColorToken::Hairline),
            egui::StrokeKind::Inside,
        );
        let id = ui.id().with("rack_selector");
        let visible = self.visible_racks();
        let position = visible.iter().position(|rack| *rack == self.selected_rack);
        let arrow = Vec2::splat(px(Layout::Control));
        let previous = Rect::from_min_size(rect.min, arrow);
        let next = Rect::from_min_size(Pos2::new(rect.right() - arrow.x, rect.top()), arrow);
        let step = [
            (
                previous,
                "‹",
                "Previous rack",
                position.and_then(|p| p.checked_sub(1)),
            ),
            (
                next,
                "›",
                "Next rack",
                position.map(|p| p + 1).filter(|p| *p < visible.len()),
            ),
        ];
        for (area, glyph, label, target) in step {
            let response = ui.interact(area, id.with(label), Sense::click());
            let enabled = target.is_some();
            if enabled && response.hovered() {
                ui.painter().rect_filled(
                    area.shrink(sp(Spacing::S2)),
                    radius(Radius::Small),
                    color(ColorToken::HoverFill),
                );
            }
            paint_text(
                ui,
                area.center(),
                Align2::CENTER_CENTER,
                glyph,
                TypographyRole::Title,
                if enabled {
                    ColorToken::Text
                } else {
                    ColorToken::Faint
                },
                area.width(),
            );
            response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
            if let Some(rack) = target.filter(|_| response.clicked()) {
                self.select_rack(rack);
                self.scroll_to_selected = true;
            }
        }
        let middle = Rect::from_min_max(
            Pos2::new(previous.right(), rect.top()),
            Pos2::new(next.left(), rect.bottom()),
        );
        let Some(rack) = self
            .controller
            .document()
            .model
            .racks
            .get(self.selected_rack)
            .cloned()
        else {
            paint_text(
                ui,
                middle.center(),
                Align2::CENTER_CENTER,
                "No racks",
                TypographyRole::Body,
                ColorToken::Faint,
                middle.width(),
            );
            return;
        };
        let renaming = self.screen == Screen::Show
            && self.view == View::Rack
            && self
                .rename
                .as_ref()
                .is_some_and(|(renaming, _)| *renaming == self.selected_rack);
        if renaming {
            self.rename_field(ui, middle.shrink2(Vec2::Y * sp(Spacing::S4)));
            return;
        }
        let index = paint_text(
            ui,
            Pos2::new(middle.left() + sp(Spacing::S4), middle.center().y),
            Align2::LEFT_CENTER,
            format!("{:02}", self.selected_rack + 1),
            TypographyRole::Mono,
            ColorToken::Dim,
            middle.width(),
        );
        paint_text(
            ui,
            Pos2::new(index.right() + sp(Spacing::S6), middle.center().y),
            Align2::LEFT_CENTER,
            &rack.name,
            TypographyRole::Strong,
            ColorToken::Text,
            (middle.right() - index.right() - sp(Spacing::S8)).max(0.0),
        );
        let response = ui.interact(middle, id.with("name"), Sense::click());
        response.widget_info(|| {
            WidgetInfo::labeled(WidgetType::Button, true, format!("Rack {}", rack.name))
        });
        if response.double_clicked() {
            self.show_view(View::Rack);
            self.rename = Some((self.selected_rack, rack.name.clone()));
        } else if response.clicked() {
            self.overlay = Overlay::RackMenu {
                rack: self.selected_rack,
                at: rect.left_bottom() + Vec2::Y * sp(Spacing::S4),
            };
        }
    }

    /// The inline rename field for the rack being renamed, filling `rect`.
    pub(super) fn rename_field(&mut self, ui: &mut Ui, rect: Rect) {
        let Some((_, draft)) = &mut self.rename else {
            return;
        };
        let response = ui.put(
            rect,
            egui::TextEdit::singleline(draft)
                .font(font(TypographyRole::Strong))
                .margin(margin(Spacing::S4, Spacing::S2))
                .desired_width(rect.width()),
        );
        if !response.has_focus() && !response.lost_focus() {
            response.request_focus();
        }
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.rename = None;
        } else if response.lost_focus() {
            self.commit_rename();
        }
    }

    pub(super) fn output_device_name(&self) -> String {
        #[cfg(audio_backend)]
        if let Some(route) = &self.selected_route
            && let Some(device) = self
                .audio_devices
                .iter()
                .find(|device| device.info.id == route.output)
        {
            return device.info.name.clone();
        }
        "no device".to_owned()
    }

    pub(super) fn buffer_frames_value(&self) -> u32 {
        #[cfg(audio_backend)]
        return self.buffer_frames;
        #[cfg(not(audio_backend))]
        128
    }

    pub(super) fn sample_rate_value(&self) -> u32 {
        #[cfg(audio_backend)]
        return self.sample_rate_hz;
        #[cfg(not(audio_backend))]
        sp_audio_io::PRODUCT_SAMPLE_RATE_HZ
    }

    /// A rounded fault-tinted banner under the top bar: `Fault`, the message, and Dismiss.
    pub(super) fn draw_fault_banner(&mut self, ctx: &egui::Context) {
        let Some(message) = self.fault.clone() else {
            return;
        };
        egui::TopBottomPanel::top("fault_banner")
            .exact_height(px(Layout::FaultBanner) + sp(Spacing::S12))
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(color(ColorToken::Base))
                    .inner_margin(Margin {
                        bottom: 0,
                        ..margin(Spacing::S12, Spacing::S12)
                    }),
            )
            .show(ctx, |ui| {
                let banner = ui.max_rect();
                ui.painter().rect_filled(
                    banner,
                    radius(Radius::Card),
                    color(ColorToken::FaultTint),
                );
                ui.painter().rect_stroke(
                    banner,
                    radius(Radius::Card),
                    egui::Stroke::new(
                        px(Layout::Hairline),
                        color(ColorToken::Fault).gamma_multiply(0.45),
                    ),
                    egui::StrokeKind::Inside,
                );
                let mut row = ui.new_child(
                    UiBuilder::new()
                        .max_rect(banner.shrink2(Vec2::new(sp(Spacing::S12), 0.0)))
                        .layout(EguiLayout::left_to_right(Align::Center)),
                );
                row.spacing_mut().item_spacing.x = sp(Spacing::S10);
                text(&mut row, "Fault", TypographyRole::Strong, ColorToken::Fault);
                row.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                    if button(ui, "Dismiss", Some("Esc"), ButtonKind::Ghost, true).clicked() {
                        self.fault = None;
                    }
                    ui.with_layout(EguiLayout::left_to_right(Align::Center), |ui| {
                        let width = ui.available_width();
                        let galley =
                            galley(ui, message, TypographyRole::Body, ColorToken::Text, width);
                        let (rect, response) =
                            ui.allocate_exact_size(galley.size(), Sense::hover());
                        ui.painter()
                            .galley(rect.min, galley, color(ColorToken::Text));
                        response
                            .widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, "Fault"));
                    });
                });
            });
    }

    /// The floating scene bar: scene pads, the fade stepper, Update and Edit, then callback
    /// load and Capture at the right.
    pub(super) fn draw_scene_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("scene_bar")
            .exact_height(px(Layout::SceneBar) + sp(Spacing::S12))
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(color(ColorToken::Base))
                    .inner_margin(Margin {
                        top: 0,
                        ..margin(Spacing::S12, Spacing::S12)
                    }),
            )
            .show(ctx, |ui| {
                let bar = ui.max_rect();
                let shadow = egui::Shadow {
                    offset: [0, i8::try_from(Spacing::S6.pixels()).unwrap_or(i8::MAX)],
                    blur: Spacing::S24.pixels(),
                    spread: 0,
                    color: color(ColorToken::Base).gamma_multiply(0.8),
                };
                ui.painter().add(shadow.as_shape(bar, radius(Radius::Bar)));
                ui.painter()
                    .rect_filled(bar, radius(Radius::Bar), color(ColorToken::Surface));
                ui.painter().rect_stroke(
                    bar,
                    radius(Radius::Bar),
                    hairline(ColorToken::Hairline),
                    egui::StrokeKind::Inside,
                );
                let mut row = ui.new_child(
                    UiBuilder::new()
                        .max_rect(bar.shrink2(Vec2::new(sp(Spacing::S10), 0.0)))
                        .layout(EguiLayout::left_to_right(Align::Center)),
                );
                self.draw_scene_controls(&mut row);
            });
    }

    fn draw_scene_controls(&mut self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing.x = sp(Spacing::S4);
        text(ui, "Scene", TypographyRole::Label, ColorToken::Dim);
        ui.add_space(sp(Spacing::S6));
        let online = self.online();
        let names: Vec<String> = self
            .controller
            .document()
            .model
            .scenes
            .iter()
            .map(|scene| scene.name.clone())
            .collect();
        for (index, name) in names.iter().enumerate() {
            let active = self.current_scene == Some(index);
            if pad(ui, "Scene", Some(index + 1), name, active, online).clicked() {
                self.recall_scene(index);
            }
        }
        if names.is_empty() {
            text(
                ui,
                "No scenes yet",
                TypographyRole::Label,
                ColorToken::Faint,
            );
        }
        divider(ui);
        text(ui, "Fade", TypographyRole::Label, ColorToken::Dim);
        ui.add_space(sp(Spacing::S4));
        if action(ui, "−", None, self.fade_ms > 0).clicked() {
            self.step_fade(-1);
        }
        text(
            ui,
            format!("{} ms", self.fade_ms),
            TypographyRole::Mono,
            ColorToken::Text,
        );
        if action(ui, "+", None, self.fade_ms < super::FADE_MAX_MS).clicked() {
            self.step_fade(1);
        }
        divider(ui);
        let scene_open = online && self.current_scene.is_some();
        if button(ui, "Update", None, ButtonKind::Ghost, scene_open).clicked() {
            self.update_current_scene();
        }
        if button(ui, "Edit", None, ButtonKind::Ghost, scene_open).clicked()
            && let Some(index) = self.current_scene
            && let Some(scene) = self.controller.document().model.scenes.get(index).cloned()
        {
            self.open_scene_editor(scene, Some(index));
        }
        ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = sp(Spacing::S8);
            if button(ui, "Capture", Some("⌘⇧C"), ButtonKind::Primary, online).clicked() {
                self.capture_scene();
            }
            ui.add_space(sp(Spacing::S8));
            self.draw_load_bars(ui);
            text(ui, "Callback", TypographyRole::Label, ColorToken::Dim);
        });
    }

    /// Twelve 3 px bars of recent callback load; a bar over 90 % is warn colour.
    fn draw_load_bars(&self, ui: &mut Ui) {
        const HOT_LOAD: f32 = 0.9;
        let bar = px(Layout::LoadBarWidth);
        let gap = sp(Spacing::S2);
        let height = px(Layout::LoadBarHeight);
        #[allow(clippy::cast_precision_loss, reason = "twelve bars")]
        let width = super::LOAD_BARS as f32 * (bar + gap) - gap;
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let online = self.online();
        for (index, load) in self.load_history.iter().enumerate() {
            #[allow(clippy::cast_precision_loss, reason = "twelve bars")]
            let left = rect.left() + index as f32 * (bar + gap);
            let bar_height = if online {
                (load * height).clamp(px(Layout::Hairline) * 2.0, height)
            } else {
                px(Layout::Hairline) * 2.0
            };
            let token = if online && *load > HOT_LOAD {
                ColorToken::Warn
            } else {
                ColorToken::Dim
            };
            ui.painter().rect_filled(
                Rect::from_min_max(
                    Pos2::new(left, rect.bottom() - bar_height),
                    Pos2::new(left + bar, rect.bottom()),
                ),
                radius(Radius::Small),
                color(token),
            );
        }
        response.widget_info(|| {
            let mut info =
                WidgetInfo::labeled(WidgetType::ProgressIndicator, true, "Callback load");
            info.value = self.callback_load().map(f64::from);
            info
        });
    }

    /// Rack input level, then output left and right, each with its held peak.
    pub(super) fn rack_levels(&self, rack: usize) -> [(f32, f32); 3] {
        #[cfg(audio_backend)]
        {
            let (input, output) = (self.rack_input_meters[rack], self.rack_output_meters[rack]);
            [
                (input.fill_fraction(), input.held_fraction()),
                (output.channel_fraction(0), output.held_fraction()),
                (output.channel_fraction(1), output.held_fraction()),
            ]
        }
        #[cfg(not(audio_backend))]
        {
            let _ = rack;
            [(0.0, 0.0); 3]
        }
    }

    /// LED columns for `columns` (0 in, 1 L, 2 R), left to right from `left`, each `width`
    /// wide and spanning `top` to `bottom`, with labels beneath. Returns the right edge.
    pub(super) fn draw_meters(
        &self,
        ui: &Ui,
        rack: usize,
        columns: &[usize],
        (left, top, bottom): (f32, f32, f32),
        width: f32,
    ) -> f32 {
        let online = self.online();
        let levels = self.rack_levels(rack);
        let label_height = px(Layout::Caption);
        let meter_bottom = bottom - label_height - sp(Spacing::S2);
        let mut x = left;
        for &column in columns {
            let (level, held) = levels[column];
            led_meter(
                ui,
                Rect::from_min_max(Pos2::new(x, top), Pos2::new(x + width, meter_bottom)),
                level,
                held,
                online,
            );
            paint_text(
                ui,
                Pos2::new(x + width / 2.0, bottom - label_height / 2.0),
                Align2::CENTER_CENTER,
                METER_LABELS[column],
                TypographyRole::MonoSmall,
                ColorToken::Dim,
                width * 3.0,
            );
            x += width + sp(Spacing::S4);
        }
        let meters = Rect::from_min_max(Pos2::new(left, top), Pos2::new(x, bottom));
        ui.interact(
            meters,
            ui.id().with(("meters", rack, columns)),
            Sense::hover(),
        )
        .widget_info(|| {
            let mut info = WidgetInfo::labeled(WidgetType::ProgressIndicator, true, "Rack level");
            info.value = Some(f64::from(levels[1].0.max(levels[2].0)));
            info
        });
        x - sp(Spacing::S4)
    }

    /// A vertical gain fader filling `rect`: a rounded track, a 0 dB tick, and the thumb. Click
    /// or drag sets the gain, Shift drags finely, arrows step it, and double-click resets it.
    pub(super) fn draw_gain_fader(&mut self, ui: &mut Ui, id: egui::Id, rect: Rect, rack: usize) {
        let Some(model_rack) = self.controller.document().model.racks.get(rack).cloned() else {
            return;
        };
        let gain_db = model_rack.gain_db.get();
        let track = Rect::from_center_size(
            rect.center(),
            Vec2::new(
                px(Layout::FaderTrack),
                rect.height() - px(Layout::FaderThumbHeight),
            ),
        );
        let response = ui.interact(rect, id.with("gain"), Sense::click_and_drag());
        let painter = ui.painter();
        painter.rect_filled(track, radius(Radius::Pill), color(ColorToken::SegmentOff));
        let y_for = |db: f32| track.bottom() - track.height() * gain_fraction(db);
        let zero = y_for(0.0);
        painter.hline(
            track.center().x - px(Layout::FaderTick) / 2.0
                ..=track.center().x + px(Layout::FaderTick) / 2.0,
            zero,
            hairline(ColorToken::Dim),
        );
        let thumb = Rect::from_center_size(
            Pos2::new(track.center().x, y_for(gain_db)),
            Vec2::new(px(Layout::FaderThumbWidth), px(Layout::FaderThumbHeight)),
        );
        painter.rect_filled(thumb, radius(Radius::Small), color(ColorToken::Text));
        painter.hline(
            thumb.shrink2(Vec2::new(sp(Spacing::S6), 0.0)).x_range(),
            thumb.center().y,
            hairline(ColorToken::Surface),
        );
        focus_outline(ui, &response, rect);
        response.widget_info(|| {
            WidgetInfo::slider(
                true,
                f64::from(gain_db),
                format!("{} gain", model_rack.name),
            )
        });
        let target = gain_target(ui, &response, track, gain_db);
        if let Some(db) = target.map(|db| db.clamp(GAIN_MIN_DB, GAIN_MAX_DB))
            && (db - gain_db).abs() > f32::EPSILON
        {
            self.set_rack_controls(rack, db, model_rack.muted, model_rack.bypassed);
        }
    }

    /// The rack's `In` and `Out` chips from `pos`, wrapping inside `max_width`. Clicking a chip
    /// opens the route popover; an output or input the device lacks is warn coloured. Returns
    /// the bottom edge.
    pub(super) fn draw_route_chips(
        &mut self,
        ui: &mut Ui,
        id: egui::Id,
        pos: Pos2,
        max_width: f32,
        rack: usize,
        which: RouteChips,
    ) -> f32 {
        let Some(model_rack) = self.controller.document().model.racks.get(rack).cloned() else {
            return pos.y;
        };
        let route = super::rack_route(&self.controller.document().model, &model_rack);
        let tone = if self.route_fits(route) {
            ChipTone::Neutral
        } else {
            ChipTone::Warn
        };
        let mut chips = Vec::new();
        if which != RouteChips::Output {
            chips.push(format!(
                "In {}",
                route
                    .input
                    .map_or_else(|| "none".to_owned(), channels_label)
            ));
        }
        if which != RouteChips::Input {
            chips.push(format!("Out {}", channels_label(route.output)));
        }
        let height = px(Layout::Chip);
        let gap = sp(Spacing::S4);
        let mut at = pos;
        for label in chips {
            let width = chip_width(ui, &label).min(max_width);
            if at.x + width > pos.x + max_width && at.x > pos.x {
                at = Pos2::new(pos.x, at.y + height + gap);
            }
            let rect = Rect::from_min_size(at, Vec2::new(width, height));
            if chip_at(ui, rect, id.with(&label), &label, tone, true).clicked() {
                self.select_rack(rack);
                self.overlay = Overlay::Route {
                    rack,
                    at: rect.left_bottom() + Vec2::Y * sp(Spacing::S4),
                };
            }
            at.x += width + gap;
        }
        at.y + height
    }

    fn commit_rename(&mut self) {
        let Some((rack, draft)) = self.rename.take() else {
            return;
        };
        let name = draft.trim();
        if name.is_empty() {
            return;
        }
        if let Some(model_rack) = self.controller.document_mut().model.racks.get_mut(rack)
            && model_rack.name != name
        {
            name.clone_into(&mut model_rack.name);
            self.set_status("Rack renamed");
        }
    }

    /// `SC Drums` or `SC in 7-8` when a slot has a sidechain.
    pub(super) fn sidechain_marker(&self, slot: &sp_model::PluginSlot) -> Option<String> {
        Some(match slot.sidechain.as_ref()? {
            SlotSidechain::PhysicalInput(channels) => {
                format!("SC in {}", channels_label(*channels))
            }
            SlotSidechain::RackOutput(rack_id) => format!(
                "SC {}",
                self.controller
                    .document()
                    .model
                    .racks
                    .iter()
                    .find(|rack| rack.id == *rack_id)
                    .map_or("missing rack", |rack| rack.name.as_str())
            ),
        })
    }

    pub(super) fn open_remove_rack(&mut self, rack: usize) {
        self.select_rack(rack);
        self.overlay = Overlay::Modal(Modal::RemoveRack(rack));
    }
}

/// One LED column filling `rect` with 2 px segments on a 4 px pitch, the top eighth in fault
/// colour. Offline, lit segments drop to faint.
pub(super) fn led_meter(ui: &Ui, rect: Rect, level: f32, held: f32, online: bool) {
    let pitch = sp(Spacing::S4);
    let segment = sp(Spacing::S2);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a meter is at most a screen tall"
    )]
    let count = ((rect.height() + pitch - segment) / pitch).floor().max(1.0) as usize;
    for index in 0..count {
        #[allow(
            clippy::cast_precision_loss,
            reason = "a meter is at most a screen tall"
        )]
        let bottom = rect.bottom() - index as f32 * pitch;
        let token = match (led_segment(level, held, index, count), online) {
            (Segment::Off, _) => ColorToken::SegmentOff,
            (_, false) => ColorToken::Faint,
            (Segment::Lit, true) => ColorToken::Text,
            (Segment::Hot, true) => ColorToken::Fault,
        };
        ui.painter().rect_filled(
            Rect::from_min_max(
                Pos2::new(rect.left(), bottom - segment),
                Pos2::new(rect.right(), bottom),
            ),
            egui::CornerRadius::ZERO,
            color(token),
        );
    }
}

/// A thin vertical divider between groups in a row.
fn divider(ui: &mut Ui) {
    ui.add_space(sp(Spacing::S8));
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(px(Layout::Hairline), px(Layout::Chip)),
        Sense::hover(),
    );
    ui.painter().vline(
        rect.center().x,
        rect.y_range(),
        hairline(ColorToken::Hairline),
    );
    ui.add_space(sp(Spacing::S8));
}

/// A scene pad: a rounded pill with `n Name`, accent while active, faint while disabled. `kind`
/// names it for assistive technology.
fn pad(
    ui: &mut Ui,
    kind: &str,
    number: Option<usize>,
    name: &str,
    active: bool,
    enabled: bool,
) -> Response {
    let number_text = number.map(|number| number.to_string());
    let gap = sp(Spacing::S4);
    let number_width = number_text.as_ref().map_or(0.0, |text| {
        galley(
            ui,
            text,
            TypographyRole::Mono,
            ColorToken::Dim,
            f32::INFINITY,
        )
        .size()
        .x + gap
    });
    let name_width = galley(
        ui,
        name,
        TypographyRole::Body,
        ColorToken::Text,
        f32::INFINITY,
    )
    .size()
    .x;
    let pad = sp(Spacing::S10);
    let size = Vec2::new(pad * 2.0 + number_width + name_width, px(Layout::Control));
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let hovered = enabled && response.hovered();
    let (fill, number_token, name_token) = match (enabled, active, hovered) {
        (false, _, _) => (None, ColorToken::Faint, ColorToken::Faint),
        (true, true, _) => (
            Some(ColorToken::Accent),
            ColorToken::AccentText,
            ColorToken::AccentText,
        ),
        (true, false, true) => (
            Some(ColorToken::HoverFill),
            ColorToken::Dim,
            ColorToken::Text,
        ),
        (true, false, false) => (None, ColorToken::Dim, ColorToken::Dim),
    };
    if let Some(fill) = fill {
        ui.painter()
            .rect_filled(rect, radius(Radius::Pill), color(fill));
    }
    let mut left = rect.left() + pad;
    if let Some(number_text) = &number_text {
        let placed = paint_text(
            ui,
            Pos2::new(left, rect.center().y),
            Align2::LEFT_CENTER,
            number_text,
            TypographyRole::Mono,
            number_token,
            rect.width(),
        );
        left = placed.right() + gap;
    }
    paint_text(
        ui,
        Pos2::new(left, rect.center().y),
        Align2::LEFT_CENTER,
        name,
        TypographyRole::Body,
        name_token,
        rect.width(),
    );
    focus_outline(ui, &response, rect);
    response.widget_info(|| {
        let label = match number {
            Some(number) => format!("{kind} {number} {name}"),
            None => format!("{kind} {name}"),
        };
        WidgetInfo::selected(WidgetType::Button, enabled, active, label)
    });
    response
}

/// The gain a click, drag, or arrow key on the vertical track asks for.
fn gain_target(ui: &Ui, response: &Response, track: Rect, gain_db: f32) -> Option<f32> {
    let step = if ui.input(|input| input.modifiers.shift) {
        GAIN_FINE_STEP_DB
    } else {
        GAIN_STEP_DB
    };
    if response.double_clicked() {
        Some(0.0)
    } else if (response.clicked() || response.dragged())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let fraction = ((track.bottom() - pointer.y) / track.height()).clamp(0.0, 1.0);
        let db = GAIN_MIN_DB + fraction * (GAIN_MAX_DB - GAIN_MIN_DB);
        Some((db / step).round() * step)
    } else if response.has_focus() {
        ui.input(|input| {
            if input.key_pressed(egui::Key::ArrowUp) {
                Some(gain_db + step)
            } else if input.key_pressed(egui::Key::ArrowDown) {
                Some(gain_db - step)
            } else {
                None
            }
        })
    } else {
        None
    }
}

/// The pointer position for a context menu, or `fallback` for keyboard-opened menus.
pub(super) fn pointer_or(ui: &Ui, fallback: Pos2) -> Pos2 {
    ui.input(|input| input.pointer.interact_pos())
        .unwrap_or(fallback)
}

/// `4 s ago`, `2 min ago`, or `1 h ago`.
pub(super) fn elapsed_label(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds} s ago")
    } else if seconds < 3600 {
        format!("{} min ago", seconds / 60)
    } else {
        format!("{} h ago", seconds / 3600)
    }
}

/// Buffer latency, as `1.3 ms`.
pub(super) fn latency_label(frames: u32, sample_rate_hz: u32) -> String {
    format!(
        "{:.1} ms",
        f64::from(frames) * 1_000.0 / f64::from(sample_rate_hz)
    )
}

/// Paints the shared `gain_label` value in mono, anchored at `pos`.
pub(super) fn paint_gain(ui: &Ui, pos: Pos2, anchor: Align2, gain_db: f32, max_width: f32) -> Rect {
    paint_text(
        ui,
        pos,
        anchor,
        gain_label(gain_db),
        TypographyRole::Mono,
        ColorToken::Text,
        max_width,
    )
}
