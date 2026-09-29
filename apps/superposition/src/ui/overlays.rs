//! Menus, popovers, the plug-in picker, and modals. Popovers sit on a transparent scrim that
//! closes them on an outside click; modals ignore it.

use eframe::egui::{self, Align, Align2, Layout as EguiLayout, Pos2, Sense, Ui, Vec2};
use sp_model::{MAX_SLOTS_PER_RACK, PhysicalChannels, RackChannelRoute, SlotSidechain};
use sp_ui::design::{ColorToken, Layout, Spacing, TypographyRole};

use super::style::{
    Mark, action, action_with, choice_row, color, hairline, jack, line, list_row, note, popover,
    popover_title, px, scrim, sp, text, text_field, title,
};
use super::{LiveRackApp, Modal, Overlay, SlotAction, channels_label, show::latency_label};
use crate::CatalogPlugin;

/// One menu row: label, key hint, enabled, and what it does.
type MenuItem<A> = Option<(&'static str, &'static str, bool, A)>;

#[derive(Clone, Copy)]
enum RackCommand {
    Rename,
    Route,
    Pages,
    MoveLeft,
    MoveRight,
    Remove,
}

#[derive(Clone, Copy)]
enum PageCommand {
    Rename,
    Remove,
}

#[derive(Clone, Copy)]
enum SlotCommand {
    Action(SlotAction),
    Sidechain,
}

/// A plug-in picker row.
struct PickerRow {
    vendor: String,
    name: String,
    /// Why it cannot be added, if it cannot.
    reason: Option<&'static str>,
    plugin: Option<CatalogPlugin>,
}

impl LiveRackApp {
    pub(super) fn draw_overlay(&mut self, ctx: &egui::Context) {
        let popover_open = matches!(
            self.overlay,
            Overlay::RackMenu { .. }
                | Overlay::SlotMenu { .. }
                | Overlay::Route { .. }
                | Overlay::Sidechain { .. }
                | Overlay::RackPages { .. }
                | Overlay::PageMenu { .. }
                | Overlay::PageRename { .. }
                | Overlay::Picker(_)
        );
        let modal_open = matches!(self.overlay, Overlay::Modal(_));
        if popover_open || modal_open {
            // Modals ignore the scrim; popovers close on an outside click.
            let outside_click = scrim(ctx, egui::Id::new("scrim"));
            if outside_click && popover_open {
                self.overlay = Overlay::None;
                return;
            }
        }
        match self.overlay {
            Overlay::None => {}
            Overlay::RackMenu { rack, at } => self.draw_rack_menu(ctx, rack, at),
            Overlay::SlotMenu { rack, slot, at } => self.draw_slot_menu(ctx, rack, slot, at),
            Overlay::Route { rack, at } => self.draw_route_popover(ctx, rack, at),
            Overlay::Sidechain { rack, slot, at } => {
                self.draw_sidechain_popover(ctx, rack, slot, at);
            }
            Overlay::RackPages { rack, at } => self.draw_rack_pages(ctx, rack, at),
            Overlay::PageMenu { page, at } => self.draw_page_menu(ctx, page, at),
            Overlay::PageRename { page, at, .. } => self.draw_page_rename(ctx, page, at),
            Overlay::Picker(_) => self.draw_picker(ctx),
            Overlay::Modal(modal) => self.draw_modal(ctx, modal),
        }
    }

    fn draw_rack_menu(&mut self, ctx: &egui::Context, rack: usize, at: Pos2) {
        let visible = self.visible_racks();
        let position = visible.iter().position(|shown| *shown == rack);
        let items: [MenuItem<RackCommand>; 8] = [
            Some(("Rename", "dbl-click", true, RackCommand::Rename)),
            Some(("Route…", "", true, RackCommand::Route)),
            Some(("Pages…", "", true, RackCommand::Pages)),
            None,
            Some((
                "Move left",
                "⌘←",
                position.is_some_and(|position| position > 0),
                RackCommand::MoveLeft,
            )),
            Some((
                "Move right",
                "⌘→",
                position.is_some_and(|position| position + 1 < visible.len()),
                RackCommand::MoveRight,
            )),
            None,
            Some(("Remove rack…", "", true, RackCommand::Remove)),
        ];
        let Some(command) = menu(ctx, "rack_menu", at, &items) else {
            return;
        };
        self.overlay = Overlay::None;
        self.select_rack(rack);
        match command {
            RackCommand::Rename => {
                let name = self.controller.document().model.racks[rack].name.clone();
                self.rename = Some((rack, name));
            }
            RackCommand::Route => self.overlay = Overlay::Route { rack, at },
            RackCommand::Pages => self.overlay = Overlay::RackPages { rack, at },
            RackCommand::MoveLeft => self.move_selected_rack(-1),
            RackCommand::MoveRight => self.move_selected_rack(1),
            RackCommand::Remove => self.open_remove_rack(rack),
        }
    }

