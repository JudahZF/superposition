//! Product [`AudioEndpoint`] implementation over the Phase 1 `CoreAudio` harness.

use sp_audio_io::{
    AudioDeviceInfo, AudioEndpoint, AudioEndpointEvent, AudioFormat, AudioFormatError,
    AudioRouteConfig,
};
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use rtrb::{Consumer, Producer, RingBuffer};
use sp_engine::{PreparedGraph, RackMeterSnapshot, RealtimeRackMixer};
use sp_midi::{
    BoundedMidiEvents, MAX_SCENE_PARAMETERS, MidiInput, MidiLearnTable, MidiMappingReceiver,
    MidiMappingTarget, MidirInput, SceneParameterTarget, ScenePlayer, SceneRampBlock,
    map_controller_to_target, sample_offset_for_timestamp, scene_index_for_program,
};
use sp_model::{MAX_RACKS, MAX_SLOTS_PER_RACK, Session};
use sp_shared_memory::{BLOCK_EVENT_PARAMETER, BLOCK_EVENT_SLOT_BYPASS, BlockEvent, MidiEvent};
use sp_shared_memory_macos::{MappedRackBanks, RackRecoverySignal, SharedMemoryRegion};

use crate::{
    ActiveDuplex, ActiveOutput, CoreAudioError, DuplexRenderer, DuplexStereoF32,
    InterleavedStereoF32, PhaseOneConfig, PhaseOneFrames, PhaseOneRenderer, RenderDisposition,
    enumerate_devices,
};

const PRODUCT_SAMPLE_RATE: u32 = 48_000;

#[derive(Clone, Copy)]
enum ProductCommand {
    TriggerScene(usize),
    SetRackGain { rack: usize, gain: f32 },
    SetRackMuted { rack: usize, muted: bool },
    SetRackBypassed { rack: usize, bypassed: bool },
    SetRackLatency { rack: usize, samples: u32 },
}

/// Main-thread endpoint for bounded live engine changes.
pub struct ProductControl {
    producer: Producer<ProductCommand>,
}

/// Callback-side endpoint paired with [`ProductControl`].
pub struct ProductControlReceiver {
    consumer: Consumer<ProductCommand>,
}

impl ProductControl {
    /// Creates a preallocated control/callback pair.
    #[must_use]
    pub fn new() -> (Self, ProductControlReceiver) {
        let (producer, consumer) = RingBuffer::new(64);
        (Self { producer }, ProductControlReceiver { consumer })
    }

    fn push(&mut self, command: ProductCommand) -> bool {
        self.producer.push(command).is_ok()
    }

    /// Requests a scene at the next audio block boundary.
    pub fn trigger_scene(&mut self, scene: usize) -> bool {
        self.push(ProductCommand::TriggerScene(scene))
    }

    /// Changes one rack's linear gain at the next block boundary.
    pub fn set_rack_gain(&mut self, rack: usize, gain: f32) -> bool {
        self.push(ProductCommand::SetRackGain { rack, gain })
    }

    /// Changes one rack's mute state at the next block boundary.
    pub fn set_rack_muted(&mut self, rack: usize, muted: bool) -> bool {
        self.push(ProductCommand::SetRackMuted { rack, muted })
    }

    /// Changes one rack's latency-matched bypass state at the next block boundary.
    pub fn set_rack_bypassed(&mut self, rack: usize, bypassed: bool) -> bool {
        self.push(ProductCommand::SetRackBypassed { rack, bypassed })
    }

    /// Updates one rack's latency-matched dry delay at the next block boundary.
    pub fn set_rack_latency(&mut self, rack: usize, samples: u32) -> bool {
        self.push(ProductCommand::SetRackLatency { rack, samples })
    }
}

struct AtomicProductMeter {
    peak: [AtomicU32; 2],
    rms: [AtomicU32; 2],
    clipped: AtomicBool,
}

impl AtomicProductMeter {
    fn new() -> Self {
        Self {
            peak: std::array::from_fn(|_| AtomicU32::new(0)),
            rms: std::array::from_fn(|_| AtomicU32::new(0)),
            clipped: AtomicBool::new(false),
        }
    }

