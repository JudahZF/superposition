//! The show screen: head readouts, the fault line, one column per rack, and the foot line.

use eframe::egui::{
    self, Align, Align2, Frame, Layout as EguiLayout, Margin, Pos2, Rect, Response, Sense, Ui,
    UiBuilder, Vec2, WidgetInfo, WidgetType,
};
use sp_model::{MAX_RACKS, SlotSidechain};
use sp_ui::components::{LED_SEGMENTS, Segment, StateToken, SystemStatusState, led_segment};
use sp_ui::design::{ColorToken, Layout, Spacing, TypographyRole};

use super::previews::Tile;
use super::style::{
    action, action_with, color, dashed_rect, font, galley, hairline, line, paint_text, px, readout,
    sp, toggle,
};
use super::{LiveRackApp, Modal, Overlay, Picker, Screen, SlotAction, channels_label, gain_label};

/// Rack gain range, in dB.
pub(super) const GAIN_MIN_DB: f32 = -60.0;
pub(super) const GAIN_MAX_DB: f32 = 12.0;
/// Gain drag steps: coarse, and fine with Shift.
const GAIN_STEP_DB: f32 = 0.5;
const GAIN_FINE_STEP_DB: f32 = 0.1;
/// Preview slots per column; every column shares one rhythm.
const PREVIEW_SLOTS: usize = sp_model::MAX_SLOTS_PER_RACK;

fn margin(horizontal: Spacing, vertical: Spacing) -> Margin {
    let pixels = |spacing: Spacing| i8::try_from(spacing.pixels()).unwrap_or(i8::MAX);
    Margin::symmetric(pixels(horizontal), pixels(vertical))
}

/// Horizontal fraction of a gain on the −60 to +12 dB track.
fn gain_fraction(gain_db: f32) -> f32 {
    ((gain_db - GAIN_MIN_DB) / (GAIN_MAX_DB - GAIN_MIN_DB)).clamp(0.0, 1.0)
}

/// Vertical extents of the column parts below the previews, top to bottom.
struct ColumnFoot {
    gain: f32,
    meters: f32,
    labels: f32,
    toggles: f32,
    status: f32,
}

impl ColumnFoot {
    fn new() -> Self {
        Self {
            // Padding, the label row, the track with its margins, and the tick labels.
            gain: sp(Spacing::S8)
                + line()
                + sp(Spacing::S8)
                + px(Layout::GainTrack)
                + sp(Spacing::S4)
                + px(Layout::Caption),
            meters: sp(Spacing::S10) + px(Layout::MeterHeight) + sp(Spacing::S4),
            labels: px(Layout::Caption),
            toggles: sp(Spacing::S8) + line() + px(Layout::Hairline) * 2.0,
            status: sp(Spacing::S8) + line(),
        }
    }

    fn height(&self) -> f32 {
        self.gain + self.meters + self.labels + self.toggles + self.status
    }
}

/// Room above the previews: the header row and the route line.
fn column_head_height() -> f32 {
    line() + sp(Spacing::S2) + line() + sp(Spacing::S10)
}

/// Preview height that fits eight slots in the column: 84 px at 1920×1080, down to 60 px on
/// the laptop preset or while the fault line shows.
fn preview_height(column_height: f32) -> f32 {
    let gaps = sp(Spacing::S4) * 7.0;
    #[allow(clippy::cast_precision_loss, reason = "eight preview slots")]
    let slots = PREVIEW_SLOTS as f32;
    let available = column_height
        - sp(Spacing::S12) * 2.0
        - column_head_height()
        - ColumnFoot::new().height()
        - gaps;
    (available / slots)
        .floor()
        .clamp(px(Layout::PreviewMinHeight), px(Layout::PreviewMaxHeight))
}

