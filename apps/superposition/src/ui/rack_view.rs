//! The rack view: the selected rack in detail. Left to right it follows the signal: the input
//! strip, the chain, the editor pane, and the output strip, then the hot plug-ins rail. On Linux
//! the selected plug-in's editor docks in the pane; elsewhere the pane shows its last picture.

use std::cmp::Ordering;

use eframe::egui::{
    self, Align, Align2, Frame, Layout as EguiLayout, Pos2, Rect, Sense, Ui, UiBuilder, Vec2,
    WidgetInfo, WidgetType,
};
use sp_model::{MAX_HOT_PLUGINS, MAX_SLOTS_PER_RACK};
use sp_ui::components::StateToken;
use sp_ui::design::{ColorToken, Layout, Radius, Spacing, TypographyRole};

use super::overview::add_plugin_row;
use super::previews::{age_label, fitted, peek_on_hover, thumbnail};
use super::show::{RouteChips, margin, paint_gain, pointer_or};
use super::style::{
    ButtonKind, ChipTone, button, card, chip, color, dashed_rect, dot, focus_outline, hairline,
    line, paint_text, px, radius, sp, text, toggle, toggle_at,
};
use super::{LiveRackApp, Overlay, Picker, SlotAction};

/// What the pane shows over the picture when no editor is drawn there.
enum PaneState {
    /// The plug-in is not installed here.
    Missing,
    /// The editor is opening.
    Opening,
    /// The editor is open in its own window.
    Floating,
    /// The editor is closed.
    Closed,
}