    fn draw_slot_menu(&mut self, ctx: &egui::Context, rack: usize, slot: usize, at: Pos2) {
        let Some(model_slot) = self
            .controller
            .document()
            .model
            .racks
            .get(rack)
            .and_then(|rack| rack.slots.get(slot))
            .cloned()
        else {
            self.overlay = Overlay::None;
            return;
        };
        let slots = self.controller.document().model.racks[rack].slots.len();
        let missing = self.slot_token(rack, slot) == sp_ui::components::StateToken::Missing;
        let sidechain = self.accepts_sidechain(&model_slot);
        // Plug-ins scanned before sidechain detection report none until a rescan.
        let outdated_catalog = self
            .product
            .as_ref()
            .is_ok_and(|product| product.stale_catalog_entries() > 0);
        let items: [MenuItem<SlotCommand>; 8] = [
            Some((
                "Open editor",
                "↩",
                !missing,
                SlotCommand::Action(SlotAction::Editor(slot)),
            )),
            Some((
                if model_slot.bypassed {
                    "Enable"
                } else {
                    "Bypass"
                },
                "B",
                true,
                SlotCommand::Action(SlotAction::Bypass(slot)),
            )),
            Some(if sidechain {
                ("Sidechain…", "", true, SlotCommand::Sidechain)
            } else if outdated_catalog {
                (
                    "Sidechain unknown: rescan plug-ins",
                    "",
                    false,
                    SlotCommand::Sidechain,
                )
            } else {
                ("No sidechain input", "", false, SlotCommand::Sidechain)
            }),
            None,
            Some((
                "Move up",
                "",
                slot > 0,
                SlotCommand::Action(SlotAction::MoveUp(slot)),
            )),
            Some((
                "Move down",
                "",
                slot + 1 < slots,
                SlotCommand::Action(SlotAction::MoveDown(slot)),
            )),
            None,
            Some((
                "Remove plug-in",
                "Del",
                true,
                SlotCommand::Action(SlotAction::Remove(slot)),
            )),
        ];
        let Some(command) = menu(ctx, "slot_menu", at, &items) else {
            return;
        };
        self.overlay = Overlay::None;
        self.select_rack(rack);
        self.selected_slot = Some(slot);
        match command {
            SlotCommand::Action(action) => self.slot_action(ctx, action),
            SlotCommand::Sidechain => self.overlay = Overlay::Sidechain { rack, slot, at },
        }
    }