impl LiveRackApp {
    pub(super) fn draw_head(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("head")
            .exact_height(px(Layout::Head))
            .frame(
                Frame::new()
                    .fill(color(ColorToken::Base))
                    .inner_margin(margin(Spacing::S20, Spacing::S12)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = sp(Spacing::S24);
                    super::style::text(
                        ui,
                        "Superposition",
                        TypographyRole::Wordmark,
                        ColorToken::Text,
                    );
                    self.draw_readouts(ui);
                    ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = sp(Spacing::S20);
                        let setup_open = self.screen == Screen::Setup;
                        let label = if setup_open { "Close setup" } else { "Setup" };
                        if action_with(ui, label, Some("⌘,"), true, setup_open).clicked() {
                            self.screen = if setup_open {
                                Screen::Show
                            } else {
                                Screen::Setup
                            };
                        }
                        if action(ui, "Save", Some("⌘S"), true).clicked() {
                            self.save_session();
                        }
                        let engine = if self.online() {
                            "Stop engine"
                        } else {
                            "Start engine"
                        };
                        if action(ui, engine, None, true).clicked() {
                            self.request_engine_toggle();
                        }
                        let message = self.saved_at.map_or_else(
                            || self.status_line.clone(),
                            |at| format!("Session saved, {}", elapsed_label(at.elapsed())),
                        );
                        let width = ui.available_width();
                        let galley =
                            galley(ui, message, TypographyRole::Body, ColorToken::Dim, width);
                        let (rect, _) = ui.allocate_exact_size(galley.size(), Sense::hover());
                        ui.painter()
                            .galley(rect.min, galley, color(ColorToken::Dim));
                    });
                });
            });
    }

    fn draw_readouts(&self, ui: &mut Ui) {
        let (engine, engine_token) = match self.system.state {
            SystemStatusState::Online => ("Online", ColorToken::Text),
            SystemStatusState::Connecting => ("Connecting", ColorToken::Warn),
            SystemStatusState::Offline => ("Offline", ColorToken::Warn),
        };
        readout(ui, "Engine", engine, engine_token, f32::INFINITY);
        readout(
            ui,
            "Device",
            &self.output_device_name(),
            ColorToken::Text,
            px(Layout::SessionNameWidth),
        );
        readout(ui, "Rate", "48 000", ColorToken::Text, f32::INFINITY);
        let frames = self.buffer_frames_value();
        readout(
            ui,
            "Buffer",
            &format!("{frames} smp"),
            ColorToken::Text,
            f32::INFINITY,
        );
        readout(
            ui,
            "Latency",
            &latency_label(frames),
            ColorToken::Text,
            f32::INFINITY,
        );
        let load = self
            .callback_load()
            .map_or_else(|| "--".to_owned(), |load| format!("{:.0} %", load * 100.0));
        readout(ui, "Load", &load, ColorToken::Text, f32::INFINITY);
        let misses = self
            .deadline_misses()
            .map_or_else(|| "--".to_owned(), |misses| misses.to_string());
        readout(ui, "Misses", &misses, ColorToken::Text, f32::INFINITY);
        let racks = self.controller.document().model.racks.len();
        let restarts: u32 = self.product.as_ref().map_or(0, |product| {
            (0..racks)
                .map(|rack| product.rack_restart_count(rack))
                .sum()
        });
        readout(
            ui,
            "Restarts",
            &restarts.to_string(),
            if restarts > 0 {
                ColorToken::Warn
            } else {
                ColorToken::Text
            },
            f32::INFINITY,
        );
        let dirty = if self.controller.is_dirty() { " *" } else { "" };
        readout(
            ui,
            "Session",
            &format!("{}{dirty}", self.session_name()),
            ColorToken::Text,
            px(Layout::SessionNameWidth),
        );
    }

    pub(super) fn output_device_name(&self) -> String {
        #[cfg(target_os = "macos")]
        if let Some(route) = &self.selected_route
            && let Some(device) = self
                .audio_devices
                .iter()
                .find(|device| device.info.id == route.output)
        {
            return device.info.name.clone();
        }
        "None".to_owned()
    }

    pub(super) fn buffer_frames_value(&self) -> u32 {
        #[cfg(target_os = "macos")]
        return self.buffer_frames;
        #[cfg(not(target_os = "macos"))]
        128
    }

    pub(super) fn draw_fault_line(&mut self, ctx: &egui::Context) {
        let Some(message) = self.fault.clone() else {
            return;
        };
        egui::TopBottomPanel::top("fault_line")
            .exact_height(px(Layout::FaultLine))
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(color(ColorToken::FaultTint))
                    .inner_margin(margin(Spacing::S20, Spacing::S8)),
            )
            .show(ctx, |ui| {
                let rule = ui
                    .max_rect()
                    .expand2(Vec2::new(sp(Spacing::S20), sp(Spacing::S8)));
                ui.painter().hline(
                    rule.x_range(),
                    rule.bottom() - px(Layout::Hairline) / 2.0,
                    hairline(ColorToken::Fault),
                );
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = sp(Spacing::S16);
                    super::style::text(ui, "Fault", TypographyRole::Body, ColorToken::Fault);
                    ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                        if action(ui, "Dismiss", Some("Esc"), true).clicked() {
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
                            response.widget_info(|| {
                                WidgetInfo::labeled(WidgetType::Label, true, "Fault")
                            });
                        });
                    });
                });
            });
    }

    pub(super) fn draw_foot(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("foot")
            .exact_height(px(Layout::Foot))
            .frame(
                Frame::new()
                    .fill(color(ColorToken::Base))
                    .inner_margin(margin(Spacing::S20, Spacing::S12)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = sp(Spacing::S4);
                    super::style::text(ui, "Scene", TypographyRole::Body, ColorToken::Dim);
                    ui.add_space(sp(Spacing::S8));
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
                    ui.add_space(sp(Spacing::S24));
                    super::style::text(ui, "Fade", TypographyRole::Body, ColorToken::Dim);
                    ui.add_space(sp(Spacing::S4));
                    if action(ui, "−", None, self.fade_ms > 0).clicked() {
                        self.step_fade(-1);
                    }
                    super::style::text(
                        ui,
                        format!("{} ms", self.fade_ms),
                        TypographyRole::Body,
                        ColorToken::Text,
                    );
                    if action(ui, "+", None, self.fade_ms < super::FADE_MAX_MS).clicked() {
                        self.step_fade(1);
                    }
                    ui.add_space(sp(Spacing::S24));
                    ui.spacing_mut().item_spacing.x = sp(Spacing::S20);
                    if action(ui, "Capture", Some("⌘⇧C"), online).clicked() {
                        self.capture_scene();
                    }
                    let scene_open = online && self.current_scene.is_some();
                    if action(ui, "Update", None, scene_open).clicked() {
                        self.update_current_scene();
                    }
                    if action(ui, "Edit", None, scene_open).clicked()
                        && let Some(index) = self.current_scene
                        && let Some(scene) =
                            self.controller.document().model.scenes.get(index).cloned()
                    {
                        self.open_scene_editor(scene, Some(index));
                    }
                    ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                        self.draw_load_bars(ui);
                        ui.add_space(sp(Spacing::S6));
                        super::style::text(ui, "Callback", TypographyRole::Body, ColorToken::Dim);
                    });
                });
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
                (load * height).clamp(px(Layout::Hairline), height)
            } else {
                px(Layout::Hairline)
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
                0.0,
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

    pub(super) fn draw_columns(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(color(ColorToken::Base)))
            .show(ctx, |ui| {
                let area = ui.max_rect();
                let add_width = if self.controller.document().model.racks.len() < MAX_RACKS {
                    px(Layout::AddRackWidth)
                } else {
                    0.0
                };
                let visible = self.visible_racks();
                if visible.is_empty() {
                    self.draw_empty_columns(ui, area, add_width);
                    return;
                }
                // Columns keep one width whatever the rack count; spare width stays empty
                // and a narrow window scrolls.
                let column_width = px(Layout::ColumnWidth);
                #[allow(clippy::cast_precision_loss, reason = "at most 64 racks")]
                let content_width = column_width * visible.len() as f32 + add_width;
                let scroll_to_selected = std::mem::take(&mut self.scroll_to_selected);
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let (content, _) = ui.allocate_exact_size(
                            Vec2::new(content_width, area.height()),
                            Sense::hover(),
                        );
                        let preview = preview_height(content.height());
                        for (position, &rack) in visible.iter().enumerate() {
                            #[allow(clippy::cast_precision_loss, reason = "at most 64 racks")]
                            let left = content.left() + column_width * position as f32;
                            let column = Rect::from_min_size(
                                Pos2::new(left, content.top()),
                                Vec2::new(column_width, content.height()),
                            );
                            if scroll_to_selected && rack == self.selected_rack {
                                ui.scroll_to_rect(column, Some(Align::Center));
                            }
                            // Columns scrolled out of view cost nothing.
                            if !ui.is_rect_visible(column) {
                                continue;
                            }
                            ui.painter().vline(
                                column.right() - px(Layout::Hairline) / 2.0,
                                column.y_range(),
                                hairline(ColorToken::Hairline),
                            );
                            self.draw_column(ui, column, rack, preview);
                        }
                        if add_width > 0.0 {
                            let add = Rect::from_min_max(
                                Pos2::new(content.right() - add_width, content.top()),
                                content.max,
                            );
                            ui.painter().vline(
                                add.right() - px(Layout::Hairline) / 2.0,
                                add.y_range(),
                                hairline(ColorToken::Hairline),
                            );
                            if add_rack_column(ui, add).clicked() {
                                self.add_rack();
                            }
                        }
                    });
            });
    }

    /// An empty session, or a page with no racks: one centred message and the add column.
    fn draw_empty_columns(&mut self, ui: &mut Ui, area: Rect, add_width: f32) {
        let message =
            Rect::from_min_max(area.min, Pos2::new(area.right() - add_width, area.bottom()));
        ui.painter().vline(
            message.right() - px(Layout::Hairline) / 2.0,
            message.y_range(),
            hairline(ColorToken::Hairline),
        );
        let center = message.center();
        let lines = if self.page.is_some() {
            [
                (
                    "No racks on this page.",
                    TypographyRole::Title,
                    ColorToken::Text,
                ),
                (
                    "Add a rack here, or use a rack's menu, Pages, on another page.",
                    TypographyRole::Body,
                    ColorToken::Dim,
                ),
                (
                    "Pages only choose which racks show. Audio and routing do not change.",
                    TypographyRole::Body,
                    ColorToken::Dim,
                ),
            ]
        } else {
            [
                ("No racks yet.", TypographyRole::Title, ColorToken::Text),
                (
                    "Add a rack, then choose a plug-in after scanning.",
                    TypographyRole::Body,
                    ColorToken::Dim,
                ),
                (
                    "Each rack is one insert chain with its own input and output channels.",
                    TypographyRole::Body,
                    ColorToken::Dim,
                ),
            ]
        };
        for (index, (text, role, token)) in lines.into_iter().enumerate() {
            #[allow(clippy::cast_precision_loss, reason = "three lines")]
            let y = center.y + (index as f32 - 1.0) * (line() + sp(Spacing::S4));
            paint_text(
                ui,
                Pos2::new(center.x, y),
                Align2::CENTER_CENTER,
                text,
                role,
                token,
                message.width(),
            );
        }
        if add_width > 0.0 {
            let add = Rect::from_min_max(Pos2::new(message.right(), area.top()), area.max);
            if add_rack_column(ui, add).clicked() {
                self.add_rack();
            }
        }
    }

    /// The page line: `All racks`, each page as `n Name`, and `+ page`. Right-click a page for
    /// its menu.
    pub(super) fn draw_page_line(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("page_line")
            .exact_height(px(Layout::FaultLine))
            .frame(
                Frame::new()
                    .fill(color(ColorToken::Base))
                    .inner_margin(margin(Spacing::S20, Spacing::S6)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = sp(Spacing::S4);
                    super::style::text(ui, "Page", TypographyRole::Body, ColorToken::Dim);
                    ui.add_space(sp(Spacing::S8));
                    if pad(ui, "Page", None, "All racks", self.page.is_none(), true).clicked() {
                        self.show_page(None);
                    }
                    let pages: Vec<String> = self
                        .controller
                        .document()
                        .model
                        .pages
                        .iter()
                        .map(|page| page.name.clone())
                        .collect();
                    for (index, name) in pages.iter().enumerate() {
                        let response = pad(
                            ui,
                            "Page",
                            Some(index + 1),
                            name,
                            self.page == Some(index),
                            true,
                        );
                        if response.clicked() {
                            self.show_page(Some(index));
                        }
                        if response.secondary_clicked() {
                            self.overlay = Overlay::PageMenu {
                                page: index,
                                at: pointer_or(ui, response.rect.left_bottom()),
                            };
                        }
                    }
                    ui.add_space(sp(Spacing::S8));
                    let can_add = pages.len() < sp_model::MAX_PAGES;
                    if action(ui, "+ page", None, can_add).clicked() {
                        self.new_page(None);
                    }
                });
            });
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one column reads top to bottom like the hardware it mirrors"
    )]
    fn draw_column(&mut self, ui: &mut Ui, column: Rect, rack_index: usize, preview: f32) {
        let Some(rack) = self
            .controller
            .document()
            .model
            .racks
            .get(rack_index)
            .cloned()
        else {
            return;
        };
        let inner = column.shrink(sp(Spacing::S12));
        let id = ui.id().with(("column", rack_index));
        let selected = rack_index == self.selected_rack;
        let online = self.online();
        let mut y = inner.top();

        // Header: index, name, and gain. The selected rack's header is inverted.
        let header =
            Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(inner.width(), line()));
        let header_hit = header.expand2(Vec2::new(sp(Spacing::S6), 0.0));
        if selected {
            ui.painter()
                .rect_filled(header_hit, 0.0, color(ColorToken::Text));
        }
        let (text_token, index_token) = if selected {
            (ColorToken::Base, ColorToken::Faint)
        } else {
            (ColorToken::Text, ColorToken::Dim)
        };
        let index_rect = paint_text(
            ui,
            header.left_center(),
            Align2::LEFT_CENTER,
            format!("{:02}", rack_index + 1),
            TypographyRole::Body,
            index_token,
            header.width(),
        );
        let gain_rect = paint_text(
            ui,
            header.right_center(),
            Align2::RIGHT_CENTER,
            gain_label(rack.gain_db.get()).trim_end_matches(" dB"),
            TypographyRole::Body,
            text_token,
            header.width(),
        );
        let name_left = index_rect.right() + sp(Spacing::S8);
        let name_width = (gain_rect.left() - name_left - sp(Spacing::S8)).max(0.0);
        if let Some((renaming, draft)) = &mut self.rename
            && *renaming == rack_index
        {
            let field = Rect::from_min_size(
                Pos2::new(name_left, header.top()),
                Vec2::new(name_width, line()),
            );
            let response = ui.put(
                field,
                egui::TextEdit::singleline(draft)
                    .font(font(TypographyRole::Title))
                    .margin(Margin::ZERO)
                    .desired_width(name_width),
            );
            if !response.has_focus() && !response.lost_focus() {
                response.request_focus();
            }
            let cancel = ui.input(|input| input.key_pressed(egui::Key::Escape));
            if cancel {
                self.rename = None;
            } else if response.lost_focus() {
                self.commit_rename();
            }
        } else {
            paint_text(
                ui,
                Pos2::new(name_left, header.center().y),
                Align2::LEFT_CENTER,
                &rack.name,
                TypographyRole::Title,
                text_token,
                name_width,
            );
        }
        let header_response = ui.interact(header_hit, id.with("header"), Sense::click());
        header_response.widget_info(|| {
            WidgetInfo::selected(WidgetType::SelectableLabel, true, selected, &rack.name)
        });
        if header_response.double_clicked() {
            self.select_rack(rack_index);
            self.rename = Some((rack_index, rack.name.clone()));
        } else if header_response.clicked() {
            self.select_rack(rack_index);
            self.selected_slot = None;
        }
        if header_response.secondary_clicked() {
            self.select_rack(rack_index);
            self.overlay = Overlay::RackMenu {
                rack: rack_index,
                at: pointer_or(ui, header.left_bottom()),
            };
        }
        y += line() + sp(Spacing::S2);

        // Route line: "in 1   out 1-2", warn colour when the device lacks a channel.
        let route = super::rack_route(&self.controller.document().model, &rack);
        let route_text = format!(
            "in {}   out {}",
            route
                .input
                .map_or_else(|| "none".to_owned(), channels_label),
            channels_label(route.output)
        );
        let route_rect =
            Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(inner.width(), line()));
        let route_response = ui.interact(route_rect, id.with("route"), Sense::click());
        let route_token = if !self.route_fits(route) {
            ColorToken::Warn
        } else if route_response.hovered() {
            ColorToken::Text
        } else {
            ColorToken::Dim
        };
        paint_text(
            ui,
            route_rect.left_center(),
            Align2::LEFT_CENTER,
            &route_text,
            TypographyRole::Body,
            route_token,
            inner.width(),
        );
        route_response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, &route_text));
        if route_response.clicked() {
            self.select_rack(rack_index);
            self.overlay = Overlay::Route {
                rack: rack_index,
                at: route_rect.left_bottom(),
            };
        }
        y += line() + sp(Spacing::S10);

        // Eight preview slots.
        for slot in 0..PREVIEW_SLOTS {
            let tile = Rect::from_min_size(
                Pos2::new(inner.left(), y),
                Vec2::new(inner.width(), preview),
            );
            y += preview + sp(Spacing::S4);
            if let Some(model_slot) = rack.slots.get(slot) {
                let token = self.slot_token(rack_index, slot);
                let live = self
                    .product
                    .as_ref()
                    .is_ok_and(|product| product.editor_open(rack_index, slot));
                let state = Tile {
                    name: &model_slot.plugin.identity.name,
                    token,
                    picture: self.previews.get(&model_slot.id.0),
                    live,
                    sidechain: self.sidechain_marker(model_slot),
                    selected: selected && self.selected_slot == Some(slot),
                };
                let response = super::previews::tile(ui, tile, id.with(("slot", slot)), &state);
                if response.clicked() {
                    self.select_rack(rack_index);
                    self.slot_action(ui.ctx(), SlotAction::Editor(slot));
                }
                if response.secondary_clicked() {
                    self.select_rack(rack_index);
                    self.selected_slot = Some(slot);
                    self.overlay = Overlay::SlotMenu {
                        rack: rack_index,
                        slot,
                        at: pointer_or(ui, tile.left_bottom()),
                    };
                }
            } else if slot == rack.slots.len() {
                let response = ui.interact(tile, id.with(("add", slot)), Sense::click());
                let token = if response.hovered() {
                    ColorToken::Dim
                } else {
                    ColorToken::Faint
                };
                dashed_rect(ui, tile, token);
                paint_text(
                    ui,
                    tile.center(),
                    Align2::CENTER_CENTER,
                    "+ add plug-in",
                    TypographyRole::Label,
                    token,
                    tile.width(),
                );
                response
                    .widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Add plug-in"));
                if response.clicked() {
                    self.select_rack(rack_index);
                    self.overlay = Overlay::Picker(Picker {
                        rack: rack_index,
                        query: String::new(),
                        cursor: 0,
                    });
                }
            } else {
                dashed_rect(ui, tile, ColorToken::Faint);
            }
        }

        // Gain, meters, toggles, and status sit at the column's foot.
        let foot = ColumnFoot::new();
        let mut y = inner.bottom() - foot.height();
        let gain_top = y + sp(Spacing::S8);
        self.draw_gain(ui, id, inner, gain_top, rack_index, &rack);
        y += foot.gain;
        self.draw_meters(ui, inner, y + sp(Spacing::S10), rack_index, online);
        y += foot.meters + foot.labels + sp(Spacing::S8);

        let mut toggles = ui.new_child(
            UiBuilder::new()
                .max_rect(Rect::from_min_size(
                    Pos2::new(inner.left(), y),
                    Vec2::new(inner.width(), line() + px(Layout::Hairline) * 2.0),
                ))
                .layout(EguiLayout::left_to_right(Align::Center)),
        );
        toggles.spacing_mut().item_spacing.x = sp(Spacing::S6);
        if toggle(&mut toggles, "Mute", rack.muted, ColorToken::Warn).clicked() {
            self.set_rack_controls(rack_index, rack.gain_db.get(), !rack.muted, rack.bypassed);
        }
        if toggle(&mut toggles, "Byp", rack.bypassed, ColorToken::Info).clicked() {
            self.set_rack_controls(rack_index, rack.gain_db.get(), rack.muted, !rack.bypassed);
        }
        y += line() + px(Layout::Hairline) * 2.0 + sp(Spacing::S8);

        let status =
            Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(inner.width(), line()));
        let token = self.rack_token(rack_index);
        let token_rect = paint_text(
            ui,
            status.left_center(),
            Align2::LEFT_CENTER,
            token.code(),
            TypographyRole::Body,
            token.color(),
            status.width(),
        );
        let ready = (0..rack.slots.len())
            .filter(|slot| self.slot_token(rack_index, *slot) == StateToken::Ready)
            .count();
        paint_text(
            ui,
            Pos2::new(token_rect.right() + sp(Spacing::S6), status.center().y),
            Align2::LEFT_CENTER,
            format!("{ready}/{}", rack.slots.len()),
            TypographyRole::Body,
            ColorToken::Text,
            status.width(),
        );
        if rack.muted {
            paint_text(
                ui,
                status.right_center(),
                Align2::RIGHT_CENTER,
                "muted",
                TypographyRole::Body,
                ColorToken::Dim,
                status.width(),
            );
        }
        ui.interact(status, id.with("status"), Sense::hover())
            .on_hover_text(token.meaning());
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

    /// Gain: label row, a 2 px track with a 0 dB tick and a marker, and tick labels. Click or
    /// drag sets it, Shift drags finely, and double-click resets to 0 dB.
    fn draw_gain(
        &mut self,
        ui: &mut Ui,
        id: egui::Id,
        inner: Rect,
        top: f32,
        rack_index: usize,
        rack: &sp_model::Rack,
    ) {
        let gain_db = rack.gain_db.get();
        let label_row = Rect::from_min_size(
            Pos2::new(inner.left(), top),
            Vec2::new(inner.width(), line()),
        );
        paint_text(
            ui,
            label_row.left_center(),
            Align2::LEFT_CENTER,
            "Gain",
            TypographyRole::Body,
            ColorToken::Dim,
            inner.width(),
        );
        paint_text(
            ui,
            label_row.right_center(),
            Align2::RIGHT_CENTER,
            gain_label(gain_db),
            TypographyRole::Body,
            ColorToken::Text,
            inner.width(),
        );
        let track_y = label_row.bottom() + sp(Spacing::S8) + px(Layout::GainTrack) / 2.0;
        let track = Rect::from_center_size(
            Pos2::new(inner.center().x, track_y),
            Vec2::new(inner.width(), px(Layout::GainTrack)),
        );
        let hit = track.expand2(Vec2::Y * sp(Spacing::S8));
        let response = ui.interact(hit, id.with("gain"), Sense::click_and_drag());
        let painter = ui.painter();
        painter.rect_filled(track, 0.0, color(ColorToken::Faint));
        let x_for = |db: f32| track.left() + track.width() * gain_fraction(db);
        painter.vline(
            x_for(0.0),
            track_y - px(Layout::GainTick) / 2.0..=track_y + px(Layout::GainTick) / 2.0,
            hairline(ColorToken::Dim),
        );
        painter.rect_filled(
            Rect::from_center_size(
                Pos2::new(x_for(gain_db), track_y),
                Vec2::new(px(Layout::GainMarkerWidth), px(Layout::GainMarkerHeight)),
            ),
            0.0,
            color(ColorToken::Text),
        );
        let ticks_y = track.bottom() + sp(Spacing::S4) + px(Layout::Caption) / 2.0;
        for (db, anchor, label) in [
            (GAIN_MIN_DB, Align2::LEFT_CENTER, "-60"),
            (0.0, Align2::CENTER_CENTER, "0"),
            (GAIN_MAX_DB, Align2::RIGHT_CENTER, "+12"),
        ] {
            paint_text(
                ui,
                Pos2::new(x_for(db), ticks_y),
                anchor,
                label,
                TypographyRole::Label,
                ColorToken::Faint,
                inner.width(),
            );
        }
        super::style::focus_outline(ui, &response, hit);
        response.widget_info(|| {
            WidgetInfo::slider(true, f64::from(gain_db), format!("{} gain", rack.name))
        });
        let target = gain_target(ui, &response, track, gain_db);
        if let Some(db) = target.map(|db| db.clamp(GAIN_MIN_DB, GAIN_MAX_DB))
            && (db - gain_db).abs() > f32::EPSILON
        {
            self.set_rack_controls(rack_index, db, rack.muted, rack.bypassed);
        }
    }

    /// Three LED columns: rack input, then output L and R.
    fn draw_meters(&self, ui: &mut Ui, inner: Rect, top: f32, rack_index: usize, online: bool) {
        #[cfg(target_os = "macos")]
        let (input, output) = (
            self.rack_input_meters[rack_index],
            self.rack_output_meters[rack_index],
        );
        #[cfg(target_os = "macos")]
        let levels = [
            (input.fill_fraction(), input.held_fraction(), "in"),
            (output.channel_fraction(0), output.held_fraction(), "L"),
            (output.channel_fraction(1), output.held_fraction(), "R"),
        ];
        #[cfg(not(target_os = "macos"))]
        let levels = [(0.0, 0.0, "in"), (0.0, 0.0, "L"), (0.0, 0.0, "R")];
        let width = px(Layout::MeterWidth);
        let segment = px(Layout::MeterSegment);
        let gap = sp(Spacing::S2);
        let bottom = top + px(Layout::MeterHeight);
        for (column, (level, held, label)) in levels.into_iter().enumerate() {
            #[allow(clippy::cast_precision_loss, reason = "three meter columns")]
            let left = inner.left() + column as f32 * (width + sp(Spacing::S10));
            for index in 0..LED_SEGMENTS {
                #[allow(clippy::cast_precision_loss, reason = "sixteen segments")]
                let segment_bottom = bottom - index as f32 * (segment + gap);
                let token = match (led_segment(level, held, index), online) {
                    (Segment::Off, _) => ColorToken::SegmentOff,
                    (_, false) => ColorToken::Faint,
                    (Segment::Lit, true) => ColorToken::Text,
                    (Segment::Hot, true) => ColorToken::Fault,
                };
                ui.painter().rect_filled(
                    Rect::from_min_max(
                        Pos2::new(left, segment_bottom - segment),
                        Pos2::new(left + width, segment_bottom),
                    ),
                    0.0,
                    color(token),
                );
            }
            paint_text(
                ui,
                Pos2::new(
                    left + width / 2.0,
                    bottom + sp(Spacing::S4) + px(Layout::Caption) / 2.0,
                ),
                Align2::CENTER_CENTER,
                label,
                TypographyRole::Label,
                ColorToken::Dim,
                width * 2.0,
            );
        }
        let meters = Rect::from_min_max(
            Pos2::new(inner.left(), top),
            Pos2::new(inner.left() + (width + sp(Spacing::S10)) * 3.0, bottom),
        );
        ui.interact(meters, ui.id().with(("meters", rack_index)), Sense::hover())
            .widget_info(|| {
                let mut info =
                    WidgetInfo::labeled(WidgetType::ProgressIndicator, true, "Rack level");
                info.value = Some(f64::from(levels[1].0.max(levels[2].0)));
                info
            });
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

/// A scene or page pad: `n Name`, inverted while active, faint while disabled. `kind` names it
/// for assistive technology.
fn pad(
    ui: &mut Ui,
    kind: &str,
    number: Option<usize>,
    name: &str,
    active: bool,
    enabled: bool,
) -> Response {
    let number_text = number.map(|number| number.to_string());
    let gap = sp(Spacing::S6);
    let number_width = number_text.as_ref().map_or(0.0, |text| {
        galley(
            ui,
            text,
            TypographyRole::Body,
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
    let pad = sp(Spacing::S8);
    let size = Vec2::new(
        pad * 2.0 + number_width + name_width,
        line() + sp(Spacing::S4),
    );
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let hovered = enabled && response.hovered();
    let (fill, number_token, name_token) = match (enabled, active, hovered) {
        (false, _, _) => (None, ColorToken::Faint, ColorToken::Faint),
        (true, true, _) => (Some(ColorToken::Text), ColorToken::Faint, ColorToken::Base),
        (true, false, true) => (
            Some(ColorToken::HoverFill),
            ColorToken::Text,
            ColorToken::Text,
        ),
        (true, false, false) => (None, ColorToken::Dim, ColorToken::Text),
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, 0.0, color(fill));
    }
    let mut left = rect.left() + pad;
    if let Some(number_text) = &number_text {
        let placed = paint_text(
            ui,
            Pos2::new(left, rect.center().y),
            Align2::LEFT_CENTER,
            number_text,
            TypographyRole::Body,
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
    super::style::focus_outline(ui, &response, rect);
    response.widget_info(|| {
        let label = match number {
            Some(number) => format!("{kind} {number} {name}"),
            None => format!("{kind} {name}"),
        };
        WidgetInfo::selected(WidgetType::Button, enabled, active, label)
    });
    response
}

/// The gain a click, drag, or arrow key on the track asks for.
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
        let fraction = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0);
        let db = GAIN_MIN_DB + fraction * (GAIN_MAX_DB - GAIN_MIN_DB);
        Some((db / step).round() * step)
    } else if response.has_focus() {
        ui.input(|input| {
            if input.key_pressed(egui::Key::ArrowLeft) {
                Some(gain_db - step)
            } else if input.key_pressed(egui::Key::ArrowRight) {
                Some(gain_db + step)
            } else {
                None
            }
        })
    } else {
        None
    }
}

/// The "+ add rack" column.
fn add_rack_column(ui: &mut Ui, rect: Rect) -> Response {
    let response = ui.interact(rect, ui.id().with("add_rack"), Sense::click());
    let token = if response.hovered() {
        ColorToken::Dim
    } else {
        ColorToken::Faint
    };
    paint_text(
        ui,
        rect.center(),
        Align2::CENTER_CENTER,
        "+ add rack",
        TypographyRole::Body,
        token,
        rect.width(),
    );
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Add rack"));
    response
}

/// The pointer position for a context menu, or `fallback` for keyboard-opened menus.
fn pointer_or(ui: &Ui, fallback: Pos2) -> Pos2 {
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

/// Buffer latency at 48 kHz, as `1.3 ms`.
pub(super) fn latency_label(frames: u32) -> String {
    format!("{:.1} ms", f64::from(frames) / 48.0)
}