impl LiveRackApp {
    pub(super) fn draw_rack_view(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(color(ColorToken::Base))
                    .inner_margin(margin(Spacing::S12, Spacing::S12)),
            )
            .show(ctx, |ui| {
                let area = ui.max_rect();
                if self.controller.document().model.racks.is_empty() {
                    self.draw_no_racks(ui, area);
                    return;
                }
                let rack = self.selected_rack;
                let gap = sp(Spacing::S8);
                let column = |left: f32, width: f32| {
                    Rect::from_min_size(
                        Pos2::new(left, area.top()),
                        Vec2::new(width, area.height()),
                    )
                };
                let input = column(area.left(), px(Layout::InputStripWidth));
                let chain = column(input.right() + gap, px(Layout::ChainWidth));
                let hot = column(
                    area.right() - px(Layout::HotRailWidth),
                    px(Layout::HotRailWidth),
                );
                let output = column(
                    hot.left() - gap - px(Layout::OutputStripWidth),
                    px(Layout::OutputStripWidth),
                );
                let pane = Rect::from_min_max(
                    Pos2::new(chain.right() + gap, area.top()),
                    Pos2::new(output.left() - gap, area.bottom()),
                );
                self.draw_input_strip(ui, input, rack);
                self.draw_chain(ui, chain, rack);
                self.draw_pane(ui, pane, rack);
                self.draw_output_strip(ui, output, rack);
                self.draw_hot_rail(ui, hot);
            });
    }

    fn draw_no_racks(&mut self, ui: &mut Ui, area: Rect) {
        let center = area.center();
        paint_text(
            ui,
            center - Vec2::Y * (line() + sp(Spacing::S6)),
            Align2::CENTER_CENTER,
            "No racks yet.",
            TypographyRole::Title,
            ColorToken::Text,
            area.width(),
        );
        let mut row = ui.new_child(
            UiBuilder::new()
                .max_rect(Rect::from_center_size(
                    center + Vec2::Y * sp(Spacing::S10),
                    Vec2::new(area.width(), px(Layout::Control)),
                ))
                .layout(EguiLayout::top_down(Align::Center)),
        );
        if button(&mut row, "Add rack", Some("⌘N"), ButtonKind::Primary, true).clicked() {
            self.add_rack();
        }
    }

    /// The input strip: route, input level, and rack bypass.
    fn draw_input_strip(&mut self, ui: &mut Ui, rect: Rect, rack: usize) {
        card(ui, rect, false);
        let inner = rect.shrink(sp(Spacing::S10));
        let id = ui.id().with(("input_strip", rack));
        let mut y = strip_title(ui, inner, "Input");
        y = self.draw_route_chips(
            ui,
            id,
            Pos2::new(inner.left(), y),
            inner.width(),
            rack,
            RouteChips::Input,
        ) + sp(Spacing::S10);
        let bypassed = self
            .controller
            .document()
            .model
            .racks
            .get(rack)
            .is_some_and(|rack| rack.bypassed);
        let toggle_rect = Rect::from_min_max(
            Pos2::new(inner.left(), inner.bottom() - px(Layout::Control)),
            inner.right_bottom(),
        );
        if toggle_at(
            ui,
            toggle_rect,
            id.with("bypass"),
            "Bypass",
            bypassed,
            ColorToken::Info,
        )
        .clicked()
            && let Some(model_rack) = self.controller.document().model.racks.get(rack).cloned()
        {
            self.set_rack_controls(
                rack,
                model_rack.gain_db.get(),
                model_rack.muted,
                !model_rack.bypassed,
            );
        }
        let width = px(Layout::MeterWidth);
        self.draw_meters(
            ui,
            rack,
            &[0],
            (
                inner.center().x - width / 2.0,
                y,
                toggle_rect.top() - sp(Spacing::S10),
            ),
            width,
        );
    }

    /// The output strip: route, output meters beside the gain fader, gain, and mute.
    fn draw_output_strip(&mut self, ui: &mut Ui, rect: Rect, rack: usize) {
        let Some(model_rack) = self.controller.document().model.racks.get(rack).cloned() else {
            return;
        };
        card(ui, rect, false);
        let inner = rect.shrink(sp(Spacing::S10));
        let id = ui.id().with(("output_strip", rack));
        let mut y = strip_title(ui, inner, "Output");
        y = self.draw_route_chips(
            ui,
            id,
            Pos2::new(inner.left(), y),
            inner.width(),
            rack,
            RouteChips::Output,
        ) + sp(Spacing::S10);
        let mute = Rect::from_min_max(
            Pos2::new(inner.left(), inner.bottom() - px(Layout::Control)),
            inner.right_bottom(),
        );
        let gain_db = model_rack.gain_db.get();
        if toggle_at(
            ui,
            mute,
            id.with("mute"),
            "Mute",
            model_rack.muted,
            ColorToken::Warn,
        )
        .clicked()
        {
            self.set_rack_controls(rack, gain_db, !model_rack.muted, model_rack.bypassed);
        }
        let gain_row = mute.top() - sp(Spacing::S10) - line();
        paint_gain(
            ui,
            Pos2::new(inner.center().x, gain_row + line() / 2.0),
            Align2::CENTER_CENTER,
            gain_db,
            inner.width(),
        );
        let bottom = gain_row - sp(Spacing::S6);
        let meters_right = self.draw_meters(
            ui,
            rack,
            &[1, 2],
            (inner.left() + sp(Spacing::S6), y, bottom),
            px(Layout::MeterWidth),
        );
        let fader = Rect::from_min_max(
            Pos2::new(meters_right + sp(Spacing::S6), y),
            Pos2::new(inner.right(), bottom - px(Layout::Caption)),
        );
        self.draw_gain_fader(ui, id, fader, rack);
    }

    /// The chain: a header, then up to eight slot rows with pictures, the add row, and empty
    /// places.
    fn draw_chain(&mut self, ui: &mut Ui, rect: Rect, rack: usize) {
        card(ui, rect, false);
        let inner = rect.shrink(sp(Spacing::S10));
        let slots = self
            .controller
            .document()
            .model
            .racks
            .get(rack)
            .map_or(0, |rack| rack.slots.len());
        let header = Rect::from_min_size(inner.min, Vec2::new(inner.width(), line()));
        paint_text(
            ui,
            header.left_center(),
            Align2::LEFT_CENTER,
            "Chain",
            TypographyRole::Strong,
            ColorToken::Text,
            header.width(),
        );
        paint_text(
            ui,
            header.right_center(),
            Align2::RIGHT_CENTER,
            format!("{slots} of {MAX_SLOTS_PER_RACK}"),
            TypographyRole::Mono,
            ColorToken::Dim,
            header.width(),
        );
        let id = ui.id().with(("chain", rack));
        let mut y = header.bottom() + sp(Spacing::S10);
        let empty_height = px(Layout::Control) + sp(Spacing::S6);
        for slot in 0..MAX_SLOTS_PER_RACK {
            let height = if slot < slots {
                px(Layout::ChainSlot)
            } else {
                empty_height
            };
            let row =
                Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(inner.width(), height));
            y += height + sp(Spacing::S4);
            match slot.cmp(&slots) {
                Ordering::Less => {
                    self.draw_chain_row(ui, row, id.with(("slot", slot)), rack, slot);
                }
                Ordering::Equal => {
                    if add_plugin_row(ui, row, id.with("add"), TypographyRole::Body).clicked() {
                        self.overlay = Overlay::Picker(Picker {
                            rack,
                            query: String::new(),
                            cursor: 0,
                        });
                    }
                }
                Ordering::Greater => {
                    dashed_rect(ui, row, ColorToken::Hairline, Radius::Control);
                }
            }
        }
    }

    /// One chain row: thumbnail, name, sidechain chip, and the state with the picture's age.
    #[allow(
        clippy::too_many_lines,
        reason = "one row reads left to right: picture, name, sidechain, state"
    )]
    fn draw_chain_row(&mut self, ui: &mut Ui, row: Rect, id: egui::Id, rack: usize, slot: usize) {
        let Some(model_slot) = self
            .controller
            .document()
            .model
            .racks
            .get(rack)
            .and_then(|rack| rack.slots.get(slot))
            .cloned()
        else {
            return;
        };
        let token = self.slot_token(rack, slot);
        let selected = self.selected_slot == Some(slot);
        let live = self
            .product
            .as_ref()
            .is_ok_and(|product| product.editor_open(rack, slot));
        let response = ui.interact(row, id, Sense::click());
        if selected || response.hovered() {
            ui.painter()
                .rect_filled(row, radius(Radius::Control), color(ColorToken::HoverFill));
        }
        if selected {
            ui.painter().rect_stroke(
                row,
                radius(Radius::Control),
                hairline(ColorToken::Accent),
                egui::StrokeKind::Inside,
            );
        }
        let picture = self.previews.get(&model_slot.id.0);
        let thumb_height = row.height() - sp(Spacing::S6) * 2.0;
        let thumb = Rect::from_min_size(
            Pos2::new(
                row.left() + sp(Spacing::S6),
                row.center().y - thumb_height / 2.0,
            ),
            Vec2::new(px(Layout::ChainThumbWidth), thumb_height),
        );
        thumbnail(ui, thumb, picture, token, Radius::Small);
        let left = thumb.right() + sp(Spacing::S8);
        let right = row.right() - sp(Spacing::S8);
        let name_y = row.center().y - line() / 2.0;
        let mut name_right = right;
        if let Some(marker) = self.sidechain_marker(&model_slot) {
            let mut chip_ui = ui.new_child(
                UiBuilder::new()
                    .max_rect(Rect::from_min_max(
                        Pos2::new(left, name_y - px(Layout::Chip) / 2.0),
                        Pos2::new(right, name_y + px(Layout::Chip) / 2.0),
                    ))
                    .layout(EguiLayout::right_to_left(Align::Center)),
            );
            name_right = chip(&mut chip_ui, &marker, ChipTone::Info, false)
                .rect
                .left()
                - sp(Spacing::S6);
        }
        let name = &model_slot.plugin.identity.name;
        paint_text(
            ui,
            Pos2::new(left, name_y),
            Align2::LEFT_CENTER,
            name,
            TypographyRole::Strong,
            if matches!(token, StateToken::Missing | StateToken::Faulted) {
                ColorToken::Fault
            } else {
                ColorToken::Text
            },
            (name_right - left).max(0.0),
        );
        let state_y = row.center().y + line() / 2.0;
        let (dot_token, state) = if live {
            (ColorToken::Fault, "Editor open".to_owned())
        } else {
            let age = picture.map(|picture| age_label(picture.file.captured_at_unix_ms));
            (
                token.color(),
                age.map_or_else(
                    || token.label().to_owned(),
                    |age| format!("{} · {age}", token.label()),
                ),
            )
        };
        dot(
            ui,
            Pos2::new(left + px(Layout::Dot) / 2.0, state_y),
            dot_token,
        );
        paint_text(
            ui,
            Pos2::new(left + px(Layout::Dot) + sp(Spacing::S6), state_y),
            Align2::LEFT_CENTER,
            state,
            TypographyRole::Label,
            ColorToken::Dim,
            (right - left).max(0.0),
        );
        focus_outline(ui, &response, row);
        response.widget_info(|| {
            WidgetInfo::selected(
                WidgetType::Button,
                true,
                selected,
                format!("{name}, {}", token.meaning()),
            )
        });
        peek_on_hover(ui, &response, row, name, picture);
        if response.clicked() {
            let ctx = ui.ctx().clone();
            self.open_editor(&ctx, rack, slot);
        }
        if response.secondary_clicked() {
            self.selected_slot = Some(slot);
            self.overlay = Overlay::SlotMenu {
                rack,
                slot,
                at: pointer_or(ui, row.left_bottom()),
            };
        }
    }

    /// The editor pane: a header with the plug-in's name and actions, then its editor (docked,
    /// Linux) or its last picture with what to do next.
    fn draw_pane(&mut self, ui: &mut Ui, rect: Rect, rack: usize) {
        card(ui, rect, false);
        let header = Rect::from_min_size(rect.min, Vec2::new(rect.width(), px(Layout::PaneHeader)));
        ui.painter().hline(
            header.x_range(),
            header.bottom(),
            hairline(ColorToken::Hairline),
        );
        let body = Rect::from_min_max(Pos2::new(rect.left(), header.bottom()), rect.max)
            .shrink(sp(Spacing::S12));
        let slot = self.selected_slot.filter(|slot| {
            self.controller
                .document()
                .model
                .racks
                .get(rack)
                .is_some_and(|rack| *slot < rack.slots.len())
        });
        let Some(slot) = slot else {
            paint_text(
                ui,
                Pos2::new(header.left() + sp(Spacing::S12), header.center().y),
                Align2::LEFT_CENTER,
                "Editor",
                TypographyRole::Title,
                ColorToken::Dim,
                header.width(),
            );
            self.draw_pane_hint(ui, body, rack);
            return;
        };
        self.draw_pane_header(
            ui,
            header.shrink2(Vec2::new(sp(Spacing::S12), 0.0)),
            rack,
            slot,
        );
        #[cfg(target_os = "linux")]
        if self.docked == Some((rack, slot))
            && let Some(streamed) = self.streamed.as_mut()
            && streamed.draw_docked(ui, body, rack, slot)
        {
            return;
        }
        self.draw_pane_picture(ui, body, rack, slot);
    }

    /// No slot selected: how to fill the pane.
    fn draw_pane_hint(&mut self, ui: &mut Ui, body: Rect, rack: usize) {
        let empty = self
            .controller
            .document()
            .model
            .racks
            .get(rack)
            .is_none_or(|rack| rack.slots.is_empty());
        let message = if empty {
            "This rack has no plug-ins yet."
        } else {
            "Choose a plug-in in the chain to show its editor here."
        };
        paint_text(
            ui,
            body.center() - Vec2::Y * (line() + sp(Spacing::S6)),
            Align2::CENTER_CENTER,
            message,
            TypographyRole::Body,
            ColorToken::Dim,
            body.width(),
        );
        if empty {
            let mut row = ui.new_child(
                UiBuilder::new()
                    .max_rect(Rect::from_center_size(
                        body.center() + Vec2::Y * sp(Spacing::S10),
                        Vec2::new(body.width(), px(Layout::Control)),
                    ))
                    .layout(EguiLayout::top_down(Align::Center)),
            );
            if button(&mut row, "Add plug-in", None, ButtonKind::Primary, true).clicked() {
                self.overlay = Overlay::Picker(Picker {
                    rack,
                    query: String::new(),
                    cursor: 0,
                });
            }
        }
    }

    /// Name and `Slot n · Vendor` at the left; sidechain, bypass, and pop out or dock at the
    /// right.
    fn draw_pane_header(&mut self, ui: &mut Ui, rect: Rect, rack: usize, slot: usize) {
        let Some(model_slot) = self
            .controller
            .document()
            .model
            .racks
            .get(rack)
            .and_then(|rack| rack.slots.get(slot))
            .cloned()
        else {
            return;
        };
        let mut row = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(EguiLayout::left_to_right(Align::Center)),
        );
        row.spacing_mut().item_spacing.x = sp(Spacing::S8);
        text(
            &mut row,
            &model_slot.plugin.identity.name,
            TypographyRole::Title,
            ColorToken::Text,
        );
        text(
            &mut row,
            format!("Slot {} · {}", slot + 1, model_slot.plugin.identity.vendor),
            TypographyRole::Label,
            ColorToken::Dim,
        );
        row.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = sp(Spacing::S6);
            #[cfg(target_os = "linux")]
            {
                let open = self
                    .product
                    .as_ref()
                    .is_ok_and(|product| product.editor_open(rack, slot));
                if self.docked == Some((rack, slot)) && open {
                    if button(ui, "Pop out", None, ButtonKind::Outline, true).clicked() {
                        self.pop_out_editor();
                    }
                } else if open && button(ui, "Dock", None, ButtonKind::Outline, true).clicked() {
                    let ctx = ui.ctx().clone();
                    self.open_editor(&ctx, rack, slot);
                }
            }
            let hot = self.is_hot(rack, slot);
            let full = self.controller.document().model.hot_plugins.len() >= MAX_HOT_PLUGINS;
            if button(
                ui,
                if hot { "Unpin" } else { "Pin" },
                None,
                ButtonKind::Outline,
                hot || !full,
            )
            .clicked()
            {
                self.toggle_hot_plugin(rack, slot);
            }
            if toggle(ui, "Bypass", model_slot.bypassed, ColorToken::Info).clicked() {
                let ctx = ui.ctx().clone();
                self.slot_action(&ctx, SlotAction::Bypass(slot));
            }
            if self.accepts_sidechain(&model_slot) {
                let marker = self
                    .sidechain_marker(&model_slot)
                    .unwrap_or_else(|| "Sidechain…".to_owned());
                let tone = if model_slot.sidechain.is_some() {
                    ChipTone::Info
                } else {
                    ChipTone::Neutral
                };
                let response = chip(ui, &marker, tone, true);
                if response.clicked() {
                    self.overlay = Overlay::Sidechain {
                        rack,
                        slot,
                        at: response.rect.left_bottom() + Vec2::Y * sp(Spacing::S4),
                    };
                }
            }
        });
    }

    /// The slot's last picture, fitted to the pane, with a card that says what to do next.
    fn draw_pane_picture(&mut self, ui: &mut Ui, body: Rect, rack: usize, slot: usize) {
        let Some(model_slot) = self
            .controller
            .document()
            .model
            .racks
            .get(rack)
            .and_then(|rack| rack.slots.get(slot))
            .cloned()
        else {
            return;
        };
        let token = self.slot_token(rack, slot);
        if let Some(picture) = self.previews.get(&model_slot.id.0) {
            fitted(ui, body, picture, token);
        } else {
            dashed_rect(ui, body, ColorToken::Hairline, Radius::Card);
        }
        let open = self
            .product
            .as_ref()
            .is_ok_and(|product| product.editor_open(rack, slot));
        #[cfg(target_os = "linux")]
        let docked = self.docked == Some((rack, slot));
        #[cfg(not(target_os = "linux"))]
        let docked = false;
        let pending = self
            .product
            .as_ref()
            .is_ok_and(crate::ProductRuntime::native_editor_pending);
        let state = if token == StateToken::Missing {
            PaneState::Missing
        } else if docked && (open || pending) {
            PaneState::Opening
        } else if open {
            PaneState::Floating
        } else {
            PaneState::Closed
        };
        let (message, action) = match state {
            PaneState::Missing => (
                format!(
                    "{} is not installed on this machine. The slot keeps its saved state until you replace it.",
                    model_slot.plugin.identity.name
                ),
                None,
            ),
            PaneState::Opening => ("Opening the editor…".to_owned(), None),
            PaneState::Floating => (
                "The editor is open in its own window.".to_owned(),
                cfg!(target_os = "linux").then_some(("Dock here", None)),
            ),
            PaneState::Closed => (
                "The editor is closed.".to_owned(),
                Some(("Open editor", Some("↩"))),
            ),
        };
        if pane_notice(ui, body, &message, action) {
            let ctx = ui.ctx().clone();
            self.open_editor(&ctx, rack, slot);
        }
    }

    /// The hot plug-ins rail: pinned plug-ins from any rack, one click from their editor.
    fn draw_hot_rail(&mut self, ui: &mut Ui, rect: Rect) {
        card(ui, rect, false);
        let inner = rect.shrink(sp(Spacing::S10));
        let pinned = self.hot_plugin_slots();
        let header = Rect::from_min_size(inner.min, Vec2::new(inner.width(), line()));
        paint_text(
            ui,
            header.left_center(),
            Align2::LEFT_CENTER,
            "Hot plug-ins",
            TypographyRole::Strong,
            ColorToken::Text,
            header.width(),
        );
        paint_text(
            ui,
            header.right_center(),
            Align2::RIGHT_CENTER,
            format!("{}/{MAX_HOT_PLUGINS}", pinned.len()),
            TypographyRole::Mono,
            ColorToken::Dim,
            header.width(),
        );
        let id = ui.id().with("hot_rail");
        let mut y = header.bottom() + sp(Spacing::S10);
        // Tiles shrink so every pin fits, with room for the pin row while one is free.
        let pin_row_height = if pinned.len() < MAX_HOT_PLUGINS {
            px(Layout::Control) + sp(Spacing::S6) * 2.0
        } else {
            0.0
        };
        #[allow(clippy::cast_precision_loss, reason = "at most twelve pins")]
        let fit =
            (inner.bottom() - y - pin_row_height) / pinned.len().max(1) as f32 - sp(Spacing::S6);
        let tile_height = px(Layout::HotTile).min(fit);
        let mut unpin = None;
        for (index, &(rack, slot)) in pinned.iter().enumerate() {
            let tile = Rect::from_min_size(
                Pos2::new(inner.left(), y),
                Vec2::new(inner.width(), tile_height),
            );
            if tile.bottom() > inner.bottom() {
                break;
            }
            y += tile_height + sp(Spacing::S6);
            match self.draw_hot_tile(ui, tile, id.with(index), rack, slot) {
                HotTileAction::Open => {
                    let ctx = ui.ctx().clone();
                    self.open_editor(&ctx, rack, slot);
                }
                HotTileAction::Unpin => unpin = Some((rack, slot)),
                HotTileAction::None => {}
            }
        }
        if let Some((rack, slot)) = unpin {
            self.toggle_hot_plugin(rack, slot);
        }
        let selected = self
            .selected_slot
            .filter(|slot| !self.is_hot(self.selected_rack, *slot));
        if pinned.len() < MAX_HOT_PLUGINS {
            let add = Rect::from_min_size(
                Pos2::new(inner.left(), y),
                Vec2::new(inner.width(), px(Layout::Control) + sp(Spacing::S6)),
            );
            if add.bottom() <= inner.bottom() {
                let label = if selected.is_some() {
                    "+ Pin selected"
                } else if pinned.is_empty() {
                    "Pin plug-ins here"
                } else {
                    "Select a plug-in to pin"
                };
                if pin_row(ui, add, id.with("pin"), label, selected.is_some()).clicked()
                    && let Some(slot) = selected
                {
                    self.toggle_hot_plugin(self.selected_rack, slot);
                }
            }
        }
    }

    /// One hot plug-in: thumbnail, name, and rack. Hover shows an unpin mark.
    #[allow(
        clippy::too_many_lines,
        reason = "one tile: picture, caption, selection, and the unpin mark"
    )]
    fn draw_hot_tile(
        &self,
        ui: &mut Ui,
        tile: Rect,
        id: egui::Id,
        rack: usize,
        slot: usize,
    ) -> HotTileAction {
        let model = &self.controller.document().model;
        let (Some(model_rack), Some(model_slot)) = (
            model.racks.get(rack),
            model.racks.get(rack).and_then(|rack| rack.slots.get(slot)),
        ) else {
            return HotTileAction::None;
        };
        let token = self.slot_token(rack, slot);
        let selected = rack == self.selected_rack && self.selected_slot == Some(slot);
        let response = ui.interact(tile, id, Sense::click());
        let hovered = response.hovered() || ui.rect_contains_pointer(tile);
        if hovered || selected {
            ui.painter()
                .rect_filled(tile, radius(Radius::Control), color(ColorToken::HoverFill));
        }
        if selected {
            ui.painter().rect_stroke(
                tile,
                radius(Radius::Control),
                hairline(ColorToken::Accent),
                egui::StrokeKind::Inside,
            );
        }
        let picture = self.previews.get(&model_slot.id.0);
        let thumb = Rect::from_min_size(
            tile.min + Vec2::splat(sp(Spacing::S4)),
            Vec2::new(
                tile.width() - sp(Spacing::S4) * 2.0,
                tile.height() - line() - sp(Spacing::S6) * 2.0,
            ),
        );
        // Tiles shrunk to fit twelve pins on a short window keep only their caption.
        if thumb.height() >= px(Layout::Chip) {
            thumbnail(ui, thumb, picture, token, Radius::Small);
        }
        let caption_y = tile.bottom() - sp(Spacing::S4) - line() / 2.0;
        dot(
            ui,
            Pos2::new(
                tile.left() + sp(Spacing::S6) + px(Layout::Dot) / 2.0,
                caption_y,
            ),
            token.color(),
        );
        let name_left = tile.left() + sp(Spacing::S6) * 2.0 + px(Layout::Dot);
        let name = paint_text(
            ui,
            Pos2::new(name_left, caption_y),
            Align2::LEFT_CENTER,
            &model_slot.plugin.identity.name,
            TypographyRole::Label,
            ColorToken::Text,
            tile.right() - name_left - sp(Spacing::S6),
        );
        paint_text(
            ui,
            Pos2::new(name.right() + sp(Spacing::S4), caption_y),
            Align2::LEFT_CENTER,
            format!("· {}", model_rack.name),
            TypographyRole::Label,
            ColorToken::Dim,
            (tile.right() - name.right() - sp(Spacing::S8)).max(0.0),
        );
        let label = format!("{}, {}", model_slot.plugin.identity.name, model_rack.name);
        if hovered {
            let mark = Rect::from_min_size(
                Pos2::new(thumb.right() - px(Layout::Chip), thumb.top()),
                Vec2::splat(px(Layout::Chip)),
            );
            let unpin = ui.interact(mark, id.with("unpin"), Sense::click());
            ui.painter().rect_filled(
                mark,
                radius(Radius::Small),
                color(if unpin.hovered() {
                    ColorToken::SegmentOff
                } else {
                    ColorToken::Surface
                }),
            );
            paint_text(
                ui,
                mark.center(),
                Align2::CENTER_CENTER,
                "×",
                TypographyRole::Body,
                ColorToken::Text,
                mark.width(),
            );
            unpin.widget_info(|| {
                WidgetInfo::labeled(WidgetType::Button, true, format!("Unpin {label}"))
            });
            if unpin.clicked() {
                return HotTileAction::Unpin;
            }
        }
        focus_outline(ui, &response, tile);
        response.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, selected, &label));
        peek_on_hover(
            ui,
            &response,
            tile,
            &model_slot.plugin.identity.name,
            picture,
        );
        if response.clicked() {
            HotTileAction::Open
        } else {
            HotTileAction::None
        }
    }
}