    /// Input mode and channels, and the output pair, for one rack. Changes re-route at the next
    /// block without restarting anything.
    #[allow(
        clippy::too_many_lines,
        reason = "one popover lays out its two jack grids in reading order"
    )]
    fn draw_route_popover(&mut self, ctx: &egui::Context, rack_index: usize, at: Pos2) {
        let model = &self.controller.document().model;
        let Some(rack) = model.racks.get(rack_index).cloned() else {
            self.overlay = Overlay::None;
            return;
        };
        let current = super::rack_route(model, &rack);
        let inputs = self.device_channels(true);
        let outputs = self.device_channels(false);
        let device = self.output_device_name();
        let mut next = None;
        let mut done = false;
        popover(
            ctx,
            egui::Id::new("route"),
            at,
            Align2::LEFT_TOP,
            None,
            |ui| {
                ui.set_width(px(Layout::PopoverWidth));
                popover_title(ui, &format!("Route for {}", rack.name), &device);
                let input = current
                    .input
                    .unwrap_or(PhysicalChannels::Mono { channel: 0 });
                let stereo = matches!(input, PhysicalChannels::Stereo { .. });
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = sp(Spacing::S4);
                    text(ui, "Input", TypographyRole::Body, ColorToken::Dim);
                    ui.add_space(sp(Spacing::S8));
                    if action_with(ui, "Mono", None, true, !stereo).clicked() && stereo {
                        next = Some(RackChannelRoute {
                            input: Some(PhysicalChannels::Mono {
                                channel: first_channel(input),
                            }),
                            ..current
                        });
                    }
                    if action_with(ui, "Stereo", None, true, stereo).clicked() && !stereo {
                        let left = first_channel(input) - first_channel(input) % 2;
                        next = Some(RackChannelRoute {
                            input: Some(PhysicalChannels::Stereo {
                                left,
                                right: left + 1,
                            }),
                            ..current
                        });
                    }
                });
                ui.add_space(sp(Spacing::S4));
                if let Some(channels) = jack_grid(ui, "route_in", Some(input), inputs, !stereo) {
                    next = Some(RackChannelRoute {
                        input: Some(channels),
                        ..current
                    });
                }
                ui.add_space(sp(Spacing::S10));
                text(
                    ui,
                    "Output, stereo pair",
                    TypographyRole::Body,
                    ColorToken::Dim,
                );
                ui.add_space(sp(Spacing::S4));
                if let Some(channels) =
                    jack_grid(ui, "route_out", Some(current.output), outputs, false)
                {
                    next = Some(RackChannelRoute {
                        output: channels,
                        ..current
                    });
                }
                if !self.route_fits(current) {
                    ui.add_space(sp(Spacing::S8));
                    note(
                        ui,
                        "This route uses channels the selected device does not have.",
                        ColorToken::Warn,
                    );
                }
                ui.add_space(sp(Spacing::S12));
                ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                    done = action(ui, "Done", Some("Esc"), true).clicked();
                });
            },
        );
        if done {
            self.overlay = Overlay::None;
        }
        if let Some(route) = next {
            self.set_rack_route(rack_index, route);
        }
    }

    fn set_rack_route(&mut self, rack_index: usize, route: RackChannelRoute) {
        // While audio runs, a route the device lacks would silence every callback.
        if self.online() && !self.route_fits(route) {
            self.show_fault(
                "That channel is not on the running audio device. The route is unchanged."
                    .to_owned(),
            );
            return;
        }
        let before = self.rack_ids();
        let rack_id = self.controller.document().model.racks[rack_index]
            .id
            .clone();
        self.controller
            .document_mut()
            .model
            .rack_routes
            .insert(rack_id, route);
        // Every rack is kept: the new graph re-routes at the next block, no restart.
        self.apply_rack_edit(&before, &[]);
        self.set_status("Rack route updated");
    }

    /// Whether a slot's plug-in declares a stereo aux input usable as a sidechain.
    fn accepts_sidechain(&self, slot: &sp_model::PluginSlot) -> bool {
        self.catalog_plugins.iter().any(|plugin| {
            plugin.sidechain_capable
                && plugin.descriptor.identity.unique_id == slot.plugin.identity.unique_id
                && plugin.descriptor.fingerprint.digest == slot.plugin.fingerprint.digest
        })
    }

    /// None, a physical input pair (same block), or another rack's post-fader output (one
    /// block late) for one plug-in's sidechain.
    #[allow(
        clippy::too_many_lines,
        reason = "the three source kinds read top to bottom in one popover"
    )]
    fn draw_sidechain_popover(
        &mut self,
        ctx: &egui::Context,
        rack_index: usize,
        slot: usize,
        at: Pos2,
    ) {
        let model = &self.controller.document().model;
        let Some(rack) = model.racks.get(rack_index) else {
            self.overlay = Overlay::None;
            return;
        };
        let Some(model_slot) = rack.slots.get(slot) else {
            self.overlay = Overlay::None;
            return;
        };
        let title = format!("Sidechain for {}", model_slot.plugin.identity.name);
        let detail = format!("on {}, slot {}", rack.name, slot + 1);
        let current = model_slot.sidechain.clone();
        let others: Vec<(sp_model::RackId, String, String)> = model
            .racks
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != rack_index)
            .map(|(_, other)| {
                (
                    other.id.clone(),
                    other.name.clone(),
                    format!(
                        "out {}",
                        channels_label(super::rack_route(model, other).output)
                    ),
                )
            })
            .collect();
        let inputs = self.device_channels(true);
        let frames = self.buffer_frames_value();
        let mut next = None;
        let mut done = false;
        popover(
            ctx,
            egui::Id::new("sidechain"),
            at,
            Align2::LEFT_TOP,
            None,
            |ui| {
                ui.set_width(px(Layout::PopoverWidth));
                popover_title(ui, &title, &detail);
                if choice_row(
                    ui,
                    Mark::Radio,
                    current.is_none(),
                    "None",
                    "plug-in uses its own input",
                    true,
                )
                .clicked()
                {
                    next = Some(None);
                }
                ui.add_space(sp(Spacing::S8));
                text(
                    ui,
                    "From a physical input, stereo pair",
                    TypographyRole::Body,
                    ColorToken::Dim,
                );
                ui.add_space(sp(Spacing::S4));
                let physical = match &current {
                    Some(SlotSidechain::PhysicalInput(channels)) => Some(*channels),
                    _ => None,
                };
                if let Some(channels) = jack_grid(ui, "sidechain_in", physical, inputs, false) {
                    next = Some(Some(SlotSidechain::PhysicalInput(channels)));
                }
                ui.add_space(sp(Spacing::S4));
                text(
                    ui,
                    "From another rack, post fader",
                    TypographyRole::Body,
                    ColorToken::Dim,
                );
                egui::ScrollArea::vertical()
                    .max_height(line() * 12.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (id, name, output) in &others {
                            let on =
                                current.as_ref() == Some(&SlotSidechain::RackOutput(id.clone()));
                            if choice_row(ui, Mark::Radio, on, name, output, true).clicked() {
                                next = Some(Some(SlotSidechain::RackOutput(id.clone())));
                            }
                        }
                    });
                if others.is_empty() {
                    text(
                        ui,
                        "No other racks yet.",
                        TypographyRole::Body,
                        ColorToken::Faint,
                    );
                }
                ui.add_space(sp(Spacing::S8));
                note(
                    ui,
                    &format!(
                        "Rack sources arrive one buffer late, {} at {frames} samples. Physical inputs are sample aligned.",
                        latency_label(frames)
                    ),
                    ColorToken::Dim,
                );
                ui.add_space(sp(Spacing::S12));
                ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                    done = action(ui, "Done", Some("Esc"), true).clicked();
                });
            },
        );
        if done {
            self.overlay = Overlay::None;
        }
        if let Some(sidechain) = next {
            self.set_slot_sidechain(rack_index, slot, sidechain);
        }
    }

    /// Sets a slot's sidechain and reloads its rack transactionally, like a topology edit.
    fn set_slot_sidechain(
        &mut self,
        rack_index: usize,
        slot: usize,
        sidechain: Option<SlotSidechain>,
    ) {
        let model = &self.controller.document().model;
        let Some(model_slot) = model
            .racks
            .get(rack_index)
            .and_then(|rack| rack.slots.get(slot))
        else {
            return;
        };
        if model_slot.sidechain == sidechain {
            return;
        }
        if let Some(SlotSidechain::PhysicalInput(channels)) = sidechain
            && self.online()
            && !super::physical_channels_fit(channels, self.device_channels(true))
        {
            self.show_fault(
                "That input pair is not on the running audio device. The sidechain is unchanged."
                    .to_owned(),
            );
            return;
        }
        let name = model_slot.plugin.identity.name.clone();
        let before = self.rack_ids();
        let rack_id = model.racks[rack_index].id.clone();
        self.controller.document_mut().model.racks[rack_index].slots[slot].sidechain = sidechain;
        self.apply_rack_edit(&before, &[rack_id]);
        if self.fault.is_none() {
            self.set_status(&format!("Sidechain for {name} updated. The rack reloaded."));
        }
    }

    /// Which pages one rack is on, with a way to start a new page from it.
    fn draw_rack_pages(&mut self, ctx: &egui::Context, rack: usize, at: Pos2) {
        let model = &self.controller.document().model;
        let Some(rack_id) = model.racks.get(rack).map(|rack| rack.id.clone()) else {
            self.overlay = Overlay::None;
            return;
        };
        let title = format!("Pages for {}", model.racks[rack].name);
        let pages: Vec<(String, bool)> = model
            .pages
            .iter()
            .map(|page| (page.name.clone(), page.racks.contains(&rack_id)))
            .collect();
        let can_add = pages.len() < sp_model::MAX_PAGES;
        let mut toggled = None;
        let mut new_page = false;
        let mut done = false;
        popover(
            ctx,
            egui::Id::new("rack_pages"),
            at,
            Align2::LEFT_TOP,
            None,
            |ui| {
                ui.set_width(px(Layout::PopoverWidth));
                popover_title(ui, &title, "");
                for (index, (name, on)) in pages.iter().enumerate() {
                    if choice_row(ui, Mark::Check, *on, name, "", true).clicked() {
                        toggled = Some(index);
                    }
                }
                if pages.is_empty() {
                    text(ui, "No pages yet.", TypographyRole::Body, ColorToken::Dim);
                }
                ui.add_space(sp(Spacing::S8));
                new_page = action(ui, "New page with this rack", None, can_add).clicked();
                ui.add_space(sp(Spacing::S8));
                note(
                    ui,
                    "Pages only choose which racks show. Audio and routing do not change.",
                    ColorToken::Dim,
                );
                ui.add_space(sp(Spacing::S12));
                ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                    done = action(ui, "Done", Some("Esc"), true).clicked();
                });
            },
        );
        if let Some(page) = toggled {
            self.toggle_rack_page(rack, page);
        }
        if new_page {
            self.overlay = Overlay::None;
            self.new_page(Some(rack));
        } else if done {
            self.overlay = Overlay::None;
        }
    }

    fn draw_page_menu(&mut self, ctx: &egui::Context, page: usize, at: Pos2) {
        let items: [MenuItem<PageCommand>; 2] = [
            Some(("Rename…", "", true, PageCommand::Rename)),
            Some(("Remove page", "", true, PageCommand::Remove)),
        ];
        let Some(command) = menu(ctx, "page_menu", at, &items) else {
            return;
        };
        self.overlay = Overlay::None;
        match command {
            PageCommand::Rename => {
                if let Some(name) = self
                    .controller
                    .document()
                    .model
                    .pages
                    .get(page)
                    .map(|page| page.name.clone())
                {
                    self.overlay = Overlay::PageRename {
                        page,
                        at,
                        draft: name,
                    };
                }
            }
            PageCommand::Remove => self.remove_page(page),
        }
    }

    fn draw_page_rename(&mut self, ctx: &egui::Context, page: usize, at: Pos2) {
        let Overlay::PageRename { draft, .. } = &mut self.overlay else {
            return;
        };
        let mut commit =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        popover(
            ctx,
            egui::Id::new("page_rename"),
            at,
            Align2::LEFT_TOP,
            None,
            |ui| {
                ui.set_width(px(Layout::PopoverWidth));
                popover_title(ui, "Rename page", "");
                let field = text_field(ui, draft, "Page name", ui.available_width());
                if !field.has_focus() && !field.lost_focus() {
                    field.request_focus();
                }
                ui.add_space(sp(Spacing::S12));
                ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                    commit |= action(ui, "Rename", Some("↩"), true).clicked();
                });
            },
        );
        let name = draft.trim().to_owned();
        if commit && !name.is_empty() {
            if let Some(page) = self.controller.document_mut().model.pages.get_mut(page) {
                page.name = name;
            }
            self.overlay = Overlay::None;
        }
    }

    fn picker_rows(&self) -> Vec<PickerRow> {
        let mut rows: Vec<PickerRow> = self
            .catalog_plugins
            .iter()
            .map(|plugin| PickerRow {
                vendor: plugin.descriptor.identity.vendor.clone(),
                name: plugin.descriptor.identity.name.clone(),
                reason: None,
                plugin: Some(plugin.clone()),
            })
            .collect();
        if let Ok(product) = self.product.as_ref() {
            rows.extend(
                product
                    .quarantined_plugins()
                    .into_iter()
                    .map(|entry| PickerRow {
                        vendor: String::new(),
                        name: entry.name,
                        reason: Some("quarantined"),
                        plugin: None,
                    }),
            );
            rows.extend(
                product
                    .unavailable_plugins()
                    .into_iter()
                    .map(|entry| PickerRow {
                        vendor: entry.vendor,
                        name: entry.name,
                        reason: Some(entry.reason),
                        plugin: None,
                    }),
            );
        }
        rows.sort_by(|a, b| {
            (a.vendor.to_lowercase(), a.name.to_lowercase())
                .cmp(&(b.vendor.to_lowercase(), b.name.to_lowercase()))
        });
        rows
    }

    /// The centred plug-in picker: search, catalog grouped by vendor, and a keyboard cursor.
    #[allow(
        clippy::too_many_lines,
        reason = "search, keyboard cursor, and grouped list share one picker state"
    )]
    fn draw_picker(&mut self, ctx: &egui::Context) {
        let Overlay::Picker(picker) = &mut self.overlay else {
            return;
        };
        let rack_index = picker.rack;
        let Some(rack) = self.controller.document().model.racks.get(rack_index) else {
            self.overlay = Overlay::None;
            return;
        };
        let rack_name = rack.name.clone();
        let slot_number = rack.slots.len() + 1;
        let rows = self.picker_rows();
        let Overlay::Picker(picker) = &mut self.overlay else {
            return;
        };
        let query = picker.query.to_lowercase();
        let matches: Vec<&PickerRow> = rows
            .iter()
            .filter(|row| {
                query.is_empty()
                    || row.name.to_lowercase().contains(&query)
                    || row.vendor.to_lowercase().contains(&query)
            })
            .collect();
        let choosable: Vec<usize> = matches
            .iter()
            .enumerate()
            .filter(|(_, row)| row.plugin.is_some())
            .map(|(index, _)| index)
            .collect();
        let (up, down, enter) = ctx.input_mut(|input| {
            (
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            )
        });
        let position = choosable
            .iter()
            .position(|index| *index == picker.cursor)
            .unwrap_or(0);
        if down {
            picker.cursor = choosable
                .get((position + 1).min(choosable.len().saturating_sub(1)))
                .copied()
                .unwrap_or(0);
        } else if up {
            picker.cursor = choosable
                .get(position.saturating_sub(1))
                .copied()
                .unwrap_or(0);
        } else if !choosable.contains(&picker.cursor) {
            picker.cursor = choosable.first().copied().unwrap_or(0);
        }
        let mut chosen = enter
            .then(|| {
                matches
                    .get(picker.cursor)
                    .and_then(|row| row.plugin.clone())
            })
            .flatten();
        let center = ctx.screen_rect().center();
        let catalog_empty = rows.is_empty();
        popover(
            ctx,
            egui::Id::new("picker"),
            center,
            Align2::CENTER_CENTER,
            Some(px(Layout::PickerWidth)),
            |ui| {
                popover_title(
                    ui,
                    &format!("Add plug-in to {rack_name}"),
                    &format!("slot {slot_number} of {MAX_SLOTS_PER_RACK}"),
                );
                let search = text_field(
                    ui,
                    &mut picker.query,
                    "Search plug-ins or vendors",
                    ui.available_width(),
                );
                if !search.has_focus() {
                    search.request_focus();
                }
                if search.changed() {
                    picker.cursor = 0;
                }
                ui.add_space(sp(Spacing::S8));
                let list_height = px(Layout::ModalMaxHeight) - line() * 6.0;
                egui::ScrollArea::vertical()
                    .max_height(list_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let mut vendor: Option<&str> = None;
                        for (index, row) in matches.iter().enumerate() {
                            if vendor != Some(row.vendor.as_str()) {
                                vendor = Some(row.vendor.as_str());
                                ui.add_space(sp(Spacing::S8));
                                let heading = if row.vendor.is_empty() {
                                    "Unavailable"
                                } else {
                                    &row.vendor
                                };
                                text(ui, heading, TypographyRole::Body, ColorToken::Dim);
                            }
                            let current = index == picker.cursor;
                            let response = list_row(
                                ui,
                                &row.name,
                                row.reason.unwrap_or(""),
                                None,
                                current,
                                row.plugin.is_some(),
                            );
                            if current {
                                response.scroll_to_me(None);
                            }
                            if response.clicked() {
                                chosen.clone_from(&row.plugin);
                            }
                        }
                        if matches.is_empty() {
                            text(
                                ui,
                                if catalog_empty {
                                    "No plug-ins yet. Rescan from Setup, Plug-ins."
                                } else {
                                    "Nothing matches."
                                },
                                TypographyRole::Body,
                                ColorToken::Dim,
                            );
                        }
                    });
                ui.add_space(sp(Spacing::S12));
                ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                    text(
                        ui,
                        "↑↓ choose   ↩ add   Esc close",
                        TypographyRole::Body,
                        ColorToken::Dim,
                    );
                });
            },
        );
        if let Some(plugin) = chosen {
            self.overlay = Overlay::None;
            self.select_rack(rack_index);
            self.add_plugin(plugin);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "each modal's copy sits beside the shared layout it fills"
    )]
    fn draw_modal(&mut self, ctx: &egui::Context, modal: Modal) {
        let (heading, body, detail, cancel, confirm) = match modal {
            Modal::StopEngine => (
                "Stop the audio engine?".to_owned(),
                "All racks go silent until you start it again. Plug-in state stays loaded."
                    .to_owned(),
                None,
                "Cancel",
                "Stop engine",
            ),
            Modal::Quit => (
                "Quit while audio is running?".to_owned(),
                "Every rack goes silent. The session is saved first.".to_owned(),
                None,
                "Cancel",
                "Save and quit",
            ),
            Modal::RemoveRack(rack) => (
                format!(
                    "Remove {}?",
                    self.controller
                        .document()
                        .model
                        .racks
                        .get(rack)
                        .map_or("this rack", |rack| rack.name.as_str())
                ),
                "Its plug-ins and their settings are removed. Other racks keep playing.".to_owned(),
                None,
                "Cancel",
                "Remove",
            ),
            Modal::Recovery => {
                let offer = self.controller.recovery_offer();
                let recovered = offer
                    .map(|offer| offer.recovered_at_unix_ms)
                    .filter(|at| *at > 0)
                    .map(ago_label);
                let explicit = offer
                    .and_then(|offer| offer.explicit_saved_at_unix_ms)
                    .map(ago_label);
                (
                    "Recover the last session?".to_owned(),
                    format!(
                        "{} did not shut down cleanly. A recovery package{} is available.",
                        self.session_name(),
                        recovered.map_or_else(String::new, |time| format!(" from {time}"))
                    ),
                    Some(explicit.map_or_else(
                        || "Restore loads that package. Discard keeps the last explicit save.".to_owned(),
                        |time| {
                            format!(
                                "Restore loads that package. Discard keeps the last explicit save from {time}."
                            )
                        },
                    )),
                    "Discard",
                    "Restore",
                )
            }
        };
        let mut choice = None;
        let confirm_key =
            ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        popover(
            ctx,
            egui::Id::new("modal"),
            ctx.screen_rect().center(),
            Align2::CENTER_CENTER,
            Some(px(Layout::ConfirmWidth)),
            |ui| {
                title(ui, &heading);
                ui.add_space(sp(Spacing::S8));
                note(ui, &body, ColorToken::Text);
                if let Some(detail) = &detail {
                    ui.add_space(sp(Spacing::S4));
                    note(ui, detail, ColorToken::Dim);
                }
                ui.add_space(sp(Spacing::S12));
                ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = sp(Spacing::S20);
                    if action(ui, confirm, Some("↩"), true).clicked() || confirm_key {
                        choice = Some(true);
                    }
                    let cancel_key = (modal != Modal::Recovery).then_some("Esc");
                    if action(ui, cancel, cancel_key, true).clicked() {
                        choice = Some(false);
                    }
                });
            },
        );
        let Some(confirmed) = choice else {
            return;
        };
        self.overlay = Overlay::None;
        match (modal, confirmed) {
            (Modal::StopEngine, true) => self.toggle_engine(),
            (Modal::Quit, true) => {
                self.save_session();
                self.toggle_engine();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            (Modal::RemoveRack(rack), true) => {
                self.selected_rack = rack;
                self.remove_selected_rack();
            }
            (Modal::Recovery, true) => self.restore_recovery(),
            (Modal::Recovery, false) => self.discard_recovery(),
            (_, false) => {}
        }
    }
}

