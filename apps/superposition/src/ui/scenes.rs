//! Scene capture and edit: which plug-in parameters a scene recalls, separate from full
//! session state.

use std::collections::{BTreeMap, BTreeSet};

use eframe::egui::{self, Align, Align2, Layout as EguiLayout};
use sp_model::{
    MAX_SCENE_PARAMETER_VALUES, ParameterId, PluginParameterMetadata, PluginSlot, Scene,
    SceneParameterTransition, SceneParameterValue, Session,
};
use sp_ui::design::{ColorToken, Layout, Spacing, TypographyRole};

use super::LiveRackApp;
use super::style::{
    Mark, action, choice_row, line, note, popover, popover_title, px, sp, text, text_field,
};

/// Parameters a new capture selects per plug-in by default.
const DEFAULT_PARAMETERS_PER_SLOT: usize = 2;

struct ParameterChoice {
    target: SceneParameterValue,
    label: String,
    /// `Rack, Plug-in`.
    location: String,
    search_text: String,
}

/// One list row: a `Rack, Plug-in` heading or a parameter choice.
enum Row {
    Group(String),
    Choice(usize),
}

pub(super) struct SceneEditor {
    pub(super) scene_index: Option<usize>,
    pub(super) scene: Scene,
    choices: Vec<ParameterChoice>,
    selected: BTreeSet<usize>,
    filter: String,
    rows: Vec<Row>,
}

/// What the user did with the scene editor this frame.
enum SceneChoice {
    Commit,
    Cancel,
}