    fn publish(&self, snapshot: RackMeterSnapshot) {
        for channel in 0..2 {
            self.peak[channel].store(snapshot.peak[channel].to_bits(), Ordering::Release);
            self.rms[channel].store(snapshot.rms[channel].to_bits(), Ordering::Release);
        }
        self.clipped.store(snapshot.clipped, Ordering::Release);
    }

    fn snapshot(&self) -> RackMeterSnapshot {
        RackMeterSnapshot {
            peak: std::array::from_fn(|channel| {
                f32::from_bits(self.peak[channel].load(Ordering::Acquire))
            }),
            rms: std::array::from_fn(|channel| {
                f32::from_bits(self.rms[channel].load(Ordering::Acquire))
            }),
            clipped: self.clipped.load(Ordering::Acquire),
        }
    }
}

/// Lock-free meter snapshots retained by the UI while the renderer is active.
pub struct ProductTelemetry {
    racks: [AtomicProductMeter; MAX_RACKS],
    output: AtomicProductMeter,
    scene: AtomicU32,
    completed: [AtomicU64; MAX_RACKS],
    deadline_misses: [AtomicU64; MAX_RACKS],
    protocol_rejections: [AtomicU64; MAX_RACKS],
}

/// Latest callback diagnostics for one rack worker.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProductRackDiagnostics {
    /// Valid blocks accepted from the rack worker.
    pub completed: u64,
    /// Blocks that missed the bounded callback deadline.
    pub deadline_misses: u64,
    /// Malformed or invalid shared-memory observations.
    pub protocol_rejections: u64,
}

