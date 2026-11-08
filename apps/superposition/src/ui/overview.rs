//! The overview: a page rail and one narrow strip per rack, console style. A strip reads top to
//! bottom like its signal: route, the eight-slot chain, meters and gain, mute and bypass, and the
//! rack's state. Clicking a plug-in opens it in the rack view.

use std::cmp::Ordering;

use eframe::egui::{
    self, Align, Align2, Frame, Pos2, Rect, Response, Sense, Ui, UiBuilder, Vec2, WidgetInfo,
    WidgetType,
};
use sp_model::{MAX_PAGES, MAX_RACKS, MAX_SLOTS_PER_RACK};
use sp_ui::components::StateToken;
use sp_ui::design::{ColorToken, Layout, Radius, Spacing, TypographyRole};

use super::previews::{peek_on_hover, thumbnail};
use super::show::{RouteChips, margin, paint_gain, pointer_or};
use super::style::{
    card, color, dashed_rect, dot, focus_outline, galley, hairline, line, paint_text, px, radius,
    sp, toggle_at,
};
use super::{LiveRackApp, Overlay, Picker};

/// Height of one page row in the rail: the name line and its level bars.
fn page_row_height() -> f32 {
    line() + sp(Spacing::S6) + sp(Spacing::S8) * 2.0
}