impl SceneEditor {
    /// Lists every automatable parameter in plug-in order, plus any the scene already recalls.
    /// Editing keeps the scene's selection; a new capture selects the first two parameters of
    /// every plug-in.
    pub(super) fn new<'a>(
        model: &Session,
        scene: Scene,
        scene_index: Option<usize>,
        metadata: impl Fn(&PluginSlot) -> &'a [PluginParameterMetadata],
    ) -> Self {
        let existing = scene
            .parameter_values
            .iter()
            .map(|parameter| {
                (
                    (
                        &parameter.rack_id,
                        &parameter.slot_id,
                        &parameter.parameter_id,
                    ),
                    parameter,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut choices = Vec::new();
        let mut selected = BTreeSet::new();
        for rack in &model.racks {
            for slot in &rack.slots {
                let location = format!("{}, {}", rack.name, slot.plugin.identity.name);
                let described = metadata(slot);
                let automatable = described
                    .iter()
                    .filter(|parameter| {
                        parameter.automatable && !parameter.read_only && !parameter.bypass
                    })
                    .map(|parameter| (ParameterId(parameter.id.to_string()), Some(parameter)));
                let prior = slot
                    .parameters
                    .values
                    .keys()
                    .filter(|id| {
                        existing.contains_key(&(&rack.id, &slot.id, *id))
                            && !described.iter().any(|parameter| {
                                parameter.automatable
                                    && !parameter.read_only
                                    && !parameter.bypass
                                    && parameter.id.to_string() == id.0
                            })
                    })
                    .map(|id| {
                        let parameter = described
                            .iter()
                            .find(|parameter| parameter.id.to_string() == id.0);
                        (id.clone(), parameter)
                    });
                let mut defaults = 0;
                for (id, parameter) in automatable.chain(prior.collect::<Vec<_>>()) {
                    let Some(value) = slot.parameters.values.get(&id) else {
                        continue;
                    };
                    let prior = existing.get(&(&rack.id, &slot.id, &id));
                    let index = choices.len();
                    if prior.is_some()
                        || (scene_index.is_none() && defaults < DEFAULT_PARAMETERS_PER_SLOT)
                    {
                        selected.insert(index);
                        defaults += 1;
                    }
                    let label = parameter.map_or_else(
                        || format!("Parameter {}", id.0),
                        |parameter| parameter.name.clone(),
                    );
                    choices.push(ParameterChoice {
                        search_text: format!("{location} {label} {}", id.0).to_lowercase(),
                        label,
                        location: location.clone(),
                        target: SceneParameterValue {
                            rack_id: rack.id.clone(),
                            slot_id: slot.id.clone(),
                            parameter_id: id.clone(),
                            value: *value,
                            transition: prior.map_or_else(
                                || {
                                    if parameter.is_some_and(|parameter| parameter.step_count > 0) {
                                        SceneParameterTransition::Step
                                    } else {
                                        SceneParameterTransition::Ramp
                                    }
                                },
                                |parameter| parameter.transition,
                            ),
                        },
                    });
                }
            }
        }
        selected = selected
            .into_iter()
            .take(MAX_SCENE_PARAMETER_VALUES)
            .collect();
        let mut editor = Self {
            scene_index,
            scene,
            choices,
            selected,
            filter: String::new(),
            rows: Vec::new(),
        };
        editor.filter_rows();
        editor
    }

    fn filter_rows(&mut self) {
        let query = self.filter.to_lowercase();
        self.rows.clear();
        let mut location = None;
        for (index, choice) in self.choices.iter().enumerate() {
            if !choice.search_text.contains(&query) {
                continue;
            }
            if location != Some(&choice.location) {
                location = Some(&choice.location);
                self.rows.push(Row::Group(choice.location.clone()));
            }
            self.rows.push(Row::Choice(index));
        }
    }

    pub(super) fn selected_scene(&self) -> Scene {
        Scene {
            parameter_values: self
                .selected
                .iter()
                .map(|&index| self.choices[index].target.clone())
                .collect(),
            ..self.scene.clone()
        }
    }

    /// Draws the centred modal and returns the user's decision, if any.
    #[allow(
        clippy::too_many_lines,
        reason = "the modal reads top to bottom: title, fields, grouped list, note, actions"
    )]
    fn show(&mut self, ctx: &egui::Context) -> Option<SceneChoice> {
        let mut choice = None;
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
            choice = Some(SceneChoice::Commit);
        }
        let heading = match self.scene_index {
            None => "Capture scene".to_owned(),
            Some(_) => format!("Edit {}", self.scene.name),
        };
        let count = format!(
            "{} of {MAX_SCENE_PARAMETER_VALUES} parameters",
            self.selected.len()
        );
        popover(
            ctx,
            egui::Id::new("scene_editor"),
            ctx.screen_rect().center(),
            Align2::CENTER_CENTER,
            Some(px(Layout::SceneModalWidth)),
            |ui| {
                popover_title(ui, &heading, &count);
                ui.horizontal(|ui| {
                    text(ui, "Name", TypographyRole::Body, ColorToken::Dim);
                    text_field(
                        ui,
                        &mut self.scene.name,
                        "Scene name",
                        px(Layout::NameFieldWidth),
                    );
                    ui.add_space(sp(Spacing::S12));
                    if text_field(ui, &mut self.filter, "Filter", px(Layout::FilterFieldWidth))
                        .changed()
                    {
                        self.filter_rows();
                    }
                });
                ui.add_space(sp(Spacing::S8));
                let list_height = px(Layout::ModalMaxHeight) - line() * 8.0;
                egui::ScrollArea::vertical()
                    .max_height(list_height)
                    .auto_shrink([false, true])
                    .show_rows(ui, line(), self.rows.len(), |ui, range| {
                        for row in range {
                            match &self.rows[row] {
                                Row::Group(location) => {
                                    text(ui, location, TypographyRole::Body, ColorToken::Dim);
                                }
                                Row::Choice(index) => {
                                    let index = *index;
                                    let parameter = &self.choices[index];
                                    let on = self.selected.contains(&index);
                                    let allowed =
                                        on || self.selected.len() < MAX_SCENE_PARAMETER_VALUES;
                                    let response = ui
                                        .push_id(index, |ui| {
                                            choice_row(
                                                ui,
                                                Mark::Check,
                                                on,
                                                &parameter.label,
                                                &format!("{:.2}", parameter.target.value.get()),
                                                allowed,
                                            )
                                        })
                                        .inner;
                                    if response.clicked() {
                                        if on {
                                            self.selected.remove(&index);
                                        } else {
                                            self.selected.insert(index);
                                        }
                                    }
                                }
                            }
                        }
                    });
                if self.choices.is_empty() {
                    text(
                        ui,
                        "No automatable plug-in parameters yet. Scan plug-ins to read them.",
                        TypographyRole::Body,
                        ColorToken::Dim,
                    );
                }
                ui.add_space(sp(Spacing::S8));
                note(
                    ui,
                    "Scenes recall only the parameters selected here, over the fade time. Opaque plug-in state is never part of a scene.",
                    ColorToken::Dim,
                );
                ui.add_space(sp(Spacing::S12));
                ui.with_layout(EguiLayout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = sp(Spacing::S20);
                    let confirm = if self.scene_index.is_some() {
                        "Save scene"
                    } else {
                        "Capture"
                    };
                    if action(ui, confirm, Some("↩"), !self.scene.name.trim().is_empty()).clicked()
                    {
                        choice = Some(SceneChoice::Commit);
                    }
                    if action(ui, "Cancel", Some("Esc"), true).clicked() {
                        choice = Some(SceneChoice::Cancel);
                    }
                });
            },
        );
        if matches!(choice, Some(SceneChoice::Commit)) && self.scene.name.trim().is_empty() {
            return None;
        }
        choice
    }
}

impl LiveRackApp {
    pub(super) fn draw_scene_editor(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.scene_editor.take() else {
            return;
        };
        super::style::scrim(ctx, egui::Id::new("scene_scrim"));
        match editor.show(ctx) {
            Some(SceneChoice::Commit) => {
                if !self.commit_scene(editor.selected_scene(), editor.scene_index) {
                    self.scene_editor = Some(editor);
                }
            }
            Some(SceneChoice::Cancel) => {}
            None => self.scene_editor = Some(editor),
        }
    }
}