/// Draws a bordered menu at `at` and returns the command of a clicked row.
fn menu<A: Copy>(ctx: &egui::Context, id: &str, at: Pos2, items: &[MenuItem<A>]) -> Option<A> {
    let mut chosen = None;
    egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Foreground)
        .fixed_pos(at)
        .constrain(true)
        .fade_in(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(color(ColorToken::Base))
                .stroke(hairline(ColorToken::Dim))
                .inner_margin(egui::Margin::symmetric(
                    0,
                    i8::try_from(Spacing::S4.pixels()).unwrap_or(i8::MAX),
                ))
                .show(ui, |ui| {
                    let width = items
                        .iter()
                        .flatten()
                        .map(|(label, key, ..)| {
                            let measure = |text: &str| {
                                super::style::galley(
                                    ui,
                                    text,
                                    TypographyRole::Body,
                                    ColorToken::Text,
                                    f32::INFINITY,
                                )
                                .size()
                                .x
                            };
                            measure(label)
                                + measure(key)
                                + sp(Spacing::S12) * 2.0
                                + sp(Spacing::S20)
                        })
                        .fold(px(Layout::MenuMinWidth), f32::max);
                    ui.set_width(width);
                    for item in items {
                        match item {
                            None => {
                                ui.add_space(sp(Spacing::S4));
                                let (rect, _) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), px(Layout::Hairline)),
                                    Sense::hover(),
                                );
                                ui.painter()
                                    .rect_filled(rect, 0.0, color(ColorToken::Hairline));
                                ui.add_space(sp(Spacing::S4));
                            }
                            Some((label, key, enabled, command)) => {
                                let key = (!key.is_empty()).then_some(*key);
                                if list_row(ui, label, "", key, false, *enabled).clicked() {
                                    chosen = Some(*command);
                                }
                            }
                        }
                    }
                });
        });
    chosen
}