/// What a click on a hot plug-in asked for.
enum HotTileAction {
    None,
    Open,
    Unpin,
}

/// A dim title at the top of a strip. Returns the y below it.
fn strip_title(ui: &Ui, inner: Rect, title: &str) -> f32 {
    paint_text(
        ui,
        Pos2::new(inner.left(), inner.top() + line() / 2.0),
        Align2::LEFT_CENTER,
        title,
        TypographyRole::Label,
        ColorToken::Dim,
        inner.width(),
    );
    inner.top() + line() + sp(Spacing::S6)
}

/// A small card centred in `body` with `message` and an optional primary action. Returns
/// whether the action was clicked.
fn pane_notice(
    ui: &mut Ui,
    body: Rect,
    message: &str,
    action: Option<(&str, Option<&str>)>,
) -> bool {
    let width = (body.width() - sp(Spacing::S24) * 2.0).min(px(Layout::PopoverWidth));
    let height = if action.is_some() {
        line() + px(Layout::Control) + sp(Spacing::S12) * 3.0
    } else {
        line() + sp(Spacing::S12) * 2.0
    };
    let notice = Rect::from_center_size(body.center(), Vec2::new(width, height));
    card(ui, notice, false);
    let inner = notice.shrink(sp(Spacing::S12));
    paint_text(
        ui,
        Pos2::new(inner.center().x, inner.top() + line() / 2.0),
        Align2::CENTER_CENTER,
        message,
        TypographyRole::Body,
        ColorToken::Text,
        inner.width(),
    );
    let Some((label, key)) = action else {
        return false;
    };
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(
                Pos2::new(inner.left(), inner.bottom() - px(Layout::Control)),
                inner.right_bottom(),
            ))
            .layout(EguiLayout::top_down(Align::Center)),
    );
    button(&mut row, label, key, ButtonKind::Primary, true).clicked()
}

/// The dashed row that pins the selected slot; faint and inert when nothing can be pinned.
fn pin_row(ui: &mut Ui, rect: Rect, id: egui::Id, label: &str, enabled: bool) -> egui::Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, id, sense);
    dashed_rect(
        ui,
        rect,
        if enabled && response.hovered() {
            ColorToken::Dim
        } else {
            ColorToken::Faint
        },
        Radius::Control,
    );
    paint_text(
        ui,
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        TypographyRole::Label,
        if enabled {
            ColorToken::Dim
        } else {
            ColorToken::Faint
        },
        rect.width(),
    );
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    response
}