impl ProductTelemetry {
    fn new() -> Self {
        Self {
            racks: std::array::from_fn(|_| AtomicProductMeter::new()),
            output: AtomicProductMeter::new(),
            scene: AtomicU32::new(u32::MAX),
            completed: std::array::from_fn(|_| AtomicU64::new(0)),
            deadline_misses: std::array::from_fn(|_| AtomicU64::new(0)),
            protocol_rejections: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Returns the latest meter for one rack.
    #[must_use]
    pub fn rack_meter(&self, rack: usize) -> Option<RackMeterSnapshot> {
        self.racks.get(rack).map(AtomicProductMeter::snapshot)
    }

    /// Returns the latest final-output meter.
    #[must_use]
    pub fn output_meter(&self) -> RackMeterSnapshot {
        self.output.snapshot()
    }

    /// Returns the latest scene selected by UI or MIDI Program Change.
    #[must_use]
    pub fn current_scene(&self) -> Option<usize> {
        let scene = self.scene.load(Ordering::Acquire);
        (scene != u32::MAX).then(|| usize::try_from(scene).unwrap_or(usize::MAX))
    }

    /// Returns callback/worker counters for one rack.
    #[must_use]
    pub fn rack_diagnostics(&self, rack: usize) -> Option<ProductRackDiagnostics> {
        Some(ProductRackDiagnostics {
            completed: self.completed.get(rack)?.load(Ordering::Acquire),
            deadline_misses: self.deadline_misses[rack].load(Ordering::Acquire),
            protocol_rejections: self.protocol_rejections[rack].load(Ordering::Acquire),
        })
    }
}

/// Model identifiers resolved to fixed numeric realtime scene targets.
#[derive(Clone)]
pub struct PreparedProductScene {
    parameters: [SceneParameterTarget; MAX_SCENE_PARAMETERS],
    parameter_count: usize,
    gains: [Option<f32>; MAX_RACKS],
    mutes: [Option<bool>; MAX_RACKS],
    bypasses: [Option<bool>; MAX_RACKS * MAX_SLOTS_PER_RACK],
    transition_ms: u32,
}

impl PreparedProductScene {
    fn empty(transition_ms: u32) -> Self {
        Self {
            parameters: [SceneParameterTarget {
                rack_index: 0,
                slot_index: 0,
                parameter_id: 0,
                value: 0.0,
            }; MAX_SCENE_PARAMETERS],
            parameter_count: 0,
            gains: [None; MAX_RACKS],
            mutes: [None; MAX_RACKS],
            bypasses: [None; MAX_RACKS * MAX_SLOTS_PER_RACK],
            transition_ms,
        }
    }

    fn parameters(&self) -> &[SceneParameterTarget] {
        &self.parameters[..self.parameter_count]
    }
}

/// Resolves all session scenes before audio starts.
#[must_use]
pub fn prepare_product_scenes(model: &Session) -> Vec<PreparedProductScene> {
    model
        .scenes
        .iter()
        .map(|scene| {
            let mut prepared = PreparedProductScene::empty(scene.transition_ms);
            for gain in &scene.gains {
                if let Some(rack) = model.racks.iter().position(|rack| rack.id == gain.rack_id) {
                    prepared.gains[rack] = Some(10.0_f32.powf(gain.gain_db.get() / 20.0));
                }
            }
            for mute in &scene.mutes {
                if let Some(rack) = model.racks.iter().position(|rack| rack.id == mute.rack_id) {
                    prepared.mutes[rack] = Some(mute.muted);
                }
            }
            for bypass in &scene.bypasses {
                if let Some((rack, slot)) = resolve_slot(model, &bypass.rack_id, &bypass.slot_id) {
                    prepared.bypasses[rack * MAX_SLOTS_PER_RACK + slot] = Some(bypass.bypassed);
                }
            }
            for parameter in scene.parameter_values.iter().take(MAX_SCENE_PARAMETERS) {
                let Some((rack, slot)) =
                    resolve_slot(model, &parameter.rack_id, &parameter.slot_id)
                else {
                    continue;
                };
                let Ok(parameter_id) = parameter.parameter_id.0.parse() else {
                    continue;
                };
                prepared.parameters[prepared.parameter_count] = SceneParameterTarget {
                    rack_index: rack,
                    slot_index: slot,
                    parameter_id,
                    value: parameter.value.get(),
                };
                prepared.parameter_count += 1;
            }
            prepared
        })
        .collect()
}

/// Resolves persisted normalized values for scene interpolation before audio starts.
#[must_use]
pub fn current_product_parameters(model: &Session) -> Vec<SceneParameterTarget> {
    model
        .racks
        .iter()
        .enumerate()
        .flat_map(|(rack, model_rack)| {
            model_rack
                .slots
                .iter()
                .enumerate()
                .flat_map(move |(slot, model_slot)| {
                    model_slot
                        .parameters
                        .values
                        .iter()
                        .filter_map(move |(parameter_id, value)| {
                            Some(SceneParameterTarget {
                                rack_index: rack,
                                slot_index: slot,
                                parameter_id: parameter_id.0.parse().ok()?,
                                value: value.get(),
                            })
                        })
                })
        })
        .take(MAX_SCENE_PARAMETERS)
        .collect()
}

/// Resolves persisted channel/CC mappings to numeric realtime targets.
#[must_use]
pub fn product_midi_mappings(model: &Session) -> MidiLearnTable {
    model
        .midi_mappings
        .iter()
        .filter_map(|mapping| {
            let (rack, slot) =
                resolve_slot(model, &mapping.target.rack_id, &mapping.target.slot_id)?;
            Some((
                mapping.source.channel,
                mapping.source.controller,
                MidiMappingTarget {
                    rack_index: rack,
                    slot_index: slot,
                    parameter_id: mapping.target.parameter_id.0.parse().ok()?,
                    minimum: mapping.minimum.get(),
                    maximum: mapping.maximum.get(),
                },
            ))
        })
        .fold(MidiLearnTable::default(), |table, (channel, cc, target)| {
            table.with_mapping(channel, cc, target)
        })
}

fn resolve_slot(
    model: &Session,
    rack_id: &sp_model::RackId,
    slot_id: &sp_model::PluginInstanceId,
) -> Option<(usize, usize)> {
    let rack = model.racks.iter().position(|rack| &rack.id == rack_id)?;
    let slot = model.racks[rack]
        .slots
        .iter()
        .position(|slot| &slot.id == slot_id)?;
    Some((rack, slot))
}

#[derive(Clone)]
struct ActiveRackScene {
    start_sample: u64,
    transition_samples: u64,
    starts: [f32; MAX_RACKS],
    gains: [Option<f32>; MAX_RACKS],
    mutes: [Option<bool>; MAX_RACKS],
    bypasses: [Option<bool>; MAX_RACKS * MAX_SLOTS_PER_RACK],
}

/// Renderer that owns the product mixer for the `CoreAudio` callback.
///
/// Configure the mixer on the control thread before [`MacOsAudioEndpoint::start`]. While the
/// stream is running the mixer is exclusively borrowed by the realtime callback.
pub struct ProductRenderer {
    mixer: RealtimeRackMixer,
    dispatcher: Option<crate::RackSharedMemoryDispatcher>,
    midi: Option<MidirInput>,
    midi_events: BoundedMidiEvents,
    wire_midi: [MidiEvent; sp_shared_memory::MAX_MIDI_EVENTS],
    automation: [crate::RackAutomationEvents; MAX_RACKS],
    control: Option<ProductControlReceiver>,
    mapping_receiver: Option<MidiMappingReceiver>,
    mappings: MidiLearnTable,
    scenes: Vec<PreparedProductScene>,
    scene_player: ScenePlayer,
    scene_block: SceneRampBlock,
    current_parameters: Vec<SceneParameterTarget>,
    active_rack_scene: Option<ActiveRackScene>,
    sample_clock: u64,
    telemetry: Arc<ProductTelemetry>,
}

impl ProductRenderer {
    /// Creates a renderer for an initial graph.
    #[must_use]
    #[allow(
        clippy::large_stack_arrays,
        reason = "fixed MIDI and automation storage is preallocated once at startup"
    )]
    pub fn new(graph: PreparedGraph) -> Self {
        Self {
            mixer: RealtimeRackMixer::new(graph),
            dispatcher: None,
            midi: None,
            midi_events: BoundedMidiEvents::new(),
            wire_midi: [MidiEvent::default(); sp_shared_memory::MAX_MIDI_EVENTS],
            automation: [crate::RackAutomationEvents::new(); MAX_RACKS],
            control: None,
            mapping_receiver: None,
            mappings: MidiLearnTable::default(),
            scenes: Vec::new(),
            scene_player: ScenePlayer::new(),
            scene_block: SceneRampBlock::new(),
            current_parameters: Vec::new(),
            active_rack_scene: None,
            sample_clock: 0,
            telemetry: Arc::new(ProductTelemetry::new()),
        }
    }