impl LiveRackApp {
    pub(super) fn draw_overview(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(color(ColorToken::Base))
                    .inner_margin(margin(Spacing::S12, Spacing::S12)),
            )
            .show(ctx, |ui| {
                let area = ui.max_rect();
                let rail = Rect::from_min_size(
                    area.min,
                    Vec2::new(px(Layout::PageRailWidth), area.height()),
                );
                self.draw_page_rail(ui, rail);
                let strips = Rect::from_min_max(
                    Pos2::new(rail.right() + sp(Spacing::S8), area.top()),
                    area.max,
                );
                let mut strips_ui = ui.new_child(UiBuilder::new().max_rect(strips));
                self.draw_strips(&mut strips_ui, strips);
            });
    }

    /// `All racks`, each page with small level bars for its racks, and `+ New page`.
    /// Right-click a page for Rename and Remove.
    fn draw_page_rail(&mut self, ui: &mut Ui, rect: Rect) {
        card(ui, rect, false);
        let inner = rect.shrink(sp(Spacing::S8));
        let id = ui.id().with("page_rail");
        paint_text(
            ui,
            Pos2::new(inner.left() + sp(Spacing::S8), inner.top() + line() / 2.0),
            Align2::LEFT_CENTER,
            "Pages",
            TypographyRole::Label,
            ColorToken::Dim,
            inner.width(),
        );
        let model = &self.controller.document().model;
        let all: Vec<usize> = (0..model.racks.len()).collect();
        let pages: Vec<(String, Vec<usize>)> = model
            .pages
            .iter()
            .map(|page| {
                let racks = model
                    .racks
                    .iter()
                    .enumerate()
                    .filter(|(_, rack)| page.racks.contains(&rack.id))
                    .map(|(index, _)| index)
                    .collect();
                (page.name.clone(), racks)
            })
            .collect();
        // Sixteen pages outgrow a short window, so the rows scroll.
        let rows = Rect::from_min_max(
            Pos2::new(inner.left(), inner.top() + line() + sp(Spacing::S6)),
            inner.max,
        );
        let mut rows_ui = ui.new_child(UiBuilder::new().max_rect(rows));
        egui::ScrollArea::vertical()
            .id_salt("page_rail")
            .auto_shrink([false, false])
            .show(&mut rows_ui, |ui| {
                ui.spacing_mut().item_spacing.y = sp(Spacing::S2);
                let row = |ui: &mut Ui, height: f32| {
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover())
                        .0
                };
                let all_row = row(ui, page_row_height());
                if self
                    .page_row(
                        ui,
                        all_row,
                        id.with("all"),
                        None,
                        "All racks",
                        &all,
                        self.page.is_none(),
                    )
                    .clicked()
                {
                    self.show_page(None);
                }
                for (index, (name, racks)) in pages.iter().enumerate() {
                    let page_row = row(ui, page_row_height());
                    let response = self.page_row(
                        ui,
                        page_row,
                        id.with(("page", index)),
                        Some(index + 1),
                        name,
                        racks,
                        self.page == Some(index),
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
                if pages.len() < MAX_PAGES {
                    let add = row(ui, px(Layout::Control));
                    if new_page_row(ui, add, id.with("new_page")).clicked() {
                        self.new_page(None);
                    }
                }
            });
    }

    /// One page row: number and name, the rack count, and one level bar per rack. Accent while
    /// shown.
    #[allow(
        clippy::too_many_arguments,
        reason = "a row draws one page's number, name, racks, and state"
    )]
    fn page_row(
        &self,
        ui: &mut Ui,
        rect: Rect,
        id: egui::Id,
        number: Option<usize>,
        name: &str,
        racks: &[usize],
        active: bool,
    ) -> Response {
        let response = ui.interact(rect, id, Sense::click());
        let (fill, text_token, secondary) = if active {
            (
                Some(ColorToken::Accent),
                ColorToken::AccentText,
                ColorToken::AccentText,
            )
        } else if response.hovered() {
            (
                Some(ColorToken::HoverFill),
                ColorToken::Text,
                ColorToken::Text,
            )
        } else {
            (None, ColorToken::Text, ColorToken::Dim)
        };
        if let Some(fill) = fill {
            ui.painter()
                .rect_filled(rect, radius(Radius::Control), color(fill));
        }
        let inner = rect.shrink(sp(Spacing::S8));
        let name_y = inner.top() + line() / 2.0;
        let mut left = inner.left();
        if let Some(number) = number {
            left = paint_text(
                ui,
                Pos2::new(left, name_y),
                Align2::LEFT_CENTER,
                number.to_string(),
                TypographyRole::Mono,
                secondary,
                inner.width(),
            )
            .right()
                + sp(Spacing::S6);
        }
        let count = paint_text(
            ui,
            Pos2::new(inner.right(), name_y),
            Align2::RIGHT_CENTER,
            racks.len().to_string(),
            TypographyRole::Mono,
            secondary,
            inner.width(),
        );
        paint_text(
            ui,
            Pos2::new(left, name_y),
            Align2::LEFT_CENTER,
            name,
            TypographyRole::Body,
            text_token,
            (count.left() - left - sp(Spacing::S6)).max(0.0),
        );
        let bar = px(Layout::LoadBarWidth);
        let bars = Rect::from_min_max(
            Pos2::new(inner.left(), inner.bottom() - sp(Spacing::S6)),
            inner.right_bottom(),
        );
        let online = self.online();
        for (position, &rack) in racks.iter().enumerate() {
            #[allow(clippy::cast_precision_loss, reason = "at most 64 racks")]
            let x = bars.left() + position as f32 * (bar + sp(Spacing::S2));
            if x + bar > bars.right() {
                break;
            }
            let levels = self.rack_levels(rack);
            let level = if online {
                levels[1].0.max(levels[2].0)
            } else {
                0.0
            };
            let height = (bars.height() * level).max(px(Layout::Hairline) * 2.0);
            ui.painter().rect_filled(
                Rect::from_min_max(
                    Pos2::new(x, bars.bottom() - height),
                    Pos2::new(x + bar, bars.bottom()),
                ),
                egui::CornerRadius::ZERO,
                color(secondary),
            );
        }
        focus_outline(ui, &response, rect);
        response.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, active, name));
        response
    }

    /// The shown racks as strips, then an add-rack strip. A narrow window scrolls sideways.
    fn draw_strips(&mut self, ui: &mut Ui, area: Rect) {
        let can_add = self.controller.document().model.racks.len() < MAX_RACKS;
        let visible = self.visible_racks();
        if visible.is_empty() {
            self.draw_empty_overview(ui, area, can_add);
            return;
        }
        let width = px(Layout::StripWidth);
        let gap = sp(Spacing::S6);
        let add_width = if can_add {
            px(Layout::AddRackWidth) + gap
        } else {
            0.0
        };
        #[allow(clippy::cast_precision_loss, reason = "at most 64 racks")]
        let content_width = (width + gap) * visible.len() as f32 - gap + add_width;
        let scroll_to_selected = std::mem::take(&mut self.scroll_to_selected);
        egui::ScrollArea::horizontal()
            .id_salt("strips")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let (content, _) =
                    ui.allocate_exact_size(Vec2::new(content_width, area.height()), Sense::hover());
                for (position, &rack) in visible.iter().enumerate() {
                    #[allow(clippy::cast_precision_loss, reason = "at most 64 racks")]
                    let left = content.left() + (width + gap) * position as f32;
                    let strip = Rect::from_min_size(
                        Pos2::new(left, content.top()),
                        Vec2::new(width, content.height()),
                    );
                    if scroll_to_selected && rack == self.selected_rack {
                        ui.scroll_to_rect(strip, Some(Align::Center));
                    }
                    // Strips scrolled out of view cost nothing.
                    if ui.is_rect_visible(strip) {
                        self.draw_strip(ui, strip, rack);
                    }
                }
                if can_add {
                    let add = Rect::from_min_max(
                        Pos2::new(content.right() - px(Layout::AddRackWidth), content.top()),
                        content.max,
                    );
                    if add_rack_card(ui, add).clicked() {
                        self.add_rack();
                    }
                }
            });
    }

    /// An empty session, or a page with no racks: a centred message and the add-rack strip.
    fn draw_empty_overview(&mut self, ui: &mut Ui, area: Rect, can_add: bool) {
        let message = if can_add {
            Rect::from_min_max(
                area.min,
                Pos2::new(
                    area.right() - px(Layout::AddRackWidth) - sp(Spacing::S6),
                    area.bottom(),
                ),
            )
        } else {
            area
        };
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
        let center = message.center();
        for (index, (text, role, token)) in lines.into_iter().enumerate() {
            #[allow(clippy::cast_precision_loss, reason = "three lines")]
            let y = center.y + (index as f32 - 1.0) * (line() + sp(Spacing::S6));
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
        if can_add {
            let add = Rect::from_min_max(
                Pos2::new(area.right() - px(Layout::AddRackWidth), area.top()),
                area.max,
            );
            if add_rack_card(ui, add).clicked() {
                self.add_rack();
            }
        }
    }

    /// One rack strip.
    fn draw_strip(&mut self, ui: &mut Ui, rect: Rect, rack: usize) {
        let Some(model_rack) = self.rack_fields(rack) else {
            return;
        };
        let selected = rack == self.selected_rack;
        card(ui, rect, selected);
        let inner = rect.shrink(sp(Spacing::S8));
        let id = ui.id().with(("strip", rack));
        let mut y = inner.top();

        // Header: index and name. Click selects, double-click renames, right-click opens the
        // rack menu.
        let header =
            Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(inner.width(), line()));
        let renaming = self
            .rename
            .as_ref()
            .is_some_and(|(renaming, _)| *renaming == rack);
        if renaming {
            self.rename_field(ui, header.expand2(Vec2::Y * sp(Spacing::S2)));
        } else {
            let index = paint_text(
                ui,
                header.left_center(),
                Align2::LEFT_CENTER,
                format!("{:02}", rack + 1),
                TypographyRole::Mono,
                ColorToken::Dim,
                header.width(),
            );
            paint_text(
                ui,
                Pos2::new(index.right() + sp(Spacing::S6), header.center().y),
                Align2::LEFT_CENTER,
                &model_rack.name,
                TypographyRole::Strong,
                ColorToken::Text,
                (header.right() - index.right() - sp(Spacing::S6)).max(0.0),
            );
            let response = ui.interact(header, id.with("header"), Sense::click());
            response.widget_info(|| {
                WidgetInfo::selected(
                    WidgetType::SelectableLabel,
                    true,
                    selected,
                    &model_rack.name,
                )
            });
            if response.double_clicked() {
                self.select_rack(rack);
                self.rename = Some((rack, model_rack.name.clone()));
            } else if response.clicked() {
                self.select_rack(rack);
                self.selected_slot = None;
            }
            if response.secondary_clicked() {
                self.select_rack(rack);
                self.overlay = Overlay::RackMenu {
                    rack,
                    at: pointer_or(ui, header.left_bottom()),
                };
            }
        }
        y += line() + sp(Spacing::S6);
        y = self.draw_route_chips(
            ui,
            id.with("route"),
            Pos2::new(inner.left(), y),
            inner.width(),
            rack,
            RouteChips::Both,
        ) + sp(Spacing::S8);

        // The eight-slot chain.
        let slot_height = px(Layout::StripSlot);
        for slot in 0..MAX_SLOTS_PER_RACK {
            let row = Rect::from_min_size(
                Pos2::new(inner.left(), y),
                Vec2::new(inner.width(), slot_height),
            );
            y += slot_height + sp(Spacing::S4);
            match slot.cmp(&model_rack.slots) {
                Ordering::Less => {
                    self.draw_strip_slot(ui, row, id.with(("slot", slot)), rack, slot);
                }
                Ordering::Equal => {
                    if add_plugin_row(ui, row, id.with("add_plugin"), TypographyRole::Label)
                        .clicked()
                    {
                        self.select_rack(rack);
                        self.overlay = Overlay::Picker(Picker {
                            rack,
                            query: String::new(),
                            cursor: 0,
                        });
                    }
                }
                Ordering::Greater => dashed_rect(ui, row, ColorToken::Hairline, Radius::Control),
            }
        }

        self.draw_strip_foot(ui, inner, y, rack, id);
    }

    /// A strip's foot, from the bottom up: state, mute and bypass, then meters and the fader
    /// filling the room between the chain (ending at `top`) and the toggles.
    fn draw_strip_foot(&mut self, ui: &mut Ui, inner: Rect, top: f32, rack: usize, id: egui::Id) {
        let Some(model_rack) = self.rack_fields(rack) else {
            return;
        };
        let status = Rect::from_min_max(
            Pos2::new(inner.left(), inner.bottom() - line()),
            inner.right_bottom(),
        );
        self.draw_strip_status(ui, status, rack, model_rack.muted);
        let toggles = Rect::from_min_max(
            Pos2::new(
                inner.left(),
                status.top() - sp(Spacing::S6) - px(Layout::Control),
            ),
            Pos2::new(inner.right(), status.top() - sp(Spacing::S6)),
        );
        let half = (toggles.width() - sp(Spacing::S4)) / 2.0;
        let mute = Rect::from_min_size(toggles.min, Vec2::new(half, toggles.height()));
        let bypass = Rect::from_min_size(
            Pos2::new(toggles.right() - half, toggles.top()),
            Vec2::new(half, toggles.height()),
        );
        let gain_db = model_rack.gain_db;
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
        if toggle_at(
            ui,
            bypass,
            id.with("bypass"),
            "Bypass",
            model_rack.bypassed,
            ColorToken::Info,
        )
        .clicked()
        {
            self.set_rack_controls(rack, gain_db, model_rack.muted, !model_rack.bypassed);
        }
        let levels_top = top + sp(Spacing::S8);
        let gain_row = toggles.top() - sp(Spacing::S8) - line();
        if gain_row - levels_top < px(Layout::Control) * 2.0 {
            return;
        }
        ui.painter().hline(
            inner.x_range(),
            levels_top - sp(Spacing::S4),
            hairline(ColorToken::Hairline),
        );
        let meters_right = self.draw_meters(
            ui,
            rack,
            &[0, 1, 2],
            (inner.left(), levels_top + sp(Spacing::S4), gain_row),
            px(Layout::MeterNarrowWidth),
        );
        let fader = Rect::from_min_max(
            Pos2::new(meters_right + sp(Spacing::S8), levels_top + sp(Spacing::S4)),
            Pos2::new(inner.right(), gain_row - px(Layout::Caption)),
        );
        self.draw_gain_fader(ui, id, fader, rack);
        paint_gain(
            ui,
            Pos2::new(fader.center().x, gain_row + line() / 2.0),
            Align2::CENTER_CENTER,
            gain_db,
            fader.width() + sp(Spacing::S8),
        );
    }

    /// One slot row in a strip: picture thumbnail, name, and state dot. Click opens the plug-in
    /// in the rack view; right-click opens the slot menu.
    fn draw_strip_slot(&mut self, ui: &mut Ui, row: Rect, id: egui::Id, rack: usize, slot: usize) {
        let Some(model_slot) = self.slot_fields(rack, slot) else {
            return;
        };
        let token = self.slot_token(rack, slot);
        let selected = rack == self.selected_rack && self.selected_slot == Some(slot);
        let response = ui.interact(row, id, Sense::click());
        if response.hovered() || selected {
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
        let picture = self.previews.get(&model_slot.id);
        let thumb_height = row.height() - sp(Spacing::S6) * 2.0;
        let thumb = Rect::from_min_size(
            Pos2::new(
                row.left() + sp(Spacing::S4),
                row.center().y - thumb_height / 2.0,
            ),
            Vec2::new(thumb_height * 1.5, thumb_height),
        );
        thumbnail(ui, thumb, picture, token, Radius::Small);
        let dot_x = row.right() - sp(Spacing::S8);
        dot(ui, Pos2::new(dot_x, row.center().y), token.color());
        let name = &model_slot.name;
        let name_token = match token {
            StateToken::Missing | StateToken::Faulted => ColorToken::Fault,
            StateToken::Bypassed | StateToken::Unloaded => ColorToken::Dim,
            _ => ColorToken::Text,
        };
        let name_left = thumb.right() + sp(Spacing::S6);
        paint_text(
            ui,
            Pos2::new(name_left, row.center().y),
            Align2::LEFT_CENTER,
            name,
            TypographyRole::Label,
            name_token,
            (dot_x - name_left - sp(Spacing::S6)).max(0.0),
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
            self.select_rack(rack);
            self.selected_slot = Some(slot);
            self.overlay = Overlay::SlotMenu {
                rack,
                slot,
                at: pointer_or(ui, row.left_bottom()),
            };
        }
    }

    /// The rack's state dot and word, the ready count, and `muted`.
    fn draw_strip_status(&self, ui: &mut Ui, rect: Rect, rack: usize, muted: bool) {
        let token = self.rack_token(rack);
        dot(
            ui,
            Pos2::new(rect.left() + px(Layout::Dot) / 2.0, rect.center().y),
            token.color(),
        );
        // The row is narrow: `muted` keeps its place at the right, the count follows the state
        // word when it fits, and the word truncates.
        let right = if muted {
            paint_text(
                ui,
                rect.right_center(),
                Align2::RIGHT_CENTER,
                "muted",
                TypographyRole::Label,
                ColorToken::Warn,
                rect.width(),
            )
            .left()
                - sp(Spacing::S6)
        } else {
            rect.right()
        };
        let slots = self
            .controller
            .document()
            .model
            .racks
            .get(rack)
            .map_or(0, |rack| rack.slots.len());
        let ready = (0..slots)
            .filter(|slot| self.slot_token(rack, *slot) == StateToken::Ready)
            .count();
        let count = galley(
            ui,
            format!("{ready}/{slots}"),
            TypographyRole::Mono,
            ColorToken::Dim,
            f32::INFINITY,
        );
        let left = rect.left() + px(Layout::Dot) + sp(Spacing::S6);
        let label = paint_text(
            ui,
            Pos2::new(left, rect.center().y),
            Align2::LEFT_CENTER,
            token.label(),
            TypographyRole::Label,
            ColorToken::Text,
            (right - left - count.size().x - sp(Spacing::S6)).max(0.0),
        );
        let count_left = label.right() + sp(Spacing::S6);
        if count_left + count.size().x <= right {
            ui.painter().galley(
                Pos2::new(count_left, rect.center().y - count.size().y / 2.0),
                count,
                color(ColorToken::Dim),
            );
        }
        ui.interact(rect, ui.id().with(("status", rack)), Sense::hover())
            .on_hover_text(token.meaning());
    }
}