/// A jack grid of 16 mono channels or 8 stereo pairs (more rows for larger devices). Channels
/// beyond `available` are faint and inert. Returns the clicked selection.
fn jack_grid(
    ui: &mut Ui,
    id: &str,
    current: Option<PhysicalChannels>,
    available: u16,
    mono: bool,
) -> Option<PhysicalChannels> {
    const MIN_CHANNELS: u16 = 16;
    let channels = available.max(MIN_CHANNELS).div_ceil(MIN_CHANNELS) * MIN_CHANNELS;
    let step: u16 = if mono { 1 } else { 2 };
    let width = if mono {
        px(Layout::JackWidth)
    } else {
        px(Layout::JackWidth) * 2.0 + sp(Spacing::S4)
    };
    let per_row = usize::from(8 / step);
    let mut chosen = None;
    let jacks: Vec<u16> = (0..channels).step_by(usize::from(step)).collect();
    ui.push_id(id, |ui| {
        for row in jacks.chunks(per_row) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = sp(Spacing::S4);
                for &first in row {
                    let label = if mono {
                        format!("{}", first + 1)
                    } else {
                        format!("{}-{}", first + 1, first + 2)
                    };
                    let on = match current {
                        Some(PhysicalChannels::Mono { channel }) => {
                            mono && u16::from(channel) == first
                        }
                        Some(PhysicalChannels::Stereo { left, .. }) => {
                            !mono && u16::from(left) - u16::from(left) % 2 == first
                        }
                        None => false,
                    };
                    let fits = first + step <= available;
                    if jack(ui, &label, on, fits, width).clicked() {
                        let channel = |value: u16| u8::try_from(value).unwrap_or(0);
                        chosen = Some(if mono {
                            PhysicalChannels::Mono {
                                channel: channel(first),
                            }
                        } else {
                            PhysicalChannels::Stereo {
                                left: channel(first),
                                right: channel(first + 1),
                            }
                        });
                    }
                }
            });
            ui.add_space(sp(Spacing::S4));
        }
    });
    chosen
}

fn first_channel(channels: PhysicalChannels) -> u8 {
    match channels {
        PhysicalChannels::Mono { channel } => channel,
        PhysicalChannels::Stereo { left, .. } => left,
    }
}

/// `12 min ago` for a Unix time in milliseconds.
fn ago_label(unix_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let seconds = now_ms.saturating_sub(unix_ms) / 1000;
    if seconds < 60 {
        format!("{seconds} s ago")
    } else if seconds < 3600 {
        format!("{} min ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} h ago", seconds / 3600)
    } else {
        format!("{} days ago", seconds / 86_400)
    }
}