    /// Creates a renderer with control-plane-created shared-memory banks, one per rack.
    ///
    /// The mappings and all dispatcher buffers are installed before the callback starts.
    ///
    /// # Errors
    ///
    /// Returns an error when the bank count exceeds the fixed rack capacity or the macOS
    /// monotonic clock cannot be initialized.
    #[allow(
        clippy::large_stack_arrays,
        reason = "fixed MIDI and automation storage is preallocated once at startup"
    )]
    pub fn with_rack_banks(
        graph: PreparedGraph,
        banks: Vec<(usize, [SharedMemoryRegion; 2], Arc<RackRecoverySignal>)>,
    ) -> io::Result<Self> {
        let banks = banks
            .into_iter()
            .map(|(rack_index, [active, inactive], recovery)| {
                // SAFETY: the control plane supplies a fresh inactive mapping that no worker has
                // received; the first mapping is the only initially active worker bank.
                unsafe { MappedRackBanks::from_regions(active, inactive) }
                    .map(|banks| (rack_index, banks, recovery))
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            mixer: RealtimeRackMixer::new(graph),
            dispatcher: Some(crate::RackSharedMemoryDispatcher::new_indexed_recoverable(
                banks,
            )?),
            midi: None,
            midi_events: BoundedMidiEvents::new(),
            wire_midi: [MidiEvent::default(); sp_shared_memory::MAX_MIDI_EVENTS],
            automation: [crate::RackAutomationEvents::new(); MAX_RACKS],
            control: None,
            mapping_receiver: None,
            mappings: MidiLearnTable::default(),
            scenes: Vec::new(),
            scene_player: ScenePlayer::new(),
            scene_block: SceneRampBlock::new(),
            current_parameters: Vec::new(),
            active_rack_scene: None,
            sample_clock: 0,
            telemetry: Arc::new(ProductTelemetry::new()),
        })
    }

    /// Borrows the mixer for control-thread configuration before start.
    pub fn mixer_mut(&mut self) -> &mut RealtimeRackMixer {
        &mut self.mixer
    }

    /// Installs an already opened callback-safe MIDI input before audio starts.
    #[must_use]
    pub fn with_midi_input(mut self, midi: MidirInput) -> Self {
        self.midi = Some(midi);
        self
    }

    /// Installs all prebuilt live-control state before the callback starts.
    #[must_use]
    pub fn with_live_control(
        mut self,
        control: ProductControlReceiver,
        mapping_receiver: MidiMappingReceiver,
        mappings: MidiLearnTable,
        scenes: Vec<PreparedProductScene>,
        current_parameters: Vec<SceneParameterTarget>,
    ) -> Self {
        self.control = Some(control);
        self.mapping_receiver = Some(mapping_receiver);
        self.mappings = mappings;
        self.scenes = scenes;
        self.current_parameters = current_parameters;
        self
    }

    /// Returns the lock-free meter handle retained by the UI while audio owns this renderer.
    #[must_use]
    pub fn telemetry(&self) -> Arc<ProductTelemetry> {
        Arc::clone(&self.telemetry)
    }
}

