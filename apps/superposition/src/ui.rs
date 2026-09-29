//! The film-strip show screen: an egui renderer over the session, engine, and worker runtime.
//!
//! The screen is an instrument panel, not a mixer: a head of readout cells, one hairline column
//! per rack with its editor previews, gain, meters, and state, and a foot line of scenes. All
//! parameter editing happens in each plug-in's own editor window; there is no master bus.

mod overlays;
mod previews;
mod scenes;
mod setup;
mod show;
mod style;

pub use style::install_style;

#[cfg(target_os = "macos")]
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime};

#[cfg(target_os = "macos")]
use crate::RackPlan;
use crate::{CatalogPlugin, MirroredParameterUpdate, ProductRuntime, SlotEdit};
use eframe::egui;
use previews::PreviewCache;
use scenes::SceneEditor;
#[cfg(target_os = "macos")]
use sp_audio_io::{AudioEndpoint, AudioEndpointEvent, AudioFormat, AudioRouteConfig};
#[cfg(target_os = "macos")]
use sp_midi::{
    MidiCcMonitor, MidiLearnController, MidiMappingPublisher, MidiMappingTarget, MidiPortId,
    MidiPortInfo, MidirInput, map_controller_to_target,
};
use sp_model::{
    AudioDeviceSelection, AudioDeviceSettings, ChannelLayout, Endpoint, EndpointId, MAX_RACKS,
    MAX_SCENE_PARAMETER_VALUES, MAX_SLOTS_PER_RACK, MidiController, MidiMapping, MidiMappingId,
    NormalizedValue, ParameterAddress, ParameterId, PhysicalChannels, PluginInstanceId, PluginSlot,
    Rack, RackBypass, RackChannelRoute, RackGain, RackId, RackMute, RackTopology, Scene, SceneId,
    SceneParameterValue, SlotBypass, Source, SourceId,
};
use sp_session::CapturedPluginState;
use sp_session::SessionController;
use sp_ui::components::{StateToken, SystemStatus, SystemStatusState};
use sp_ui::design::MotionPreference;

#[cfg(target_os = "macos")]
use sp_audio_io_macos::{
    MacOsAudioDevice, MacOsAudioEndpoint, ProductControl, ProductTelemetry, enumerate_devices,
};
#[cfg(target_os = "macos")]
use sp_ui::components::MeterBallistics;

const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
/// Fade steps and range for scene transitions.
const FADE_STEP_MS: u32 = 50;
const FADE_MAX_MS: u32 = 10_000;
const DEFAULT_FADE_MS: u32 = 250;
/// Recent callback-load bars in the foot and how often one is added.
const LOAD_BARS: usize = 12;
const LOAD_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// ⌘0 shows all racks; ⌘1–⌘9 show pages 1–9.
const PAGE_KEYS: [egui::Key; 10] = [
    egui::Key::Num0,
    egui::Key::Num1,
    egui::Key::Num2,
    egui::Key::Num3,
    egui::Key::Num4,
    egui::Key::Num5,
    egui::Key::Num6,
    egui::Key::Num7,
    egui::Key::Num8,
    egui::Key::Num9,
];
/// More racks than fit across 1920 px bring up the page line even before any page exists.
const PAGE_LINE_RACKS: usize = 8;
/// Reduced motion holds meters still, refreshing them this rarely.
const REDUCED_MOTION_METER_INTERVAL: Duration = Duration::from_secs(1);

fn mirror_update_matches(model: &sp_model::Session, update: &MirroredParameterUpdate) -> bool {
    model
        .racks
        .get(update.rack_index)
        .filter(|rack| rack.id == update.rack_id)
        .and_then(|rack| rack.slots.get(update.slot_index))
        .is_some_and(|slot| {
            slot.id == update.slot_id
                && slot.plugin.fingerprint.digest == update.fingerprint
                && slot.plugin.identity.unique_id == update.class_id
        })
}

/// The column area shows racks or the setup page; head and foot stay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    Show,
    Setup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetupTab {
    Audio,
    Midi,
    Plugins,
    Diagnostics,
}

/// The one popover, menu, picker, or modal open above the columns.
#[derive(Debug, PartialEq)]
enum Overlay {
    None,
    RackMenu {
        rack: usize,
        at: egui::Pos2,
    },
    SlotMenu {
        rack: usize,
        slot: usize,
        at: egui::Pos2,
    },
    Route {
        rack: usize,
        at: egui::Pos2,
    },
    Sidechain {
        rack: usize,
        slot: usize,
        at: egui::Pos2,
    },
    /// Which pages one rack is on.
    RackPages {
        rack: usize,
        at: egui::Pos2,
    },
    PageMenu {
        page: usize,
        at: egui::Pos2,
    },
    PageRename {
        page: usize,
        at: egui::Pos2,
        draft: String,
    },
    Picker(Picker),
    Modal(Modal),
}