/// A dashed `+ Add plug-in` row.
pub(super) fn add_plugin_row(
    ui: &mut Ui,
    rect: Rect,
    id: egui::Id,
    role: TypographyRole,
) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    let token = if response.hovered() {
        ColorToken::Text
    } else {
        ColorToken::Dim
    };
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, radius(Radius::Control), color(ColorToken::HoverFill));
    }
    dashed_rect(
        ui,
        rect,
        if response.hovered() {
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
        "+ Add plug-in",
        role,
        token,
        rect.width(),
    );
    focus_outline(ui, &response, rect);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Add plug-in"));
    response
}

/// The dashed add-rack strip.
fn add_rack_card(ui: &mut Ui, rect: Rect) -> Response {
    let response = ui.interact(rect, ui.id().with("add_rack"), Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.painter()
            .rect_filled(rect, radius(Radius::Card), color(ColorToken::Surface));
    }
    dashed_rect(
        ui,
        rect,
        if hovered {
            ColorToken::Dim
        } else {
            ColorToken::Faint
        },
        Radius::Card,
    );
    let token = if hovered {
        ColorToken::Text
    } else {
        ColorToken::Dim
    };
    paint_text(
        ui,
        rect.center() - Vec2::Y * line() / 2.0,
        Align2::CENTER_CENTER,
        "+",
        TypographyRole::Title,
        token,
        rect.width(),
    );
    paint_text(
        ui,
        rect.center() + Vec2::Y * line() / 2.0,
        Align2::CENTER_CENTER,
        "Add rack",
        TypographyRole::Label,
        token,
        rect.width(),
    );
    focus_outline(ui, &response, rect);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "Add rack"));
    response
}

/// The rail's `+ New page` row.
fn new_page_row(ui: &mut Ui, rect: Rect, id: egui::Id) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, radius(Radius::Control), color(ColorToken::HoverFill));
    }
    paint_text(
        ui,
        Pos2::new(rect.left() + sp(Spacing::S8), rect.center().y),
        Align2::LEFT_CENTER,
        "+ New page",
        TypographyRole::Body,
        if response.hovered() {
            ColorToken::Text
        } else {
            ColorToken::Dim
        },
        rect.width(),
    );
    focus_outline(ui, &response, rect);
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "New page"));
    response
}