impl ProductRenderer {
    fn service_control(&mut self) {
        if let Some(receiver) = self.mapping_receiver.as_mut() {
            receiver.receive_latest(&mut self.mappings);
        }
        while let Some(command) = self
            .control
            .as_mut()
            .and_then(|control| control.consumer.pop().ok())
        {
            match command {
                ProductCommand::TriggerScene(scene) => self.trigger_scene(scene),
                ProductCommand::SetRackGain { rack, gain } => {
                    self.mixer.set_rack_gain(rack, gain);
                }
                ProductCommand::SetRackMuted { rack, muted } => {
                    self.mixer.set_rack_muted(rack, muted);
                }
                ProductCommand::SetRackBypassed { rack, bypassed } => {
                    self.mixer.set_rack_bypassed(rack, bypassed);
                }
                ProductCommand::SetRackLatency { rack, samples } => {
                    self.mixer
                        .set_dry_delay_frames(rack, usize::try_from(samples).unwrap_or(usize::MAX));
                }
            }
        }
    }

    fn trigger_scene(&mut self, scene_index: usize) {
        let Some(scene) = self.scenes.get(scene_index) else {
            return;
        };
        self.telemetry.scene.store(
            u32::try_from(scene_index).unwrap_or(u32::MAX),
            Ordering::Release,
        );
        self.scene_player.trigger_targets(
            scene.parameters(),
            &self.current_parameters,
            scene.transition_ms,
            self.sample_clock,
            PRODUCT_SAMPLE_RATE,
        );
        let starts = std::array::from_fn(|rack| {
            self.mixer
                .rack_settings(rack)
                .map_or(1.0, |settings| settings.gain)
        });
        for (rack, muted) in scene.mutes.iter().copied().enumerate() {
            if muted == Some(false) {
                self.mixer.set_rack_muted(rack, false);
            }
        }
        self.active_rack_scene = Some(ActiveRackScene {
            start_sample: self.sample_clock,
            transition_samples: u64::from(scene.transition_ms)
                .saturating_mul(u64::from(PRODUCT_SAMPLE_RATE))
                .div_ceil(1_000)
                .max(1),
            starts,
            gains: scene.gains,
            mutes: scene.mutes,
            bypasses: scene.bypasses,
        });
    }

    fn push_parameter(&mut self, target: SceneParameterTarget, frame_offset: u32) {
        let Some(events) = self.automation.get_mut(target.rack_index) else {
            return;
        };
        let _ = events.push(BlockEvent {
            frame_offset,
            event_type: BLOCK_EVENT_PARAMETER,
            key: target.parameter_id,
            value: target.value.clamp(0.0, 1.0),
            flags: u32::try_from(target.slot_index.saturating_add(1)).unwrap_or(u32::MAX),
        });
        if let Some(current) = self.current_parameters.iter_mut().find(|current| {
            current.rack_index == target.rack_index
                && current.slot_index == target.slot_index
                && current.parameter_id == target.parameter_id
        }) {
            current.value = target.value;
        }
    }