/// The plug-in picker for one rack: a search query and the keyboard cursor.
#[derive(Debug, PartialEq)]
struct Picker {
    rack: usize,
    query: String,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Modal {
    /// Stop the running engine.
    StopEngine,
    /// Quit while the engine runs: save, stop, then close.
    Quit,
    RemoveRack(usize),
    /// Offer the recovery package after an unclean shutdown.
    Recovery,
}

#[derive(Clone, Copy)]
enum SlotAction {
    Bypass(usize),
    Editor(usize),
    MoveUp(usize),
    MoveDown(usize),
    Remove(usize),
}

/// Root egui application state for the film-strip show screen.
pub struct LiveRackApp {
    controller: SessionController,
    product: Result<ProductRuntime, String>,
    catalog_plugins: Vec<CatalogPlugin>,
    selected_rack: usize,
    /// Keyboard slot selection within the selected rack.
    selected_slot: Option<usize>,
    system: SystemStatus,
    fault: Option<String>,
    status_line: String,
    /// When the last explicit save succeeded, for "Session saved, 4 s ago".
    saved_at: Option<Instant>,
    screen: Screen,
    setup_tab: SetupTab,
    overlay: Overlay,
    /// The page shown, or `None` for all racks.
    page: Option<usize>,
    /// Scroll the selected rack's column into view on the next frame.
    scroll_to_selected: bool,
    /// Rack being renamed inline, with its draft name.
    rename: Option<(usize, String)>,
    current_scene: Option<usize>,
    scene_editor: Option<SceneEditor>,
    /// Transition for new scenes, and the active scene's transition while one is active.
    fade_ms: u32,
    previews: PreviewCache,
    last_scan: Option<SystemTime>,
    /// Recent callback loads, oldest first, as fractions of the block period.
    load_history: [f32; LOAD_BARS],
    load_sampled_at: Instant,
    motion: MotionPreference,
    next_autosave_at: Instant,
    last_product_diagnostic: Option<String>,
    last_rack_latency: [u32; MAX_RACKS],
    /// After a device loss, the next time to look for the saved route and restart audio.
    #[cfg(target_os = "macos")]
    reconnect_at: Option<Instant>,
    #[cfg(target_os = "macos")]
    meter_clock: Instant,
    #[cfg(target_os = "macos")]
    meters_updated_at: Option<Instant>,
    #[cfg(target_os = "macos")]
    rack_input_meters: [MeterBallistics; MAX_RACKS],
    #[cfg(target_os = "macos")]
    rack_output_meters: [MeterBallistics; MAX_RACKS],
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
    pending_parameter_observations: BTreeMap<(usize, usize, u32), f32>,
    #[cfg(target_os = "macos")]
    midi_mapping_publisher: Option<MidiMappingPublisher>,
    #[cfg(target_os = "macos")]
    midi_learn: MidiLearnController,
    #[cfg(target_os = "macos")]
    midi_cc_monitor: Option<MidiCcMonitor>,
    /// MIDI Learn waits for a touched editor control, then for one CC.
    #[cfg(target_os = "macos")]
    midi_learn_armed: bool,
    #[cfg(target_os = "macos")]
    midi_learn_target: Option<MidiMappingTarget>,
    #[cfg(target_os = "macos")]
    telemetry: Option<std::sync::Arc<ProductTelemetry>>,
    /// Snapshot fixtures draw racks as if their workers were loaded.
    #[cfg(test)]
    test_workers_running: bool,
}

impl LiveRackApp {
    /// Creates the show screen over a session and the worker runtime.
    #[allow(
        clippy::too_many_lines,
        reason = "initializes live handles and presentation state together"
    )]
    pub fn new(
        controller: SessionController,
        recovery_offered: bool,
        product: Result<ProductRuntime, String>,
    ) -> Self {
        #[cfg(target_os = "macos")]
        let audio = MacOsAudioEndpoint::new().allow_device_reconfiguration();
        #[cfg(target_os = "macos")]
        let audio_devices = route_devices().unwrap_or_default();
        #[cfg(target_os = "macos")]
        let buffer_frames = controller
            .document()
            .model
            .audio_settings
            .as_ref()
            .map_or(128, |settings| settings.buffer_frames);
        #[cfg(target_os = "macos")]
        let selected_route = controller
            .document()
            .model
            .audio_settings
            .as_ref()
            .and_then(|settings| saved_route(&audio_devices, settings));
        #[cfg(target_os = "macos")]
        let midi_ports = MidirInput::enumerate_ports().unwrap_or_default();
        #[cfg(target_os = "macos")]
        let selected_midi = midi_ports.first().map(|port| port.id.clone());
        let catalog_plugins = product
            .as_ref()
            .map_or_else(|_| Vec::new(), ProductRuntime::catalog_plugins);
        let status_line = if controller.document().model.racks.is_empty() {
            "No session yet".to_owned()
        } else {
            "Session loaded".to_owned()
        };
        Self {
            controller,
            product,
            catalog_plugins,
            selected_rack: 0,
            selected_slot: None,
            system: SystemStatus::new(SystemStatusState::Offline),
            fault: None,
            status_line,
            saved_at: None,
            screen: Screen::Show,
            setup_tab: SetupTab::Audio,
            overlay: if recovery_offered {
                Overlay::Modal(Modal::Recovery)
            } else {
                Overlay::None
            },
            page: None,
            scroll_to_selected: false,
            rename: None,
            current_scene: None,
            scene_editor: None,
            fade_ms: DEFAULT_FADE_MS,
            previews: PreviewCache::default(),
            last_scan: None,
            load_history: [0.0; LOAD_BARS],
            load_sampled_at: Instant::now(),
            motion: reduced_motion_preference(),
            next_autosave_at: Instant::now() + AUTOSAVE_INTERVAL,
            last_product_diagnostic: None,
            last_rack_latency: [u32::MAX; MAX_RACKS],
            #[cfg(target_os = "macos")]
            reconnect_at: None,
            #[cfg(target_os = "macos")]
            meter_clock: Instant::now(),
            #[cfg(target_os = "macos")]
            meters_updated_at: None,
            #[cfg(target_os = "macos")]
            rack_input_meters: [MeterBallistics::default(); MAX_RACKS],
            #[cfg(target_os = "macos")]
            rack_output_meters: [MeterBallistics::default(); MAX_RACKS],
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
            pending_parameter_observations: BTreeMap::new(),
            #[cfg(target_os = "macos")]
            midi_mapping_publisher: None,
            #[cfg(target_os = "macos")]
            midi_learn: MidiLearnController::new(),
            #[cfg(target_os = "macos")]
            midi_cc_monitor: None,
            #[cfg(target_os = "macos")]
            midi_learn_armed: false,
            #[cfg(target_os = "macos")]
            midi_learn_target: None,
            #[cfg(target_os = "macos")]
            telemetry: None,
            #[cfg(test)]
            test_workers_running: false,
        }
    }

    fn set_status(&mut self, message: &str) {
        message.clone_into(&mut self.status_line);
        self.saved_at = None;
    }

    fn show_fault(&mut self, message: String) {
        self.fault = Some(message);
    }

    fn online(&self) -> bool {
        self.system.state == SystemStatusState::Online
    }

    #[cfg(target_os = "macos")]
    fn persist_audio_settings(&mut self) {
        let Some(route) = &self.selected_route else {
            return;
        };
        let Some(output) = self
            .audio_devices
            .iter()
            .find(|device| device.info.id == route.output)
        else {
            return;
        };
        let input = route
            .input
            .as_ref()
            .and_then(|id| {
                self.audio_devices
                    .iter()
                    .find(|device| &device.info.id == id)
            })
            .map(|device| AudioDeviceSelection {
                id: device.info.id.as_str().to_owned(),
                name: device.info.name.clone(),
            });
        self.controller.document_mut().model.audio_settings = Some(AudioDeviceSettings {
            input,
            output: AudioDeviceSelection {
                id: output.info.id.as_str().to_owned(),
                name: output.info.name.clone(),
            },
            buffer_frames: self.buffer_frames,
        });
    }

    fn sync_worker_parameters(&mut self) -> Result<(), String> {
        let Ok(product) = self.product.as_mut() else {
            return Ok(());
        };
        let mut model = self.controller.document().model.clone();
        product.sync_worker_parameters(&mut model)?;
        if model != self.controller.document().model {
            self.controller.document_mut().model = model;
        }
        Ok(())
    }

    /// Applies parameter changes made in native editors to the session. While MIDI Learn is
    /// armed, the last touched parameter becomes its target.
    fn poll_parameter_mirror(&mut self) {
        let updates = self
            .product
            .as_mut()
            .map_or_else(|_| Vec::new(), ProductRuntime::take_parameter_updates);
        if updates.is_empty() {
            #[cfg(target_os = "macos")]
            self.flush_parameter_observations();
            return;
        }
        let model = &self.controller.document().model;
        #[cfg(target_os = "macos")]
        let scene_targets: BTreeSet<(usize, usize, u32)> = if self.product_control.is_some() {
            model
                .scenes
                .iter()
                .flat_map(|scene| &scene.parameter_values)
                .filter_map(|target| {
                    let rack_index = model
                        .racks
                        .iter()
                        .position(|rack| rack.id == target.rack_id)?;
                    let slot_index = model.racks[rack_index]
                        .slots
                        .iter()
                        .position(|slot| slot.id == target.slot_id)?;
                    let parameter_id = target.parameter_id.0.parse().ok()?;
                    Some((rack_index, slot_index, parameter_id))
                })
                .collect()
        } else {
            BTreeSet::new()
        };
        let mut changed = Vec::new();
        for update in updates {
            if !mirror_update_matches(model, &update) {
                continue;
            }
            #[cfg(target_os = "macos")]
            {
                if scene_targets.contains(&(
                    update.rack_index,
                    update.slot_index,
                    update.parameter_id,
                )) {
                    self.pending_parameter_observations.insert(
                        (update.rack_index, update.slot_index, update.parameter_id),
                        update.value.get(),
                    );
                }
                if self.midi_learn_armed {
                    let target = MidiMappingTarget {
                        rack_index: update.rack_index,
                        slot_index: update.slot_index,
                        parameter_id: update.parameter_id,
                        minimum: 0.0,
                        maximum: 1.0,
                    };
                    self.midi_learn.arm(target);
                    self.midi_learn_target = Some(target);
                }
            }
            let slot = &model.racks[update.rack_index].slots[update.slot_index];
            if slot
                .parameters
                .values
                .get(&ParameterId(update.parameter_id.to_string()))
                != Some(&update.value)
            {
                changed.push(update);
            }
        }
        if !changed.is_empty() {
            let model = &mut self.controller.document_mut().model;
            for update in changed {
                model.racks[update.rack_index].slots[update.slot_index]
                    .parameters
                    .values
                    .insert(ParameterId(update.parameter_id.to_string()), update.value);
            }
        }
        #[cfg(target_os = "macos")]
        self.flush_parameter_observations();
    }

    #[cfg(target_os = "macos")]
    fn flush_parameter_observations(&mut self) {
        let model = &self.controller.document().model;
        let Some(control) = self.product_control.as_mut() else {
            return;
        };
        let mut sent = Vec::new();
        for (&(rack, slot, parameter_id), &value) in
            self.pending_parameter_observations.iter().take(64)
        {
            let current = model
                .racks
                .get(rack)
                .and_then(|rack| rack.slots.get(slot))
                .and_then(|slot| {
                    slot.parameters
                        .values
                        .get(&ParameterId(parameter_id.to_string()))
                })
                .map(|current| current.get().to_bits());
            if current != Some(value.to_bits()) {
                sent.push((rack, slot, parameter_id));
                continue;
            }
            if !control.observe_parameter(rack, slot, parameter_id, value) {
                break;
            }
            sent.push((rack, slot, parameter_id));
        }
        for key in sent {
            self.pending_parameter_observations.remove(&key);
        }
    }

    fn saved_states(&self, rack: &Rack) -> Result<Vec<Option<CapturedPluginState>>, String> {
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
            .map_err(|error| format!("Saved plug-in state is unreadable: {error}"))
    }

    #[cfg(target_os = "macos")]
    fn load_unloaded_racks(&mut self) -> Result<(), String> {
        let racks = self.controller.document().model.racks.clone();
        for (rack_index, rack) in racks.iter().enumerate() {
            if rack.slots.is_empty()
                || self
                    .product
                    .as_ref()
                    .is_ok_and(|product| product.worker_running(rack_index))
            {
                continue;
            }
            let saved_states = self.saved_states(rack)?;
            self.product_mut()?
                .load_rack(rack_index, rack, &saved_states)
                .map_err(|error| format!("Rack {} could not load: {error}", rack_index + 1))?;
        }
        Ok(())
    }

    fn autosave_session(&mut self) {
        self.next_autosave_at = Instant::now() + AUTOSAVE_INTERVAL;
        if let Err(error) = self.controller.autosave() {
            self.show_fault(format!("Autosave failed: {error}"));
        }
    }

    fn autosave_if_due(&mut self, now: Instant) {
        if now < self.next_autosave_at {
            return;
        }
        self.next_autosave_at = now + AUTOSAVE_INTERVAL;
        if self.controller.is_dirty() {
            self.autosave_session();
        }
    }

    /// Saves the session, every plug-in's opaque state, and the newest editor pictures while
    /// audio keeps running.
    fn save_session(&mut self) {
        let result = (|| {
            self.sync_worker_parameters()?;
            let model = self.controller.document().model.clone();
            let captures = match self.product.as_mut() {
                Ok(product) => product.capture_plugin_states(&model)?,
                Err(_) => Vec::new(),
            };
            let previews = self.previews.files(&model);
            self.controller
                .save_with_plugin_states_and_previews(&captures, &previews)
                .map_err(|error| error.to_string())
        })();
        match result {
            Ok(()) => {
                self.next_autosave_at = Instant::now() + AUTOSAVE_INTERVAL;
                self.set_status("Session saved");
                self.saved_at = Some(Instant::now());
            }
            Err(error) => self.show_fault(format!("Save failed: {error}")),
        }
    }

    /// Keeps the recovered document and clears the offer.
    fn restore_recovery(&mut self) {
        self.controller.acknowledge_recovery_offer();
        self.set_status("Recovered session loaded");
    }

    /// Returns to the last explicit save.
    fn discard_recovery(&mut self) {
        match self.controller.discard_recovery() {
            Ok(()) => {
                self.previews = PreviewCache::default();
                self.clamp_selection();
                self.set_status("Recovery discarded. The last explicit save is loaded.");
            }
            Err(error) => self.show_fault(format!("Could not discard recovery: {error}")),
        }
    }

    fn clamp_selection(&mut self) {
        let pages = self.controller.document().model.pages.len();
        self.page = self.page.filter(|page| *page < pages);
        let racks = &self.controller.document().model.racks;
        self.selected_rack = self.selected_rack.min(racks.len().saturating_sub(1));
        let slots = racks
            .get(self.selected_rack)
            .map_or(0, |rack| rack.slots.len());
        self.selected_slot = self.selected_slot.filter(|slot| *slot < slots);
    }

    fn select_rack(&mut self, rack: usize) {
        if self.selected_rack != rack {
            self.selected_rack = rack;
            self.selected_slot = None;
        }
    }

    /// Rack indices the show screen lists: every rack, or the current page's racks, in session
    /// rack order.
    fn visible_racks(&self) -> Vec<usize> {
        let model = &self.controller.document().model;
        let page = self.page.and_then(|page| model.pages.get(page));
        model
            .racks
            .iter()
            .enumerate()
            .filter(|(_, rack)| page.is_none_or(|page| page.racks.contains(&rack.id)))
            .map(|(index, _)| index)
            .collect()
    }

    /// Shows a page, or all racks, keeping the selected rack when it is visible there.
    fn show_page(&mut self, page: Option<usize>) {
        self.page = page;
        let visible = self.visible_racks();
        if !visible.contains(&self.selected_rack)
            && let Some(&first) = visible.first()
        {
            self.select_rack(first);
        }
        self.scroll_to_selected = true;
    }

    /// Adds a page named `Page n`, holding `rack` if given, and shows it.
    fn new_page(&mut self, rack: Option<usize>) {
        let model = &self.controller.document().model;
        if model.pages.len() >= sp_model::MAX_PAGES {
            self.set_status("Page limit reached (16)");
            return;
        }
        let number = (1..=sp_model::MAX_PAGES)
            .find(|number| {
                !model
                    .pages
                    .iter()
                    .any(|page| page.id.0 == format!("page-{number}"))
            })
            .expect("finite page list");
        let racks = rack
            .and_then(|rack| model.racks.get(rack))
            .map(|rack| vec![rack.id.clone()])
            .unwrap_or_default();
        let pages = &mut self.controller.document_mut().model.pages;
        pages.push(sp_model::RackPage {
            id: sp_model::PageId(format!("page-{number}")),
            name: format!("Page {}", pages.len() + 1),
            racks,
        });
        let index = pages.len() - 1;
        self.show_page(Some(index));
        self.set_status("Page added. Choose its racks from each rack's menu, Pages.");
    }

    /// Adds `rack` to `page`, or takes it off. Audio and routing do not change.
    fn toggle_rack_page(&mut self, rack: usize, page: usize) {
        let model = &mut self.controller.document_mut().model;
        let Some(rack_id) = model.racks.get(rack).map(|rack| rack.id.clone()) else {
            return;
        };
        let Some(page) = model.pages.get_mut(page) else {
            return;
        };
        if let Some(position) = page.racks.iter().position(|id| *id == rack_id) {
            page.racks.remove(position);
        } else {
            page.racks.push(rack_id);
        }
    }

    fn remove_page(&mut self, page: usize) {
        let pages = &mut self.controller.document_mut().model.pages;
        if page >= pages.len() {
            return;
        }
        pages.remove(page);
        self.page = match self.page {
            Some(shown) if shown == page => None,
            Some(shown) if shown > page => Some(shown - 1),
            shown => shown,
        };
        self.set_status("Page removed. Its racks stay in the session.");
    }

    /// Whether the catalog on this machine has the plug-in a slot names.
    fn plugin_installed(&self, slot: &PluginSlot) -> bool {
        self.catalog_plugins
            .iter()
            .any(|plugin| plugin.descriptor.identity.unique_id == slot.plugin.identity.unique_id)
    }

    fn worker_running(&self, rack: usize) -> bool {
        #[cfg(test)]
        if self.test_workers_running {
            return true;
        }
        self.product
            .as_ref()
            .is_ok_and(|product| product.worker_running(rack))
    }

    fn slot_running(&self, rack: usize, slot: usize) -> bool {
        #[cfg(test)]
        if self.test_workers_running {
            return true;
        }
        self.product
            .as_ref()
            .is_ok_and(|product| product.slot_running(rack, slot))
    }

    /// The state token shown for one plug-in slot.
    fn slot_token(&self, rack: usize, slot: usize) -> StateToken {
        let Some(model_slot) = self
            .controller
            .document()
            .model
            .racks
            .get(rack)
            .and_then(|rack| rack.slots.get(slot))
        else {
            return StateToken::Unloaded;
        };
        let product = self.product.as_ref().ok();
        let running = self.worker_running(rack);
        if !self.plugin_installed(model_slot) || (running && !self.slot_running(rack, slot)) {
            StateToken::Missing
        } else if product.is_some_and(|product| product.worker_recovery_failed(rack)) {
            StateToken::Faulted
        } else if product.is_some_and(|product| product.worker_recovering(rack)) {
            StateToken::Loading
        } else if !self.online() || !running {
            StateToken::Unloaded
        } else if model_slot.bypassed {
            StateToken::Bypassed
        } else {
            StateToken::Ready
        }
    }

    /// The state token shown under a rack column.
    fn rack_token(&self, rack: usize) -> StateToken {
        let Some(model_rack) = self.controller.document().model.racks.get(rack) else {
            return StateToken::Unloaded;
        };
        let product = self.product.as_ref().ok();
        if product.is_some_and(|product| product.worker_recovery_failed(rack)) {
            StateToken::Faulted
        } else if product.is_some_and(|product| product.worker_recovering(rack)) {
            StateToken::Recovering
        } else if !self.online() || (!model_rack.slots.is_empty() && !self.worker_running(rack)) {
            StateToken::Unloaded
        } else if model_rack.bypassed {
            StateToken::Bypassed
        } else if (0..model_rack.slots.len())
            .any(|slot| self.slot_token(rack, slot) == StateToken::Missing)
        {
            StateToken::Missing
        } else {
            StateToken::Ready
        }
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
                    self.set_status("Audio engine stopped");
                }
                Err(error) => {
                    self.show_fault(format!("Audio engine did not stop cleanly: {error}"));
                }
            }
            return;
        }

        if let Err(error) = self.validate_selected_audio_route() {
            self.show_fault(error);
            return;
        }

        if let Err(error) = self
            .sync_worker_parameters()
            .and_then(|()| self.load_unloaded_racks())
        {
            self.show_fault(error);
            return;
        }

        let prepared = self
            .product
            .as_mut()
            .map_err(|error| error.clone())
            .and_then(|product| {
                crate::engine::prepare_audio(product, &self.controller.document().model)
            });
        let crate::engine::PreparedAudio {
            mut renderer,
            control: product_control,
            mapping_publisher,
            mappings,
        } = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.show_fault(error);
                return;
            }
        };
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
            self.show_fault("Choose an audio output device before starting".to_owned());
            return;
        };
        route.format = AudioFormat::product_stereo(self.buffer_frames)
            .expect("fixed product buffer choice is valid");
        let Some(output_device) = self
            .audio_devices
            .iter()
            .find(|device| device.info.id == route.output)
        else {
            self.system.set_state(SystemStatusState::Offline);
            self.show_fault("Selected audio output is unavailable. Refresh devices.".to_owned());
            return;
        };
        route.format.channel_count = output_device.capabilities.max_output_channels.min(64);
        if !available_buffer_frames(&self.audio_devices, &route).contains(&self.buffer_frames) {
            self.system.set_state(SystemStatusState::Offline);
            self.show_fault("Selected input and output do not share this buffer size".to_owned());
            return;
        }
        match self.audio.start_route(route.clone()) {
            Ok(()) => {
                self.meter_clock = Instant::now();
                self.rack_input_meters = [MeterBallistics::default(); MAX_RACKS];
                self.rack_output_meters = [MeterBallistics::default(); MAX_RACKS];
                self.product_control = Some(product_control);
                self.midi_mapping_publisher = Some(mapping_publisher);
                self.midi_learn = MidiLearnController::from_table(mappings);
                self.midi_cc_monitor = midi_cc_monitor;
                self.telemetry = Some(telemetry);
                self.system.set_state(SystemStatusState::Online);
                let racks = self.controller.document().model.racks.len();
                self.set_status(&format!(
                    "Audio engine online, {racks} {} loaded",
                    if racks == 1 { "rack" } else { "racks" }
                ));
            }
            Err(error) => {
                self.system.set_state(SystemStatusState::Offline);
                self.show_fault(format!("CoreAudio could not start: {error}"));
            }
        }
    }

    /// Channel count of the selected input or output device, capped at 64.
    #[cfg(target_os = "macos")]
    fn device_channels(&self, input: bool) -> u16 {
        self.selected_route
            .as_ref()
            .and_then(|route| {
                if input {
                    route.input.as_ref()
                } else {
                    Some(&route.output)
                }
            })
            .and_then(|id| {
                self.audio_devices
                    .iter()
                    .find(|device| &device.info.id == id)
            })
            .map_or(0, |device| {
                if input {
                    device.capabilities.max_input_channels.min(64)
                } else {
                    device.capabilities.max_output_channels.min(64)
                }
            })
    }

    #[cfg(not(target_os = "macos"))]
    #[allow(clippy::unused_self, reason = "matches the macOS signature")]
    fn device_channels(&self, _input: bool) -> u16 {
        0
    }

    /// Whether a rack's route only uses channels the selected devices have.
    fn route_fits(&self, route: RackChannelRoute) -> bool {
        physical_channels_fit(route.output, self.device_channels(false))
            && route
                .input
                .is_none_or(|input| physical_channels_fit(input, self.device_channels(true)))
    }

    #[cfg(target_os = "macos")]
    fn validate_selected_audio_route(&self) -> Result<(), String> {
        let route = self
            .selected_route
            .as_ref()
            .ok_or("Choose an audio output device before starting")?;
        if self.device_channels(false) == 0 {
            return Err("Selected audio output is unavailable. Refresh devices.".to_owned());
        }
        if route.input.is_some() && self.device_channels(true) == 0 {
            return Err("Selected audio input is unavailable. Refresh devices.".to_owned());
        }
        if !available_buffer_frames(&self.audio_devices, route).contains(&self.buffer_frames) {
            return Err("Selected input and output do not share this buffer size".to_owned());
        }
        let session = &self.controller.document().model;
        for rack in &session.racks {
            if !self.route_fits(rack_route(session, rack)) {
                return Err(format!(
                    "{} uses channels the selected devices do not have. Change its route.",
                    rack.name
                ));
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn refresh_audio_devices(&mut self) {
        match route_devices() {
            Ok(devices) => {
                self.audio_devices = devices;
                if let Some(route) = &mut self.selected_route {
                    if self
                        .audio_devices
                        .iter()
                        .any(|device| device.info.id == route.output)
                    {
                        if route.input.as_ref().is_some_and(|input| {
                            !self
                                .audio_devices
                                .iter()
                                .any(|device| &device.info.id == input)
                        }) {
                            route.input = None;
                        }
                        let buffers = available_buffer_frames(&self.audio_devices, route);
                        if !buffers.contains(&self.buffer_frames) {
                            self.buffer_frames = buffers.first().copied().unwrap_or(128);
                        }
                    } else {
                        self.selected_route = None;
                    }
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
                if self.selected_midi.as_ref().is_some_and(|selected| {
                    !self.midi_ports.iter().any(|port| &port.id == selected)
                }) {
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
        self.reconnect_at = Some(Instant::now() + RECONNECT_INTERVAL);
        self.show_fault(format!(
            "Audio device {device} changed or disconnected. The engine reconnects when it returns."
        ));
    }

    /// Restarts audio on the saved route once its devices are present again. A manual start or
    /// stop cancels the attempt.
    #[cfg(target_os = "macos")]
    fn poll_reconnect(&mut self, now: Instant) {
        let Some(due) = self.reconnect_at else {
            return;
        };
        if self.system.state != SystemStatusState::Offline {
            self.reconnect_at = None;
            return;
        }
        if now < due {
            return;
        }
        self.reconnect_at = Some(now + RECONNECT_INTERVAL);
        let Ok(devices) = route_devices() else {
            return;
        };
        let Some(route) = self
            .controller
            .document()
            .model
            .audio_settings
            .as_ref()
            .and_then(|settings| saved_route(&devices, settings))
        else {
            return;
        };
        self.audio_devices = devices;
        self.selected_route = Some(route);
        if self.validate_selected_audio_route().is_err() {
            return;
        }
        self.toggle_engine();
        if self.online() {
            self.reconnect_at = None;
            self.fault = None;
            self.set_status("Audio device returned. The engine reconnected.");
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn toggle_engine(&mut self) {
        self.show_fault("The live audio engine requires Apple Silicon macOS".to_owned());
    }

    /// Starts the engine, or asks before stopping a running one.
    fn request_engine_toggle(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.reconnect_at = None;
        }
        if self.online() {
            self.overlay = Overlay::Modal(Modal::StopEngine);
        } else {
            self.toggle_engine();
        }
    }

    #[cfg(target_os = "macos")]
    fn clear_live_handles(&mut self) {
        if let Ok(product) = self.product.as_mut() {
            product.stop_retiring_workers();
        }
        self.system.set_state(SystemStatusState::Offline);
        self.product_control = None;
        self.pending_parameter_observations.clear();
        self.midi_mapping_publisher = None;
        self.midi_cc_monitor = None;
        self.telemetry = None;
        self.rack_input_meters = [MeterBallistics::default(); MAX_RACKS];
        self.rack_output_meters = [MeterBallistics::default(); MAX_RACKS];
        self.load_history = [0.0; LOAD_BARS];
        self.cancel_midi_learn();
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
        let position = session.racks.len();
        let route = new_rack_route(
            position,
            self.device_channels(true),
            self.device_channels(false),
        );
        let before = self.rack_ids();
        let source_id = SourceId(format!("input-{index}"));
        let endpoint_id = EndpointId(format!("output-{index}"));
        let rack_id = RackId(format!("rack-{index}"));
        let document = self.controller.document_mut();
        document.model.sources.push(Source {
            id: source_id.clone(),
            name: format!("Input {index}"),
            layout: ChannelLayout::Mono,
        });
        document.model.endpoints.push(Endpoint {
            id: endpoint_id.clone(),
            name: format!("Output {index}"),
            layout: ChannelLayout::Stereo,
        });
        document.model.rack_routes.insert(rack_id.clone(), route);
        document.model.racks.push(Rack {
            id: rack_id,
            name: format!("Rack {index}"),
            source_id,
            endpoint_id,
            topology: RackTopology::Serial,
            gain_db: sp_model::GainDb::default(),
            muted: false,
            bypassed: false,
            slots: Vec::new(),
        });
        if let Some(page) = self
            .page
            .and_then(|page| self.controller.document_mut().model.pages.get_mut(page))
        {
            page.racks.push(RackId(format!("rack-{index}")));
        }
        self.select_rack(position);
        self.scroll_to_selected = true;
        self.apply_rack_edit(&before, &[]);
        self.set_status("Rack added. Choose a plug-in after scanning.");
    }

    /// Moves the selected rack past its visible neighbour, so it moves on screen even when
    /// the current page hides the racks between them.
    fn move_selected_rack(&mut self, offset: isize) {
        let visible = self.visible_racks();
        let Some(neighbour) = visible
            .iter()
            .position(|rack| *rack == self.selected_rack)
            .and_then(|position| position.checked_add_signed(offset))
            .and_then(|position| visible.get(position).copied())
        else {
            return;
        };
        let before = self.rack_ids();
        // Taking the rack out shifts a right-hand neighbour down one, so inserting at the
        // neighbour's old index lands on its far side in both directions.
        let racks = &mut self.controller.document_mut().model.racks;
        let moved = racks.remove(self.selected_rack);
        racks.insert(neighbour, moved);
        self.selected_rack = neighbour;
        self.scroll_to_selected = true;
        self.apply_rack_edit(&before, &[]);
        self.set_status("Rack order updated");
    }

    fn remove_selected_rack(&mut self) {
        let model = &mut self.controller.document_mut().model;
        if self.selected_rack >= model.racks.len() {
            return;
        }
        let before: Vec<RackId> = model.racks.iter().map(|rack| rack.id.clone()).collect();
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
            scene.rack_bypasses.retain(|value| value.rack_id != rack.id);
            scene.bypasses.retain(|value| value.rack_id != rack.id);
            scene
                .parameter_values
                .retain(|value| value.rack_id != rack.id);
        });
        model
            .midi_mappings
            .retain(|mapping| mapping.target.rack_id != rack.id);
        model.rack_routes.remove(&rack.id);
        for page in &mut model.pages {
            page.racks.retain(|id| *id != rack.id);
        }
        // Slots keyed by the removed rack lose their sidechain; their racks reload.
        let removed_source = Some(sp_model::SlotSidechain::RackOutput(rack.id.clone()));
        let mut rebuilt = Vec::new();
        for other in &mut model.racks {
            for slot in &mut other.slots {
                if slot.sidechain == removed_source {
                    slot.sidechain = None;
                    rebuilt.push(other.id.clone());
                }
            }
        }
        rebuilt.dedup();
        self.selected_rack = self.selected_rack.min(model.racks.len().saturating_sub(1));
        self.selected_slot = None;
        self.apply_rack_edit(&before, &rebuilt);
        self.set_status("Rack removed. Other racks kept playing.");
    }

    /// Applies a rack layout edit already made to the model. `before` lists rack IDs in their
    /// previous order and `rebuilt` names racks whose plug-in chain changed.
    ///
    /// While audio runs, untouched racks keep playing and only `rebuilt` or new racks restart;
    /// stopped audio uses the ordinary rebuild.
    fn apply_rack_edit(&mut self, before: &[RackId], rebuilt: &[RackId]) {
        #[cfg(target_os = "macos")]
        if self.product_control.is_some() {
            if let Err(error) = self.apply_rack_edit_live(before, rebuilt) {
                self.show_fault(error);
            }
            return;
        }
        let _ = (before, rebuilt);
        self.with_audio_paused(Self::rebuild_all_workers_stopped);
    }

    #[cfg(target_os = "macos")]
    fn apply_rack_edit_live(
        &mut self,
        before: &[RackId],
        rebuilt: &[RackId],
    ) -> Result<(), String> {
        self.sync_worker_parameters()?;
        let model = self.controller.document().model.clone();
        let plan: Vec<RackPlan> = model
            .racks
            .iter()
            .map(|rack| {
                let from = before.iter().position(|id| *id == rack.id);
                match from {
                    Some(from) if !rebuilt.contains(&rack.id) => RackPlan::Keep(from),
                    from => RackPlan::Rebuild(from),
                }
            })
            .collect();
        let mut saved_states = Vec::with_capacity(model.racks.len());
        for (rack, step) in model.racks.iter().zip(&plan) {
            let mut states = self.saved_states(rack)?;
            // A rebuilt rack keeps each surviving plug-in's live state.
            if let RackPlan::Rebuild(Some(from)) = step {
                let live = self.product_mut()?.capture_rack_states(*from)?;
                for (slot, state) in rack.slots.iter().zip(&mut states) {
                    if let Some(captured) = live.iter().find(|live| {
                        live.instance_id == slot.id.0
                            && live.metadata.fingerprint == slot.plugin.fingerprint.digest
                    }) {
                        *state = Some(captured.clone());
                    }
                }
            }
            saved_states.push(states);
        }
        let control = self
            .product_control
            .as_mut()
            .ok_or("Live control is unavailable")?;
        let product = self.product.as_mut().map_err(|error| error.clone())?;
        product.publish_live_topology(control, &model, &plan, &saved_states)?;
        self.midi_learn =
            MidiLearnController::from_table(sp_audio_io_macos::product_midi_mappings(&model));
        self.last_rack_latency = [u32::MAX; MAX_RACKS];
        self.rack_input_meters = [MeterBallistics::default(); MAX_RACKS];
        self.rack_output_meters = [MeterBallistics::default(); MAX_RACKS];
        self.clamp_selection();
        Ok(())
    }

    /// Runs a topology change that needs stopped audio, then restarts audio if it was running,
    /// so a rack edit during a show causes a short gap instead of lasting silence.
    fn with_audio_paused(&mut self, change: impl FnOnce(&mut Self)) {
        #[cfg(target_os = "macos")]
        let resume = self.audio.active_format().is_some();
        #[cfg(target_os = "macos")]
        if resume {
            if let Err(error) = self.audio.stop() {
                self.show_fault(format!(
                    "Audio did not stop before the rack change: {error}"
                ));
                return;
            }
            self.clear_live_handles();
        }
        change(self);
        #[cfg(target_os = "macos")]
        if resume {
            let status = self.status_line.clone();
            self.toggle_engine();
            if self.online() {
                self.set_status(&format!("{status}. Audio resumed."));
            }
        }
    }

    fn rebuild_all_workers_stopped(&mut self) {
        if let Err(error) = self.sync_worker_parameters() {
            self.show_fault(format!("Could not preserve plug-in parameters: {error}"));
            return;
        }
        let mut live_states = Vec::new();
        if let Ok(product) = self.product.as_mut() {
            for rack_index in 0..MAX_RACKS {
                match product.capture_rack_states(rack_index) {
                    Ok(states) => live_states.extend(states),
                    Err(error) => {
                        self.show_fault(format!("Could not preserve plug-in state: {error}"));
                        return;
                    }
                }
            }
        }
        let racks = self.controller.document().model.racks.clone();
        let saved_states = racks
            .iter()
            .map(|rack| self.saved_states(rack))
            .collect::<Result<Vec<_>, _>>();
        let mut saved_states = match saved_states {
            Ok(states) => states,
            Err(error) => {
                self.show_fault(error);
                return;
            }
        };
        for (rack, states) in racks.iter().zip(&mut saved_states) {
            for (slot, state) in rack.slots.iter().zip(states) {
                if let Some(live) = live_states.iter().find(|live| {
                    live.instance_id == slot.id.0
                        && live.metadata.fingerprint == slot.plugin.fingerprint.digest
                }) {
                    *state = Some(live.clone());
                }
            }
        }
        let reload_error = {
            let Ok(product) = self.product.as_mut() else {
                self.clamp_selection();
                return;
            };
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
        self.clamp_selection();
    }

    fn snapshot_scene(
        model: &sp_model::Session,
        id: SceneId,
        name: String,
        transition_ms: u32,
        scope: &[SceneParameterValue],
    ) -> Result<Scene, String> {
        if scope.len() > MAX_SCENE_PARAMETER_VALUES {
            return Err(format!(
                "Scene has {} selected parameters; maximum is {MAX_SCENE_PARAMETER_VALUES}",
                scope.len()
            ));
        }
        let parameter_values = scope
            .iter()
            .map(|target| {
                let value = model
                    .racks
                    .iter()
                    .find(|rack| rack.id == target.rack_id)
                    .and_then(|rack| rack.slots.iter().find(|slot| slot.id == target.slot_id))
                    .and_then(|slot| slot.parameters.values.get(&target.parameter_id))
                    .ok_or_else(|| {
                        format!(
                            "Scene parameter {} is no longer available",
                            target.parameter_id.0
                        )
                    })?;
                Ok(SceneParameterValue {
                    value: *value,
                    ..target.clone()
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Scene {
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
            rack_bypasses: model
                .racks
                .iter()
                .map(|rack| RackBypass {
                    rack_id: rack.id.clone(),
                    bypassed: rack.bypassed,
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
                .collect(),
            parameter_values,
            transition_ms,
        })
    }

    /// Opens the scene picker for a new scene with the default parameter selection.
    fn capture_scene(&mut self) {
        let model = &self.controller.document().model;
        if model.scenes.len() >= sp_model::MAX_SCENES {
            self.show_fault(format!(
                "A session supports at most {} scenes",
                sp_model::MAX_SCENES
            ));
            return;
        }
        let sequence = (1..=model.scenes.len() + 1)
            .find(|number| {
                !model
                    .scenes
                    .iter()
                    .any(|scene| scene.id.0 == format!("scene-{number}"))
            })
            .expect("finite scene list");
        let scene = Self::snapshot_scene(
            model,
            SceneId(format!("scene-{sequence}")),
            format!("Scene {}", model.scenes.len() + 1),
            self.fade_ms,
            &[],
        )
        .expect("empty parameter scope is valid");
        self.open_scene_editor(scene, None);
    }

    fn open_scene_editor(&mut self, scene: Scene, scene_index: Option<usize>) {
        let model = &self.controller.document().model;
        let product = self.product.as_ref().ok();
        self.scene_editor = Some(SceneEditor::new(model, scene, scene_index, |slot| {
            product.map_or(&[], |product| product.scene_parameter_metadata(slot))
        }));
    }

    fn update_current_scene(&mut self) {
        let Some(index) = self.current_scene else {
            self.set_status("Recall or capture a scene before updating it");
            return;
        };
        let Some(existing) = self.controller.document().model.scenes.get(index).cloned() else {
            return;
        };
        let name = existing.name.clone();
        if self.commit_scene(existing, Some(index)) {
            self.set_status(&format!("Scene {name} updated from current values"));
        }
    }

    fn commit_scene(&mut self, scope: Scene, index: Option<usize>) -> bool {
        let mut staged_model = self.controller.document().model.clone();
        let result = (|| {
            let mut scene = Self::snapshot_scene(
                &staged_model,
                scope.id,
                scope.name,
                scope.transition_ms,
                &scope.parameter_values,
            )?;
            if !scene.parameter_values.is_empty() {
                let product = self.product.as_mut().map_err(
                    |_| "Plug-in workers are unavailable; cannot capture current parameter values",
                )?;
                product.capture_scene_parameters(&mut staged_model, &mut scene)?;
            }
            let scene_index = index.unwrap_or(staged_model.scenes.len());
            if let Some(index) = index {
                *staged_model
                    .scenes
                    .get_mut(index)
                    .ok_or("Scene no longer exists")? = scene;
            } else {
                staged_model.scenes.push(scene);
            }
            staged_model.validate().map_err(|error| error.to_string())?;
            Ok::<usize, String>(scene_index)
        })();
        let scene_index = match result {
            Ok(index) => index,
            Err(error) => {
                self.show_fault(format!("Could not capture scene: {error}"));
                return false;
            }
        };
        self.controller.document_mut().model = staged_model;
        self.current_scene = Some(scene_index);
        if self.publish_live_scenes() {
            self.set_status("Scene captured. Only the selected plug-in parameters are recalled.");
        }
        true
    }

    /// Sends the session's scenes to the running engine. Returns false after reporting a fault.
    fn publish_live_scenes(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        if let Some(control) = self.product_control.as_mut()
            && !control.publish_scenes(&self.controller.document().model)
        {
            self.show_fault(
                "The scene change is saved but not live yet. The engine is still applying the last one; try again."
                    .to_owned(),
            );
            return false;
        }
        true
    }

    /// Steps the fade by `steps` × 50 ms. The active scene's transition follows it.
    fn step_fade(&mut self, steps: i32) {
        let fade = i64::from(self.fade_ms) + i64::from(steps) * i64::from(FADE_STEP_MS);
        self.fade_ms = u32::try_from(fade.clamp(0, i64::from(FADE_MAX_MS))).unwrap_or(0);
        if let Some(index) = self.current_scene
            && let Some(scene) = self.controller.document_mut().model.scenes.get_mut(index)
        {
            scene.transition_ms = self.fade_ms;
            self.publish_live_scenes();
        }
    }

    /// Sends new rack output controls to the engine, then commits them to the session.
    fn set_rack_controls(&mut self, rack_index: usize, gain_db: f32, muted: bool, bypassed: bool) {
        if self
            .controller
            .document()
            .model
            .racks
            .get(rack_index)
            .is_none()
        {
            return;
        }
        let Ok(gain_db) = sp_model::GainDb::new(gain_db) else {
            self.show_fault("Rack gain is invalid".to_owned());
            return;
        };
        #[cfg(target_os = "macos")]
        if self.online() {
            let Some(control) = self.product_control.as_mut() else {
                self.show_fault("Rack control queue is unavailable".to_owned());
                return;
            };
            let gain = 10.0_f32.powf(gain_db.get() / 20.0);
            if !control.set_rack_controls(rack_index, gain, muted, bypassed) {
                self.show_fault("Rack control queue is full".to_owned());
                return;
            }
        }
        let rack = &mut self.controller.document_mut().model.racks[rack_index];
        rack.gain_db = gain_db;
        rack.muted = muted;
        rack.bypassed = bypassed;
        if let Ok(product) = self.product.as_mut() {
            product.update_rack_snapshot(rack_index, rack);
        }
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
                self.last_scan = Some(SystemTime::now());
                self.set_status(&format!("Scan complete, {count} bundles cataloged"));
            }
            Err(error) => self.show_fault(format!("Plug-in scan unavailable: {error}")),
        }
    }

    /// Appends a catalog plug-in to the selected rack and reloads that rack.
    fn add_plugin(&mut self, plugin: CatalogPlugin) {
        let name = plugin.descriptor.identity.name.clone();
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
        let rack_name = rack.name.clone();
        rack.slots.push(PluginSlot {
            id,
            plugin: plugin.descriptor,
            bypassed: false,
            parameters: plugin.parameters,
            sidechain: None,
        });
        let slot = rack.slots.len() - 1;
        self.selected_slot = Some(slot);
        self.reload_selected_rack_after_edit(SlotEdit::Add(slot));
        if self.fault.is_none() {
            self.set_status(&format!("{name} added to {rack_name}. The rack reloaded."));
        }
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
        self.reload_selected_rack_after_edit(SlotEdit::Swap(from, to));
    }

    fn remove_slot(&mut self, slot: usize) {
        let model = &mut self.controller.document_mut().model;
        let Some(rack) = model.racks.get_mut(self.selected_rack) else {
            return;
        };
        if slot >= rack.slots.len() {
            return;
        }
        let removed = rack.slots.remove(slot);
        let rack_id = rack.id.clone();
        for scene in &mut model.scenes {
            scene
                .bypasses
                .retain(|value| value.rack_id != rack_id || value.slot_id != removed.id);
            scene
                .parameter_values
                .retain(|value| value.rack_id != rack_id || value.slot_id != removed.id);
        }
        model.midi_mappings.retain(|mapping| {
            mapping.target.rack_id != rack_id || mapping.target.slot_id != removed.id
        });
        self.selected_slot = None;
        self.reload_selected_rack_after_edit(SlotEdit::Remove(slot));
    }

    /// Applies a slot edit already made to the model. While audio runs, the rack's worker
    /// changes in place so its other plug-ins keep playing; if it cannot, only this rack's
    /// worker restarts.
    fn reload_selected_rack_after_edit(&mut self, edit: SlotEdit) {
        #[cfg(target_os = "macos")]
        if let Some(control) = self.product_control.as_mut() {
            if !control.changes_applied() {
                self.show_fault(
                    "The previous rack change is still being applied. Try again.".to_owned(),
                );
                return;
            }
            let rack_index = self.selected_rack;
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
            let before = self.rack_ids();
            let in_place = match self
                .product_mut()
                .and_then(|product| product.edit_slot_live(rack_index, &rack, edit))
            {
                Ok(in_place) => in_place,
                Err(error) => {
                    self.show_fault(error);
                    return;
                }
            };
            // Every rack is kept after an in-place edit; the graph and scenes pick up the new
            // slot layout at the next block.
            let rebuilt: Vec<RackId> = (!in_place).then(|| rack.id.clone()).into_iter().collect();
            self.apply_rack_edit(&before, &rebuilt);
            self.set_status(if in_place {
                "Rack updated. Its other plug-ins kept playing."
            } else {
                "Rack reloaded. Other racks kept playing."
            });
            return;
        }
        let _ = edit;
        let empty = self
            .controller
            .document()
            .model
            .racks
            .get(self.selected_rack)
            .is_none_or(|rack| rack.slots.is_empty());
        if empty {
            self.with_audio_paused(|app| {
                if let Ok(product) = app.product.as_mut() {
                    product.unload_rack(app.selected_rack);
                }
                app.clamp_selection();
                app.set_status("Rack updated");
            });
        } else {
            self.load_selected_rack();
        }
    }

    /// Reloads the selected rack's worker with its current topology.
    fn load_selected_rack(&mut self) {
        #[cfg(target_os = "macos")]
        if self.product_control.is_some() {
            let before = self.rack_ids();
            let rebuilt: Vec<RackId> = before
                .get(self.selected_rack)
                .cloned()
                .into_iter()
                .collect();
            self.apply_rack_edit(&before, &rebuilt);
            self.set_status("Rack reloaded. Other racks kept playing.");
            return;
        }
        self.with_audio_paused(Self::load_selected_rack_stopped);
    }

    fn rack_ids(&self) -> Vec<RackId> {
        self.controller
            .document()
            .model
            .racks
            .iter()
            .map(|rack| rack.id.clone())
            .collect()
    }

    fn load_selected_rack_stopped(&mut self) {
        if let Err(error) = self.sync_worker_parameters() {
            self.show_fault(format!("Could not preserve plug-in parameters: {error}"));
            return;
        }
        let live_states = match self.product.as_mut() {
            Ok(product) => match product.capture_rack_states(self.selected_rack) {
                Ok(states) => states,
                Err(error) => {
                    self.show_fault(format!("Could not preserve plug-in state: {error}"));
                    return;
                }
            },
            Err(_) => Vec::new(),
        };
        let Some(rack) = self
            .controller
            .document()
            .model
            .racks
            .get(self.selected_rack)
            .cloned()
        else {
            return;
        };
        let mut saved_states = match self.saved_states(&rack) {
            Ok(states) => states,
            Err(error) => {
                self.show_fault(error);
                return;
            }
        };
        for (slot, state) in rack.slots.iter().zip(&mut saved_states) {
            if let Some(live) = live_states.iter().find(|live| {
                live.instance_id == slot.id.0
                    && live.metadata.fingerprint == slot.plugin.fingerprint.digest
            }) {
                *state = Some(live.clone());
            }
        }
        let rack_index = self.selected_rack;
        match self
            .product_mut()
            .and_then(|product| product.load_rack(rack_index, &rack, &saved_states))
        {
            Ok(()) => self.set_status("Rack worker ready"),
            Err(error) => self.show_fault(format!("Rack load failed: {error}")),
        }
    }

    /// Opens a slot's native editor beside its column, or brings an open one to front.
    fn open_editor(&mut self, ctx: &egui::Context, rack: usize, slot: usize) {
        self.select_rack(rack);
        self.selected_slot = Some(slot);
        if self.slot_token(rack, slot) == StateToken::Missing {
            let name = self.controller.document().model.racks[rack].slots[slot]
                .plugin
                .identity
                .name
                .clone();
            self.show_fault(format!(
                "{name} is not available on this machine. The slot keeps its saved state until you replace it."
            ));
            return;
        }
        let placement = editor_placement(ctx, rack, slot);
        let running = self.online();
        match self
            .product_mut()
            .and_then(|product| product.begin_native_editor(rack, slot, running, placement))
        {
            Ok(()) => self.set_status("Opening the plug-in editor"),
            Err(error) => self.show_fault(error),
        }
    }

    fn slot_action(&mut self, ctx: &egui::Context, action: SlotAction) {
        let rack_index = self.selected_rack;
        match action {
            SlotAction::Editor(slot) => self.open_editor(ctx, rack_index, slot),
            SlotAction::Bypass(slot) => {
                let result = self
                    .controller
                    .document()
                    .model
                    .racks
                    .get(rack_index)
                    .and_then(|rack| rack.slots.get(slot))
                    .map(|slot| !slot.bypassed)
                    .ok_or_else(|| "slot is unavailable".to_owned())
                    .and_then(|bypassed| {
                        if let Ok(product) = self.product.as_mut()
                            && product.worker_running(rack_index)
                        {
                            product.set_slot_bypass(rack_index, slot, bypassed)?;
                        }
                        if let Some(model_slot) = self
                            .controller
                            .document_mut()
                            .model
                            .racks
                            .get_mut(rack_index)
                            .and_then(|rack| rack.slots.get_mut(slot))
                        {
                            model_slot.bypassed = bypassed;
                        }
                        Ok(bypassed)
                    });
                match result {
                    Ok(true) => self.set_status("Plug-in bypassed"),
                    Ok(false) => self.set_status("Plug-in enabled"),
                    Err(error) => self.show_fault(format!("Bypass failed: {error}")),
                }
            }
            SlotAction::MoveUp(slot) => self.move_slot(slot, slot.saturating_sub(1)),
            SlotAction::MoveDown(slot) => self.move_slot(slot, slot.saturating_add(1)),
            SlotAction::Remove(slot) => self.remove_slot(slot),
        }
    }

    /// Arms MIDI Learn: the next CC maps to the parameter last touched in any plug-in editor.
    #[cfg(target_os = "macos")]
    fn arm_midi_learn(&mut self) {
        if !self.online() || self.midi_cc_monitor.is_none() {
            self.show_fault(
                "Start the engine with a MIDI input before arming MIDI Learn".to_owned(),
            );
            return;
        }
        self.midi_learn.cancel();
        self.midi_learn_target = None;
        self.midi_learn_armed = true;
        self.set_status("MIDI Learn armed. Touch a control in a plug-in editor, then move one CC.");
    }

    fn cancel_midi_learn(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.midi_learn.cancel();
            self.midi_learn_target = None;
            self.midi_learn_armed = false;
        }
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
        self.midi_learn_armed = false;
        let model = &self.controller.document().model;
        let Some((rack, slot)) = model
            .racks
            .get(target.rack_index)
            .and_then(|rack| Some((rack, rack.slots.get(target.slot_index)?)))
        else {
            return;
        };
        let parameter_id = ParameterId(target.parameter_id.to_string());
        let described = self.parameter_label(rack, slot, &parameter_id);
        let mapping = MidiMapping {
            id: MidiMappingId(format!("midi-{channel}-{controller}")),
            source: MidiController {
                channel,
                controller,
            },
            target: ParameterAddress {
                rack_id: rack.id.clone(),
                slot_id: slot.id.clone(),
                parameter_id,
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
            self.set_status(&format!("Mapped CC {controller} to {described}"));
        } else {
            self.show_fault(
                "The MIDI mapping queue is full. Restart the engine to apply the mapping."
                    .to_owned(),
            );
        }
    }

    /// Removes one MIDI mapping and republishes the table.
    fn remove_midi_mapping(&mut self, index: usize) {
        let model = &mut self.controller.document_mut().model;
        if index >= model.midi_mappings.len() {
            return;
        }
        model.midi_mappings.remove(index);
        #[cfg(target_os = "macos")]
        {
            let table = sp_audio_io_macos::product_midi_mappings(&self.controller.document().model);
            self.midi_learn = MidiLearnController::from_table(table);
            if let Some(publisher) = self.midi_mapping_publisher.as_mut()
                && publisher.publish(table).is_err()
            {
                self.show_fault(
                    "The MIDI mapping queue is full. Restart the engine to apply the change."
                        .to_owned(),
                );
                return;
            }
        }
        self.set_status("MIDI mapping removed");
    }

    /// "Rack, Plug-in, Parameter" for a parameter address.
    fn parameter_label(&self, rack: &Rack, slot: &PluginSlot, parameter: &ParameterId) -> String {
        let name = self
            .product
            .as_ref()
            .ok()
            .and_then(|product| {
                product
                    .scene_parameter_metadata(slot)
                    .iter()
                    .find(|metadata| metadata.id.to_string() == parameter.0)
                    .map(|metadata| metadata.name.clone())
            })
            .unwrap_or_else(|| format!("Parameter {}", parameter.0));
        format!("{}, {}, {name}", rack.name, slot.plugin.identity.name)
    }

    #[cfg(target_os = "macos")]
    fn recall_scene(&mut self, scene_index: usize) {
        if !self.online() {
            return;
        }
        let triggered = self
            .product_control
            .as_mut()
            .is_some_and(|control| control.trigger_scene(scene_index));
        if !triggered {
            self.show_fault("Scene trigger queue is unavailable".to_owned());
            return;
        }
        self.apply_scene_snapshot(scene_index);
        if let Some(scene) = self.controller.document().model.scenes.get(scene_index) {
            let message = format!(
                "Scene {} recalled over {} ms",
                scene.name, scene.transition_ms
            );
            self.set_status(&message);
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn recall_scene(&mut self, _scene_index: usize) {}

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
        self.fade_ms = scene.transition_ms;
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
        for bypass in scene.rack_bypasses {
            if let Some(rack) = model
                .racks
                .iter_mut()
                .find(|rack| rack.id == bypass.rack_id)
            {
                rack.bypassed = bypass.bypassed;
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

    fn retry_quarantined_plugin(&mut self, fingerprint: &sp_supervisor::BundleFingerprint) {
        match self
            .product_mut()
            .and_then(|product| product.retry_quarantined_plugin(fingerprint))
        {
            Ok(()) => self.set_status("Quarantine cleared. Load its rack or start the engine."),
            Err(error) => self.show_fault(format!("Could not allow plug-in retry: {error}")),
        }
    }

    /// Retries a rack whose worker restart failed, with fresh plug-in state.
    fn retry_rack_restart(&mut self, rack: usize) {
        match self
            .product_mut()
            .and_then(|product| product.retry_planned_maintenance(rack))
        {
            Ok(()) => self.set_status("Retrying the rack restart with fresh plug-in state"),
            Err(error) => self.show_fault(format!("Rack restart retry failed: {error}")),
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
                || message.contains("failed")
                || message.contains("remains dry")
                || message.contains("mirror is incomplete"))
        {
            self.show_fault(message);
        }
    }

    #[cfg(target_os = "macos")]
    fn publish_latency_changes(&mut self) {
        if !self.online() {
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

    /// Closes the topmost overlay in the brief's order: menus and popovers, modals, an editor
    /// window, the fault line, then the setup page. Returns false when nothing was open.
    fn close_topmost(&mut self) -> bool {
        if self.overlay != Overlay::None {
            if self.overlay == Overlay::Modal(Modal::Recovery) {
                self.restore_recovery();
            }
            self.overlay = Overlay::None;
        } else if self.scene_editor.is_some() {
            self.scene_editor = None;
        } else if self.close_open_editor() {
        } else if self.fault.is_some() {
            self.fault = None;
        } else if self.screen == Screen::Setup {
            self.screen = Screen::Show;
        } else {
            return false;
        }
        true
    }

    /// Closes one open native editor window. Returns false when none is open.
    fn close_open_editor(&mut self) -> bool {
        let racks = self.controller.document().model.racks.len();
        let running = self.online();
        let Ok(product) = self.product.as_mut() else {
            return false;
        };
        let open = (0..racks).find_map(|rack| {
            (0..MAX_SLOTS_PER_RACK)
                .find(|&slot| product.editor_open(rack, slot))
                .map(|slot| (rack, slot))
        });
        let Some((rack, slot)) = open else {
            return false;
        };
        if let Err(error) = product.set_native_editor_open(rack, slot, false, running) {
            self.show_fault(error);
        }
        true
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers};
        if self.rename.is_none()
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.close_topmost();
            return;
        }
        if ctx.wants_keyboard_input() {
            return;
        }
        let command = |key| ctx.input_mut(|input| input.consume_key(Modifiers::COMMAND, key));
        if command(Key::S) {
            self.save_session();
        }
        if command(Key::Comma) {
            self.screen = match self.screen {
                Screen::Show => Screen::Setup,
                Screen::Setup => Screen::Show,
            };
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::C))
            && self.online()
            && self.overlay == Overlay::None
        {
            self.capture_scene();
        }
        if command(Key::N) {
            self.add_rack();
        }
        if let Some(key) = PAGE_KEYS.iter().position(|key| command(*key)) {
            let pages = self.controller.document().model.pages.len();
            match key {
                0 => self.show_page(None),
                page if page <= pages => self.show_page(Some(page - 1)),
                _ => {}
            }
        }
        let blocked = self.overlay != Overlay::None
            || self.scene_editor.is_some()
            || self.screen != Screen::Show
            || ctx.memory(|memory| memory.focused().is_some());
        if blocked {
            return;
        }
        if command(Key::ArrowLeft) {
            self.move_selected_rack(-1);
        }
        if command(Key::ArrowRight) {
            self.move_selected_rack(1);
        }
        self.handle_show_keys(ctx);
    }

    /// Scene numbers and slot keys, which only act on the show screen with nothing open.
    fn handle_show_keys(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers};
        const SCENE_KEYS: [Key; 8] = [
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
        ];
        let plain = |key| ctx.input_mut(|input| input.consume_key(Modifiers::NONE, key));
        if let Some(scene) = SCENE_KEYS.iter().position(|key| plain(*key))
            && scene < self.controller.document().model.scenes.len()
        {
            self.recall_scene(scene);
        }
        let slots = self
            .controller
            .document()
            .model
            .racks
            .get(self.selected_rack)
            .map_or(0, |rack| rack.slots.len());
        if slots == 0 {
            return;
        }
        if plain(Key::ArrowDown) {
            self.selected_slot = Some(
                self.selected_slot
                    .map_or(0, |slot| (slot + 1).min(slots - 1)),
            );
        }
        if plain(Key::ArrowUp) {
            self.selected_slot = Some(self.selected_slot.map_or(0, |slot| slot.saturating_sub(1)));
        }
        if let Some(slot) = self.selected_slot {
            if plain(Key::Enter) {
                self.slot_action(ctx, SlotAction::Editor(slot));
            }
            if plain(Key::B) {
                self.slot_action(ctx, SlotAction::Bypass(slot));
            }
        }
    }

    fn product_mut(&mut self) -> Result<&mut ProductRuntime, String> {
        self.product.as_mut().map_err(|error| error.clone())
    }

    #[cfg(target_os = "macos")]
    fn update_meters(&mut self) {
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        let now = Instant::now();
        if self.motion == MotionPreference::Reduced
            && self
                .meters_updated_at
                .is_some_and(|at| now.duration_since(at) < REDUCED_MOTION_METER_INTERVAL)
        {
            return;
        }
        self.meters_updated_at = Some(now);
        let elapsed = self.meter_clock.elapsed();
        for rack in 0..self.controller.document().model.racks.len() {
            if let Some(input) = telemetry.take_rack_input_meter(rack) {
                self.rack_input_meters[rack].update(input.peak, input.clipped, elapsed);
            }
            if let Some(output) = telemetry.take_rack_output_meter(rack) {
                self.rack_output_meters[rack].update(output.peak, output.clipped, elapsed);
            }
        }
        if now.duration_since(self.load_sampled_at) >= LOAD_SAMPLE_INTERVAL {
            self.load_sampled_at = now;
            let load = telemetry.take_callback_load().unwrap_or(0.0);
            self.load_history.rotate_left(1);
            self.load_history[LOAD_BARS - 1] = load;
        }
    }

    /// The latest callback load, or `None` while offline.
    fn callback_load(&self) -> Option<f32> {
        self.online().then(|| self.load_history[LOAD_BARS - 1])
    }

    /// Total deadline misses across racks, or `None` while offline.
    fn deadline_misses(&self) -> Option<u64> {
        #[cfg(target_os = "macos")]
        if let Some(telemetry) = &self.telemetry {
            return Some(
                (0..MAX_RACKS)
                    .filter_map(|rack| telemetry.rack_diagnostics(rack))
                    .map(|diagnostics| diagnostics.deadline_misses)
                    .sum(),
            );
        }
        None
    }

    fn session_name(&self) -> String {
        self.controller.root().file_stem().map_or_else(
            || "Untitled".to_owned(),
            |stem| stem.to_string_lossy().into_owned(),
        )
    }

    /// Ingests new editor pictures and, once, the pictures stored in the session.
    fn poll_previews(&mut self, ctx: &egui::Context) {
        if !self.previews.loaded() {
            let ids: Vec<String> = self
                .controller
                .document()
                .model
                .racks
                .iter()
                .flat_map(|rack| rack.slots.iter().map(|slot| slot.id.0.clone()))
                .collect();
            for id in ids {
                match self.controller.load_editor_preview(&id) {
                    Ok(Some(file)) => self.previews.insert(ctx, file),
                    Ok(None) => {}
                    Err(error) => self.show_fault(format!("Editor picture unreadable: {error}")),
                }
            }
            self.previews.mark_loaded();
        }
        let updates = self
            .product
            .as_mut()
            .map_or_else(|_| Vec::new(), ProductRuntime::take_editor_previews);
        for preview in updates {
            self.previews.insert(ctx, preview);
        }
    }
}

impl eframe::App for LiveRackApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame(ctx);
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

impl LiveRackApp {
    /// Runs one UI frame. Kept separate from `eframe::App` so tests can drive it headlessly.
    fn frame(&mut self, ctx: &egui::Context) {
        let online = self.online();
        while let Some(result) = self
            .product
            .as_mut()
            .ok()
            .and_then(|product| product.poll_native_editor_result(online))
        {
            match result {
                Ok(()) => self.set_status("Plug-in editor open"),
                Err(error) => self.show_fault(error),
            }
        }
        #[cfg(target_os = "macos")]
        let stopped_dispatcher_racks = self.audio.service_stopped_recoveries();
        if let Ok(product) = self.product.as_mut() {
            #[cfg(target_os = "macos")]
            product.poll_with_stopped_dispatcher(stopped_dispatcher_racks.as_ref());
            #[cfg(not(target_os = "macos"))]
            product.poll();
            if product.native_editor_pending() || product.maintenance_pending() {
                ctx.request_repaint_after(Duration::from_millis(16));
            }
        }
        self.poll_parameter_mirror();
        self.poll_product_diagnostic();
        #[cfg(target_os = "macos")]
        {
            self.publish_latency_changes();
            if let (Ok(product), Some(control)) =
                (self.product.as_mut(), self.product_control.as_mut())
            {
                product.finish_live_topology(control);
            }
            self.poll_audio_event();
            self.poll_reconnect(Instant::now());
            if self.reconnect_at.is_some() {
                ctx.request_repaint_after(RECONNECT_INTERVAL);
            }
            self.poll_midi_learn();
            self.poll_realtime_scene();
            self.update_meters();
        }
        self.poll_previews(ctx);
        self.autosave_if_due(Instant::now());
        ctx.request_repaint_after(
            self.next_autosave_at
                .saturating_duration_since(Instant::now()),
        );
        // Preview captions and "saved 4 s ago" age once a second; meters repaint at 30 Hz.
        ctx.request_repaint_after(Duration::from_secs(1));
        if self.online() {
            ctx.request_repaint_after(match self.motion {
                MotionPreference::Full => sp_ui::components::METER_REPAINT_INTERVAL,
                MotionPreference::Reduced => REDUCED_MOTION_METER_INTERVAL,
            });
        }
        self.clamp_selection();
        self.handle_keys(ctx);
        if ctx.input(|input| input.viewport().close_requested()) && self.online() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.overlay = Overlay::Modal(Modal::Quit);
        }

        self.draw_head(ctx);
        if self.fault.is_some() {
            self.draw_fault_line(ctx);
        }
        let model = &self.controller.document().model;
        if self.screen == Screen::Show
            && (!model.pages.is_empty() || model.racks.len() > PAGE_LINE_RACKS)
        {
            self.draw_page_line(ctx);
        }
        self.draw_foot(ctx);
        match self.screen {
            Screen::Show => self.draw_columns(ctx),
            Screen::Setup => self.draw_setup(ctx),
        }
        self.draw_overlay(ctx);
        self.draw_scene_editor(ctx);
    }
}

/// Reads the system's reduced-motion preference once at startup.
fn reduced_motion_preference() -> MotionPreference {
    #[cfg(target_os = "macos")]
    if objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() {
        return MotionPreference::Reduced;
    }
    MotionPreference::Full
}

/// Screen position for a new editor window: beside its column, stepping down per slot.
fn editor_placement(
    ctx: &egui::Context,
    rack: usize,
    slot: usize,
) -> Option<crate::EditorPlacement> {
    const LEFT: f32 = 520.0;
    const TOP: f32 = 180.0;
    const RACK_STEP: f32 = 40.0;
    const SLOT_STEP: f32 = 20.0;
    let window = ctx.input(|input| input.viewport().inner_rect)?;
    #[allow(
        clippy::cast_precision_loss,
        reason = "rack and slot indices are below 8"
    )]
    let (rack, slot) = (rack as f32, slot as f32);
    Some(crate::EditorPlacement {
        left: window.left() + LEFT + rack * RACK_STEP,
        top: window.top() + TOP + slot * SLOT_STEP,
    })
}

/// A new rack's default route: the next mono input and the next stereo output pair the
/// device has, wrapping to the first ones.
fn new_rack_route(position: usize, inputs: u16, outputs: u16) -> RackChannelRoute {
    let position = u16::try_from(position).unwrap_or(0);
    let input = if position < inputs { position } else { 0 };
    let pair = if position * 2 + 1 < outputs {
        position * 2
    } else {
        0
    };
    let channel = |value: u16| u8::try_from(value).unwrap_or(0);
    RackChannelRoute {
        input: Some(PhysicalChannels::Mono {
            channel: channel(input),
        }),
        output: PhysicalChannels::Stereo {
            left: channel(pair),
            right: channel(pair + 1),
        },
    }
}

/// A rack's hardware route, falling back to its source and endpoint layouts.
fn rack_route(model: &sp_model::Session, rack: &sp_model::Rack) -> RackChannelRoute {
    model.rack_routes.get(&rack.id).copied().unwrap_or_else(|| {
        let layout =
            |found: Option<&ChannelLayout>| found.unwrap_or(&ChannelLayout::Stereo).clone();
        let input = layout(
            model
                .sources
                .iter()
                .find(|source| source.id == rack.source_id)
                .map(|source| &source.layout),
        );
        let output = layout(
            model
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == rack.endpoint_id)
                .map(|endpoint| &endpoint.layout),
        );
        RackChannelRoute {
            input: Some(default_physical_channels(&input)),
            output: default_physical_channels(&output),
        }
    })
}

/// `1`, `1-2`, or `1/4` for a channel selection.
fn channels_label(channels: PhysicalChannels) -> String {
    match channels {
        PhysicalChannels::Mono { channel } => format!("{}", u16::from(channel) + 1),
        PhysicalChannels::Stereo { left, right } if u16::from(right) == u16::from(left) + 1 => {
            format!("{}-{}", u16::from(left) + 1, u16::from(right) + 1)
        }
        PhysicalChannels::Stereo { left, right } => {
            format!("{}/{}", u16::from(left) + 1, u16::from(right) + 1)
        }
    }
}

/// `-3.0 dB`, `+2.0 dB`, or `-inf dB`.
fn gain_label(gain_db: f32) -> String {
    if gain_db <= show::GAIN_MIN_DB {
        "-inf dB".to_owned()
    } else if gain_db > 0.0 {
        format!("+{gain_db:.1} dB")
    } else {
        format!("{gain_db:.1} dB")
    }
}

#[cfg(target_os = "macos")]
fn route_devices() -> Result<Vec<MacOsAudioDevice>, Box<dyn std::error::Error + Send + Sync>> {
    Ok(enumerate_devices()?
        .into_iter()
        .filter(|device| {
            (device.capabilities.max_input_channels > 0
                || device.capabilities.max_output_channels > 0)
                && !device.supported_buffer_frames.is_empty()
        })
        .collect())
}

#[cfg(target_os = "macos")]
fn saved_route(
    devices: &[MacOsAudioDevice],
    settings: &AudioDeviceSettings,
) -> Option<AudioRouteConfig> {
    let output = resolve_audio_device(devices, &settings.output, false)?;
    let input = match &settings.input {
        Some(selection) => Some(resolve_audio_device(devices, selection, true)?),
        None => None,
    };
    Some(AudioRouteConfig {
        input,
        output,
        format: AudioFormat::product_stereo(settings.buffer_frames)
            .expect("saved product buffer size"),
    })
}

#[cfg(target_os = "macos")]
fn resolve_audio_device(
    devices: &[MacOsAudioDevice],
    saved: &AudioDeviceSelection,
    input: bool,
) -> Option<sp_audio_io::AudioDeviceId> {
    let candidates = devices.iter().filter(|device| {
        if input {
            device.capabilities.max_input_channels > 0
        } else {
            device.capabilities.max_output_channels > 0
        }
    });
    if let Some(device) = candidates
        .clone()
        .find(|device| device.info.id.as_str() == saved.id && device.info.name == saved.name)
    {
        return Some(device.info.id.clone());
    }
    let mut matching_names = candidates.filter(|device| device.info.name == saved.name);
    let device = matching_names.next()?;
    matching_names
        .next()
        .is_none()
        .then(|| device.info.id.clone())
}

#[cfg(target_os = "macos")]
fn available_buffer_frames(devices: &[MacOsAudioDevice], route: &AudioRouteConfig) -> Vec<u32> {
    let Some(output) = devices.iter().find(|device| device.info.id == route.output) else {
        return Vec::new();
    };
    output
        .supported_buffer_frames
        .iter()
        .copied()
        .filter(|frames| {
            route.input.as_ref().is_none_or(|input| {
                devices
                    .iter()
                    .find(|device| &device.info.id == input)
                    .is_some_and(|device| device.supported_buffer_frames.contains(frames))
            })
        })
        .collect()
}

fn default_physical_channels(layout: &ChannelLayout) -> PhysicalChannels {
    if layout.channels() == 1 {
        PhysicalChannels::Mono { channel: 0 }
    } else {
        PhysicalChannels::Stereo { left: 0, right: 1 }
    }
}

fn physical_channels_fit(route: PhysicalChannels, channel_count: u16) -> bool {
    match route {
        PhysicalChannels::Mono { channel } => u16::from(channel) < channel_count,
        PhysicalChannels::Stereo { left, right } => {
            left != right && u16::from(left) < channel_count && u16::from(right) < channel_count
        }
    }
}

#[cfg(test)]
mod tests;