    fn service_scene(&mut self, frames: usize) {
        self.scene_player.render_block(
            self.sample_clock,
            u32::try_from(frames).unwrap_or(u32::MAX),
            &mut self.scene_block,
        );
        let scene_block = self.scene_block.clone();
        for &target in scene_block.targets() {
            self.push_parameter(target, 0);
        }

        let Some(active) = self.active_rack_scene.as_ref() else {
            return;
        };
        let elapsed = self
            .sample_clock
            .saturating_add(u64::try_from(frames).unwrap_or(u64::MAX))
            .saturating_sub(active.start_sample);
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            reason = "normalized ramp progress never needs more than f32 precision"
        )]
        let progress = (elapsed.min(active.transition_samples) as f64
            / active.transition_samples as f64) as f32;
        for (rack, end) in active.gains.iter().copied().enumerate() {
            if let Some(end) = end {
                self.mixer.set_rack_gain(
                    rack,
                    active.starts[rack] + (end - active.starts[rack]) * progress,
                );
            }
        }
        if elapsed < active.transition_samples {
            return;
        }
        let mutes = active.mutes;
        let bypasses = active.bypasses;
        self.active_rack_scene = None;
        for (rack, muted) in mutes.into_iter().enumerate() {
            if let Some(muted) = muted {
                self.mixer.set_rack_muted(rack, muted);
            }
        }
        for (index, entry) in bypasses.into_iter().enumerate() {
            let Some(bypass_enabled) = entry else {
                continue;
            };
            let rack = index / MAX_SLOTS_PER_RACK;
            let slot = index % MAX_SLOTS_PER_RACK;
            let _ = self.automation[rack].push(BlockEvent {
                frame_offset: 0,
                event_type: BLOCK_EVENT_SLOT_BYPASS,
                key: 0,
                value: u8::from(bypass_enabled).into(),
                flags: u32::try_from(slot + 1).unwrap_or(u32::MAX),
            });
        }
    }

    fn collect_midi(&mut self, frames: usize) -> usize {
        self.midi_events.clear();
        let Some(midi) = self.midi.as_mut() else {
            return 0;
        };
        if midi.drain_into(&mut self.midi_events).is_err() {
            return 0;
        }
        let block_end = midi.micros_since_open();
        let block_duration = u64::try_from(frames)
            .unwrap_or(u64::MAX)
            .saturating_mul(1_000_000)
            / u64::from(PRODUCT_SAMPLE_RATE);
        let block_start = block_end.saturating_sub(block_duration);
        let mut midi_count = 0;
        for index in 0..self.wire_midi.len().min(self.midi_events.as_slice().len()) {
            let event = self.midi_events.as_slice()[index];
            let frame_offset = sample_offset_for_timestamp(
                event.timestamp_micros,
                block_start,
                1_000_000.0 / f64::from(PRODUCT_SAMPLE_RATE),
                u32::try_from(frames).unwrap_or(u32::MAX),
            );
            self.wire_midi[index] = MidiEvent {
                frame_offset,
                port: 0,
                data_length: u32::from(event.len),
                data: event.bytes,
                flags: 0,
            };
            match event.bytes[0] & 0xf0 {
                0xb0 if event.len == 3 => {
                    let channel = (event.bytes[0] & 0x0f) + 1;
                    if let Some(target) = self.mappings.target_for(channel, event.bytes[1]) {
                        self.push_parameter(
                            SceneParameterTarget {
                                value: map_controller_to_target(event.bytes[2], target),
                                rack_index: target.rack_index,
                                slot_index: target.slot_index,
                                parameter_id: target.parameter_id,
                            },
                            frame_offset,
                        );
                    }
                }
                0xc0 if event.len >= 2 => {
                    if let Some(scene) = scene_index_for_program(event.bytes[1], self.scenes.len())
                    {
                        self.trigger_scene(scene);
                    }
                }
                _ => {}
            }
            midi_count += 1;
        }
        midi_count
    }

    fn publish_telemetry(&self) {
        for rack in 0..MAX_RACKS {
            if let Some(snapshot) = self.mixer.rack_meter_snapshot(rack) {
                self.telemetry.racks[rack].publish(snapshot);
            }
        }
        self.telemetry
            .output
            .publish(self.mixer.output_meter_snapshot());
        if let Some(dispatcher) = &self.dispatcher {
            for rack in 0..MAX_RACKS {
                if let Some(snapshot) = dispatcher.telemetry(rack) {
                    self.telemetry.completed[rack].store(snapshot.completed, Ordering::Release);
                    self.telemetry.deadline_misses[rack]
                        .store(snapshot.deadline_misses, Ordering::Release);
                    self.telemetry.protocol_rejections[rack]
                        .store(snapshot.protocol_rejections, Ordering::Release);
                }
            }
        }
    }

    fn render_samples(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        frames: usize,
    ) -> RenderDisposition {
        let samples = frames.saturating_mul(2);
        if samples > 512 || input.len() < samples || output.len() < samples {
            output.fill(0.0);
            return RenderDisposition::Silence;
        }
        for events in &mut self.automation {
            events.clear();
        }
        self.service_control();
        let midi_count = self.collect_midi(frames);
        self.service_scene(frames);
        if let Some(dispatcher) = self.dispatcher.as_mut() {
            let deadline = Duration::from_nanos(
                u64::try_from(frames)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(1_000_000_000)
                    .saturating_mul(3)
                    / (u64::from(PRODUCT_SAMPLE_RATE) * 4),
            );
            dispatcher.process_block_with_events(
                input,
                &self.wire_midi[..midi_count],
                &self.automation,
                frames,
                deadline,
            );
            dispatcher.apply_to_mixer(&mut self.mixer);
            let sources = dispatcher.sources();
            if self
                .mixer
                .render_block(input, &sources, output, frames)
                .is_ok()
            {
                self.sample_clock = self
                    .sample_clock
                    .saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));
                self.publish_telemetry();
                return RenderDisposition::Rendered;
            }
        } else if self.mixer.render_block(input, &[], output, frames).is_ok() {
            self.sample_clock = self
                .sample_clock
                .saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));
            self.publish_telemetry();
            return RenderDisposition::Rendered;
        }
        output.fill(0.0);
        RenderDisposition::Silence
    }
}

impl PhaseOneRenderer for ProductRenderer {
    fn render(&mut self, mut output: InterleavedStereoF32<'_>) -> RenderDisposition {
        let frames = output.frames().as_u32() as usize;
        let input = [0.0_f32; 512];
        // Legacy output-only fallback intentionally renders silence at the input boundary.
        self.render_samples(&input, output.samples_mut(), frames)
    }
}

impl DuplexRenderer for ProductRenderer {
    fn render(&mut self, block: DuplexStereoF32<'_>) -> RenderDisposition {
        let DuplexStereoF32 { input, mut output } = block;
        let frames = output.frames().as_u32() as usize;
        self.render_samples(input, output.samples_mut(), frames)
    }
}

/// macOS product audio endpoint: 48 kHz stereo, 128/256-frame `CoreAudio` output.
pub struct MacOsAudioEndpoint {
    pending_renderer: Option<ProductRenderer>,
    output: Option<ActiveOutput<ProductRenderer>>,
    duplex: Option<ActiveDuplex<ProductRenderer>>,
    active_format: Option<AudioFormat>,
    selected_route: Option<AudioRouteConfig>,
    allow_device_reconfiguration: bool,
}

impl MacOsAudioEndpoint {
    /// Creates a stopped endpoint with an empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::with_renderer(ProductRenderer::new(PreparedGraph::empty()))
    }

    /// Creates a stopped endpoint with a caller-configured renderer.
    #[must_use]
    pub fn with_renderer(renderer: ProductRenderer) -> Self {
        Self {
            pending_renderer: Some(renderer),
            output: None,
            duplex: None,
            active_format: None,
            selected_route: None,
            allow_device_reconfiguration: false,
        }
    }

    /// Permits requesting the fixed product sample rate / buffer size on the device.
    #[must_use]
    pub const fn allow_device_reconfiguration(mut self) -> Self {
        self.allow_device_reconfiguration = true;
        self
    }

    /// Returns the explicitly selected duplex route while the endpoint is running.
    #[must_use]
    pub fn selected_route(&self) -> Option<&AudioRouteConfig> {
        self.selected_route.as_ref()
    }

    /// Configures the pending mixer while the endpoint is stopped.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream is already running.
    pub fn with_pending_mixer_mut<R>(
        &mut self,
        configure: impl FnOnce(&mut RealtimeRackMixer) -> R,
    ) -> Result<R, Box<dyn std::error::Error + Send + Sync>> {
        let renderer = self
            .pending_renderer
            .as_mut()
            .ok_or("audio endpoint is running; stop before configuring the mixer")?;
        Ok(configure(renderer.mixer_mut()))
    }
}

impl Default for MacOsAudioEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioEndpoint for MacOsAudioEndpoint {
    fn enumerate_outputs(
        &self,
    ) -> Result<Vec<AudioDeviceInfo>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(enumerate_devices()?
            .into_iter()
            .filter_map(|device| {
                (device.capabilities.max_output_channels >= 2).then_some(device.info)
            })
            .collect())
    }

    fn enumerate_inputs(
        &self,
    ) -> Result<Vec<AudioDeviceInfo>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(enumerate_devices()?
            .into_iter()
            .filter_map(|device| {
                (device.capabilities.max_input_channels >= 2).then_some(device.info)
            })
            .collect())
    }

    fn start_route(
        &mut self,
        route: AudioRouteConfig,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let format = route.format.validate()?;
        if !format.is_product_format() {
            return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                frames: format.max_frames_per_callback,
            }));
        }
        self.stop()?;
        let renderer = self
            .pending_renderer
            .take()
            .unwrap_or_else(|| ProductRenderer::new(PreparedGraph::empty()));
        let duplex =
            ActiveDuplex::start(route.clone(), renderer, self.allow_device_reconfiguration)
                .map_err(core_audio_box)?;
        self.active_format = Some(format);
        self.selected_route = Some(route);
        self.duplex = Some(duplex);
        Ok(())
    }

    fn poll_event(&mut self) -> Option<AudioEndpointEvent> {
        self.duplex.as_mut().and_then(ActiveDuplex::poll_event)
    }

    fn start(
        &mut self,
        format: AudioFormat,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let format = format
            .validate()
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?;
        if !format.is_product_format() {
            return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                frames: format.max_frames_per_callback,
            }));
        }
        if self.output.is_some() || self.duplex.is_some() {
            self.stop()?;
        }
        let frames = match format.max_frames_per_callback {
            128 => PhaseOneFrames::Frames128,
            256 => PhaseOneFrames::Frames256,
            other => {
                return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                    frames: other,
                }));
            }
        };
        let mut config = PhaseOneConfig::new(frames);
        if self.allow_device_reconfiguration {
            config = config.allow_device_reconfiguration();
        }
        let renderer = self
            .pending_renderer
            .take()
            .unwrap_or_else(|| ProductRenderer::new(PreparedGraph::empty()));
        let output = ActiveOutput::start(config, renderer).map_err(core_audio_box)?;
        self.output = Some(output);
        self.active_format = Some(format);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(mut duplex) = self.duplex.take() {
            duplex.stop().map_err(core_audio_box)?;
            self.pending_renderer = Some(ProductRenderer::new(PreparedGraph::empty()));
        }
        if let Some(mut output) = self.output.take() {
            output.stop().map_err(core_audio_box)?;
            // Renderer is dropped with ActiveOutput; restore a fresh pending mixer.
            self.pending_renderer = Some(ProductRenderer::new(PreparedGraph::empty()));
        }
        self.active_format = None;
        self.selected_route = None;
        Ok(())
    }

    fn active_format(&self) -> Option<AudioFormat> {
        self.active_format
    }
}

fn core_audio_box(error: CoreAudioError) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(error)
}

#[cfg(test)]
mod tests {
    use sp_audio_io::{AudioEndpoint, AudioFormat};

    use super::MacOsAudioEndpoint;

    #[test]
    fn enumerates_stereo_capable_outputs() {
        let endpoint = MacOsAudioEndpoint::new();
        // The device list depends on the host hardware, so only the stereo-output filter
        // contract is asserted: enumeration succeeds and every entry is identifiable.
        let devices = endpoint.enumerate_outputs().expect("enumerate");
        for device in devices {
            assert!(!device.name.is_empty());
        }
    }

    #[test]
    fn rejects_non_product_format_without_starting_device() {
        let mut endpoint = MacOsAudioEndpoint::new();
        let error = endpoint
            .start(AudioFormat {
                sample_rate_hz: 44_100,
                channel_count: 2,
                max_frames_per_callback: 128,
            })
            .expect_err("must reject");
        assert!(!error.to_string().is_empty());
        assert!(endpoint.active_format().is_none());
    }
}
