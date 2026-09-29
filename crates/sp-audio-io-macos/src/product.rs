//! Product [`AudioEndpoint`] implementation over the direct AUHAL streams in this crate.

use sp_audio_io::{
    AudioDeviceInfo, AudioEndpoint, AudioEndpointEvent, AudioFormat, AudioFormatError,
    AudioRouteConfig,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use rtrb::{Consumer, Producer, RingBuffer};
use sp_engine::{MixerLane, PreparedGraph, RackMeterSnapshot, RackSettings, RealtimeRackMixer};
use sp_midi::{
    BoundedMidiEvents, MAX_SCENE_PARAMETERS, MidiInput, MidiLearnTable, MidiMappingReceiver,
    MidiMappingTarget, MidirInput, SceneParameterTarget, ScenePlayer, SceneRampBlock,
    map_controller_to_target, sample_offset_for_timestamp, scene_index_for_program,
};
use sp_model::{
    MAX_RACKS, MAX_SLOTS_PER_RACK, PhysicalChannels, SceneParameterTransition, Session,
};
use sp_shared_memory::{BLOCK_EVENT_PARAMETER, BLOCK_EVENT_SLOT_BYPASS, BlockEvent, MidiEvent};
use sp_shared_memory_macos::{MonotonicClock, RackRecoverySignal, SharedMemoryRegion};

use crate::{
    ActiveMultiChannelDuplex, ActiveOutput, BlockFrames, CallbackTelemetry, CoreAudioError,
    InterleavedStereoF32, LaneSource, MultiChannelDuplexF32, MultiChannelDuplexRenderer,
    OutputConfig, OutputRenderer, PreparedRackLane, RenderDisposition, enumerate_devices,
};

const PRODUCT_SAMPLE_RATE: u32 = 48_000;
const PRODUCT_CONTROL_CAPACITY: usize = 64;

fn input_meter(samples: &[f32], frames: usize) -> RackMeterSnapshot {
    let mut peak = [0.0_f32; 2];
    let mut sum_squares = [0.0_f32; 2];
    let mut clipped = false;
    for frame in samples[..frames * 2].chunks_exact(2) {
        for channel in 0..2 {
            let sample = if frame[channel].is_finite() {
                frame[channel]
            } else {
                0.0
            };
            peak[channel] = peak[channel].max(sample.abs());
            sum_squares[channel] += sample * sample;
            clipped |= sample.abs() >= 1.0;
        }
    }
    RackMeterSnapshot {
        peak,
        rms: sum_squares.map(|sum| {
            (sum / f32::from(u16::try_from(frames).expect("bounded callback frame count"))).sqrt()
        }),
        clipped,
    }
}

#[derive(Clone, Copy)]
enum ProductCommand {
    TriggerScene(usize),
    ObserveParameter(SceneParameterTarget),
    SetParameter {
        rack: usize,
        slot: usize,
        parameter_id: u32,
        normalized: f32,
    },
    SetRackGain {
        rack: usize,
        gain: f32,
    },
    SetRackMuted {
        rack: usize,
        muted: bool,
    },
    SetRackBypassed {
        rack: usize,
        bypassed: bool,
    },
    SetRackControls {
        rack: usize,
        gain: f32,
        muted: bool,
        bypassed: bool,
    },
    SetRackLatency {
        rack: usize,
        samples: u32,
    },
}

/// Complete scene tables built off the callback. The callback swaps them in at a block
/// boundary and hands the previous tables back so they are freed on the control thread.
struct SceneSet {
    scenes: Vec<PreparedProductScene>,
    current_parameters: Vec<SceneParameterTarget>,
    // Boxed: the graph and mapping table are large, and the callback stack is small.
    topology: Option<Box<LiveTopology>>,
}

/// A rack layout change applied to a running callback at one block boundary.
struct LiveTopology {
    graph: PreparedGraph,
    mixer_lanes: [MixerLane; MAX_RACKS],
    lane_sources: [LaneSource; MAX_RACKS],
    new_lanes: Vec<PreparedRackLane>,
    /// Filled by the callback with lanes it retired; freed later on the control thread.
    retired: Vec<PreparedRackLane>,
    mappings: MidiLearnTable,
}

/// What processes a rack after a [`ProductControl::publish_topology`] change.
pub enum LaneWorker {
    /// The worker previously at `from` continues without interruption.
    Keep,
    /// A new worker replaces any previous one. The rack plays dry until the new worker
    /// delivers three valid blocks, then fades to it over 16 samples.
    Replace(PreparedRackLane),
    /// No worker: the rack passes latency-matched dry audio.
    Dry,
}

/// One rack position in a [`ProductControl::publish_topology`] change.
pub struct TopologyLane {
    /// Previous rack position that continues here, or `None` for a new rack.
    pub from: Option<usize>,
    /// The worker for this position.
    pub worker: LaneWorker,
    /// Live controls for a new rack.
    pub settings: RackSettings,
    /// Whether the rack may fall back to latency-matched dry audio.
    pub dry_fallback: bool,
}

/// Main-thread endpoint for bounded live engine changes.
pub struct ProductControl {
    producer: Producer<ProductCommand>,
    scene_sets: Producer<Box<SceneSet>>,
    retired_scene_sets: Consumer<Box<SceneSet>>,
    published_scene_sets: u64,
    applied_scene_sets: Arc<AtomicU64>,
}

/// Callback-side endpoint paired with [`ProductControl`].
pub struct ProductControlReceiver {
    consumer: Consumer<ProductCommand>,
    scene_sets: Consumer<Box<SceneSet>>,
    retired_scene_sets: Producer<Box<SceneSet>>,
    applied_scene_sets: Arc<AtomicU64>,
}

impl ProductControlReceiver {
    /// Reports queued control work without consuming it.
    #[must_use]
    pub fn has_pending_command(&self) -> bool {
        !self.consumer.is_empty()
    }
}

impl ProductControl {
    /// Creates a preallocated control/callback pair.
    #[must_use]
    pub fn new() -> (Self, ProductControlReceiver) {
        let (producer, consumer) = RingBuffer::new(PRODUCT_CONTROL_CAPACITY);
        let (scene_producer, scene_consumer) = RingBuffer::new(1);
        let (retired_producer, retired_consumer) = RingBuffer::new(2);
        let applied_scene_sets = Arc::new(AtomicU64::new(0));
        (
            Self {
                producer,
                scene_sets: scene_producer,
                retired_scene_sets: retired_consumer,
                published_scene_sets: 0,
                applied_scene_sets: Arc::clone(&applied_scene_sets),
            },
            ProductControlReceiver {
                consumer,
                scene_sets: scene_consumer,
                retired_scene_sets: retired_producer,
                applied_scene_sets,
            },
        )
    }

    fn push(&mut self, command: ProductCommand) -> bool {
        self.producer.push(command).is_ok()
    }

    /// Replaces the callback's scene tables while audio runs, so scenes can be captured and
    /// edited during a show. Returns false while a previous replacement is still pending.
    pub fn publish_scenes(&mut self, model: &Session) -> bool {
        while self.retired_scene_sets.pop().is_ok() {}
        if self.scene_sets.is_full() {
            return false;
        }
        let scenes = prepare_product_scenes(model);
        let current_parameters = scene_parameter_cache(&scenes, current_product_parameters(model));
        let pushed = self
            .scene_sets
            .push(Box::new(SceneSet {
                scenes,
                current_parameters,
                topology: None,
            }))
            .is_ok();
        self.published_scene_sets += u64::from(pushed);
        pushed
    }

    /// Replaces the running rack layout at one block boundary. Racks that continue keep their
    /// audio, fades, and worker; a rack with a replacement worker fades to dry, then fades to
    /// the new worker over 16 samples once it delivers three valid blocks.
    ///
    /// `lanes` lists the new rack positions in order.
    ///
    /// # Errors
    /// Returns the lanes unchanged while a previous change is still pending.
    pub fn publish_topology(
        &mut self,
        model: &Session,
        graph: PreparedGraph,
        lanes: Vec<TopologyLane>,
    ) -> Result<(), Vec<TopologyLane>> {
        while self.retired_scene_sets.pop().is_ok() {}
        if self.scene_sets.is_full() || lanes.len() > MAX_RACKS {
            return Err(lanes);
        }
        let mut mixer_lanes = [MixerLane::FRESH; MAX_RACKS];
        let mut lane_sources = [LaneSource::Empty; MAX_RACKS];
        let mut new_lanes = Vec::with_capacity(MAX_RACKS);
        for (position, lane) in lanes.into_iter().enumerate() {
            mixer_lanes[position] = MixerLane {
                from: lane.from,
                settings: lane.settings,
                dry_fallback: lane.dry_fallback,
                restart: lane.from.is_some() && !matches!(lane.worker, LaneWorker::Keep),
            };
            lane_sources[position] = match (lane.worker, lane.from) {
                (LaneWorker::Replace(worker), _) => {
                    new_lanes.push(worker);
                    LaneSource::New
                }
                (LaneWorker::Keep, Some(previous)) => LaneSource::Keep(previous),
                (LaneWorker::Keep | LaneWorker::Dry, _) => LaneSource::Empty,
            };
        }
        let scenes = prepare_product_scenes(model);
        let current_parameters = scene_parameter_cache(&scenes, current_product_parameters(model));
        let pushed = self.scene_sets.push(Box::new(SceneSet {
            scenes,
            current_parameters,
            topology: Some(Box::new(LiveTopology {
                graph,
                mixer_lanes,
                lane_sources,
                new_lanes,
                retired: Vec::with_capacity(MAX_RACKS),
                mappings: product_midi_mappings(model),
            })),
        }));
        debug_assert!(pushed.is_ok(), "capacity was checked above");
        self.published_scene_sets += 1;
        Ok(())
    }

    /// Whether the callback has applied every published scene or topology change. Also frees
    /// retired tables and worker lanes here, on the control thread.
    #[must_use]
    pub fn changes_applied(&mut self) -> bool {
        while self.retired_scene_sets.pop().is_ok() {}
        self.applied_scene_sets.load(Ordering::Acquire) == self.published_scene_sets
    }

    /// Requests a scene at the next audio block boundary.
    pub fn trigger_scene(&mut self, scene: usize) -> bool {
        self.push(ProductCommand::TriggerScene(scene))
    }

    /// Queues a normalized plug-in parameter write for the next audio block.
    pub fn set_parameter(
        &mut self,
        rack: usize,
        slot: usize,
        parameter_id: u32,
        normalized: f32,
    ) -> bool {
        if rack >= MAX_RACKS
            || slot >= MAX_SLOTS_PER_RACK
            || !normalized.is_finite()
            || !(0.0..=1.0).contains(&normalized)
        {
            return false;
        }
        self.push(ProductCommand::SetParameter {
            rack,
            slot,
            parameter_id,
            normalized,
        })
    }

    /// Updates the scene's current-value cache without sending automation to the plug-in.
    /// Returns false when the bounded control queue is full or the value is invalid.
    pub fn observe_parameter(
        &mut self,
        rack: usize,
        slot: usize,
        parameter_id: u32,
        normalized: f32,
    ) -> bool {
        if rack >= MAX_RACKS
            || slot >= MAX_SLOTS_PER_RACK
            || !normalized.is_finite()
            || !(0.0..=1.0).contains(&normalized)
        {
            return false;
        }
        self.push(ProductCommand::ObserveParameter(SceneParameterTarget {
            rack_index: rack,
            slot_index: slot,
            parameter_id,
            value: normalized,
        }))
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

    /// Changes a rack's gain, mute, and bypass together at the next block boundary.
    pub fn set_rack_controls(
        &mut self,
        rack: usize,
        gain: f32,
        muted: bool,
        bypassed: bool,
    ) -> bool {
        if rack >= MAX_RACKS || !gain.is_finite() || gain < 0.0 {
            return false;
        }
        self.push(ProductCommand::SetRackControls {
            rack,
            gain,
            muted,
            bypassed,
        })
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

/// One published window waits for one UI consumer; later blocks collect in callback-owned state.
struct UiMeterWindow {
    peak: [AtomicU32; 2],
    clipped: AtomicBool,
    sequence: AtomicU64,
    acknowledged: AtomicU64,
    reading: AtomicBool,
}

impl UiMeterWindow {
    fn new() -> Self {
        Self {
            peak: std::array::from_fn(|_| AtomicU32::new(0)),
            clipped: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            acknowledged: AtomicU64::new(0),
            reading: AtomicBool::new(false),
        }
    }

    fn take(&self, rms: [f32; 2]) -> RackMeterSnapshot {
        let empty = RackMeterSnapshot {
            peak: [0.0; 2],
            rms,
            clipped: false,
        };
        // A second reader must not acknowledge a window while the first is copying it.
        if self.reading.swap(true, Ordering::Acquire) {
            return empty;
        }
        let sequence = self.sequence.load(Ordering::Acquire);
        let snapshot = if sequence == self.acknowledged.load(Ordering::Relaxed) {
            empty
        } else {
            let snapshot = RackMeterSnapshot {
                peak: std::array::from_fn(|channel| {
                    f32::from_bits(self.peak[channel].load(Ordering::Relaxed))
                }),
                rms,
                clipped: self.clipped.load(Ordering::Relaxed),
            };
            self.acknowledged.store(sequence, Ordering::Release);
            snapshot
        };
        self.reading.store(false, Ordering::Release);
        snapshot
    }
}

/// Owned by the audio callback. Each publish performs bounded work, including at most one flush.
struct UiMeterAccumulator {
    peak: [f32; 2],
    clipped: bool,
    sequence: u64,
}

impl UiMeterAccumulator {
    const fn new() -> Self {
        Self {
            peak: [0.0; 2],
            clipped: false,
            sequence: 0,
        }
    }

    fn publish(&mut self, window: &UiMeterWindow, snapshot: RackMeterSnapshot) {
        for channel in 0..2 {
            self.peak[channel] = self.peak[channel].max(snapshot.peak[channel]);
        }
        self.clipped |= snapshot.clipped;
        if window.acknowledged.load(Ordering::Acquire) == self.sequence {
            for channel in 0..2 {
                window.peak[channel].store(self.peak[channel].to_bits(), Ordering::Relaxed);
            }
            window.clipped.store(self.clipped, Ordering::Relaxed);
            self.sequence = self.sequence.wrapping_add(1);
            window.sequence.store(self.sequence, Ordering::Release);
            self.peak = [0.0; 2];
            self.clipped = false;
        }
    }
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
    input_racks: [AtomicProductMeter; MAX_RACKS],
    racks: [AtomicProductMeter; MAX_RACKS],
    output: AtomicProductMeter,
    ui_input_racks: [UiMeterWindow; MAX_RACKS],
    ui_racks: [UiMeterWindow; MAX_RACKS],
    ui_output: UiMeterWindow,
    scene: AtomicU32,
    completed: [AtomicU64; MAX_RACKS],
    deadline_misses: [AtomicU64; MAX_RACKS],
    wake_failures: [AtomicU64; MAX_RACKS],
    max_wake_ticks: [AtomicU64; MAX_RACKS],
    missed_unclaimed: [AtomicU64; MAX_RACKS],
    missed_in_progress: [AtomicU64; MAX_RACKS],
    missed_completed_late: [AtomicU64; MAX_RACKS],
    last_miss_block_index: [AtomicU64; MAX_RACKS],
    last_miss_sequence: [AtomicU64; MAX_RACKS],
    last_miss_request_tick: [AtomicU64; MAX_RACKS],
    last_miss_claimed_tick: [AtomicU64; MAX_RACKS],
    last_miss_observed_tick: [AtomicU64; MAX_RACKS],
    last_miss_worker_phase: [AtomicU64; MAX_RACKS],
    last_miss_worker_wait_sequence: [AtomicU64; MAX_RACKS],
    last_miss_wake_sequence: [AtomicU64; MAX_RACKS],
    last_miss_worker_loop_tick: [AtomicU64; MAX_RACKS],
    protocol_rejections: [AtomicU64; MAX_RACKS],
    fallback_activations: [AtomicU64; MAX_RACKS],
    gate_closed_blocks: [AtomicU64; MAX_RACKS],
    /// Highest callback load since the last take, in permille of the block period plus one.
    /// Zero means no callback has run since.
    callback_load: AtomicU32,
}

/// Latest callback diagnostics for one rack worker.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProductRackDiagnostics {
    /// Valid blocks accepted from the rack worker.
    pub completed: u64,
    /// Blocks that missed the bounded callback deadline.
    pub deadline_misses: u64,
    /// Failed attempts to wake the rack worker.
    pub wake_failures: u64,
    /// Longest worker wake call, in Darwin monotonic ticks.
    pub max_wake_ticks: u64,
    /// Deadline misses before the worker claimed the request.
    pub missed_unclaimed: u64,
    /// Deadline misses after the worker owned or started the request.
    pub missed_in_progress: u64,
    /// Deadline misses with a completion visible after the final sweep.
    pub missed_completed_late: u64,
    /// Callback block index of the most recent miss.
    pub last_miss_block_index: u64,
    /// Request sequence of the most recent miss.
    pub last_miss_sequence: u64,
    /// Monotonic publication tick for the most recent miss.
    pub last_miss_request_tick: u64,
    /// Monotonic worker claim tick for the most recent miss, or zero.
    pub last_miss_claimed_tick: u64,
    /// Monotonic callback observation tick for the most recent miss.
    pub last_miss_observed_tick: u64,
    /// Worker loop phase at the last miss (0 scan, 1 control, 2 editor, 3 wait).
    pub last_miss_worker_phase: u64,
    /// Wake sequence last observed by the worker at the last miss.
    pub last_miss_worker_wait_sequence: u64,
    /// Producer wake sequence at the last miss.
    pub last_miss_wake_sequence: u64,
    /// Most recent worker loop start at the last miss.
    pub last_miss_worker_loop_tick: u64,
    /// Malformed or invalid shared-memory observations.
    pub protocol_rejections: u64,
    /// Transitions to rack-local fallback.
    pub fallback_activations: u64,
    /// Callback blocks where the rack gate was closed.
    pub gate_closed_blocks: u64,
}

impl ProductTelemetry {
    fn new() -> Self {
        Self {
            input_racks: std::array::from_fn(|_| AtomicProductMeter::new()),
            racks: std::array::from_fn(|_| AtomicProductMeter::new()),
            output: AtomicProductMeter::new(),
            ui_input_racks: std::array::from_fn(|_| UiMeterWindow::new()),
            ui_racks: std::array::from_fn(|_| UiMeterWindow::new()),
            ui_output: UiMeterWindow::new(),
            scene: AtomicU32::new(u32::MAX),
            completed: std::array::from_fn(|_| AtomicU64::new(0)),
            deadline_misses: std::array::from_fn(|_| AtomicU64::new(0)),
            wake_failures: std::array::from_fn(|_| AtomicU64::new(0)),
            max_wake_ticks: std::array::from_fn(|_| AtomicU64::new(0)),
            missed_unclaimed: std::array::from_fn(|_| AtomicU64::new(0)),
            missed_in_progress: std::array::from_fn(|_| AtomicU64::new(0)),
            missed_completed_late: std::array::from_fn(|_| AtomicU64::new(0)),
            last_miss_block_index: std::array::from_fn(|_| AtomicU64::new(0)),
            last_miss_sequence: std::array::from_fn(|_| AtomicU64::new(0)),
            last_miss_request_tick: std::array::from_fn(|_| AtomicU64::new(0)),
            last_miss_claimed_tick: std::array::from_fn(|_| AtomicU64::new(0)),
            last_miss_observed_tick: std::array::from_fn(|_| AtomicU64::new(0)),
            last_miss_worker_phase: std::array::from_fn(|_| AtomicU64::new(0)),
            last_miss_worker_wait_sequence: std::array::from_fn(|_| AtomicU64::new(0)),
            last_miss_wake_sequence: std::array::from_fn(|_| AtomicU64::new(0)),
            last_miss_worker_loop_tick: std::array::from_fn(|_| AtomicU64::new(0)),
            protocol_rejections: std::array::from_fn(|_| AtomicU64::new(0)),
            fallback_activations: std::array::from_fn(|_| AtomicU64::new(0)),
            gate_closed_blocks: std::array::from_fn(|_| AtomicU64::new(0)),
            callback_load: AtomicU32::new(0),
        }
    }

    /// Highest callback load since the last call, as a fraction of the block period (0.0..), or
    /// None if no callback ran since.
    #[must_use]
    pub fn take_callback_load(&self) -> Option<f32> {
        let load = self.callback_load.swap(0, Ordering::Relaxed);
        #[allow(
            clippy::cast_precision_loss,
            reason = "permille values stay far below f32 integer precision"
        )]
        (load != 0).then(|| (load - 1) as f32 / 1000.0)
    }

    /// Returns the latest pre-processing input meter for one rack.
    #[must_use]
    pub fn rack_input_meter(&self, rack: usize) -> Option<RackMeterSnapshot> {
        self.input_racks.get(rack).map(AtomicProductMeter::snapshot)
    }

    /// Takes accumulated input peaks and clipping; RMS is from the latest block.
    /// Use one logical UI reader. A concurrent read returns no peak, and a pending
    /// window may become available on the next audio callback.
    #[must_use]
    pub fn take_rack_input_meter(&self, rack: usize) -> Option<RackMeterSnapshot> {
        Some(
            self.ui_input_racks
                .get(rack)?
                .take(self.input_racks[rack].snapshot().rms),
        )
    }

    /// Returns the latest post-processing output meter for one rack.
    #[must_use]
    pub fn rack_output_meter(&self, rack: usize) -> Option<RackMeterSnapshot> {
        self.racks.get(rack).map(AtomicProductMeter::snapshot)
    }

    /// Takes accumulated rack-output peaks and clipping; RMS is from the latest block.
    /// Use one logical UI reader. A concurrent read returns no peak, and a pending
    /// window may become available on the next audio callback.
    #[must_use]
    pub fn take_rack_output_meter(&self, rack: usize) -> Option<RackMeterSnapshot> {
        Some(
            self.ui_racks
                .get(rack)?
                .take(self.racks[rack].snapshot().rms),
        )
    }

    /// Returns the latest post-processing output meter for one rack.
    #[must_use]
    pub fn rack_meter(&self, rack: usize) -> Option<RackMeterSnapshot> {
        self.rack_output_meter(rack)
    }

    /// Returns the latest final-output meter.
    #[must_use]
    pub fn output_meter(&self) -> RackMeterSnapshot {
        self.output.snapshot()
    }

    /// Takes accumulated final-output peaks and clipping; RMS is from the latest block.
    /// Use one logical UI reader. A concurrent read returns no peak, and a pending
    /// window may become available on the next audio callback.
    #[must_use]
    pub fn take_output_meter(&self) -> RackMeterSnapshot {
        self.ui_output.take(self.output.snapshot().rms)
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
            wake_failures: self.wake_failures[rack].load(Ordering::Acquire),
            max_wake_ticks: self.max_wake_ticks[rack].load(Ordering::Acquire),
            missed_unclaimed: self.missed_unclaimed[rack].load(Ordering::Acquire),
            missed_in_progress: self.missed_in_progress[rack].load(Ordering::Acquire),
            missed_completed_late: self.missed_completed_late[rack].load(Ordering::Acquire),
            last_miss_block_index: self.last_miss_block_index[rack].load(Ordering::Acquire),
            last_miss_sequence: self.last_miss_sequence[rack].load(Ordering::Acquire),
            last_miss_request_tick: self.last_miss_request_tick[rack].load(Ordering::Acquire),
            last_miss_claimed_tick: self.last_miss_claimed_tick[rack].load(Ordering::Acquire),
            last_miss_observed_tick: self.last_miss_observed_tick[rack].load(Ordering::Acquire),
            last_miss_worker_phase: self.last_miss_worker_phase[rack].load(Ordering::Acquire),
            last_miss_worker_wait_sequence: self.last_miss_worker_wait_sequence[rack]
                .load(Ordering::Acquire),
            last_miss_wake_sequence: self.last_miss_wake_sequence[rack].load(Ordering::Acquire),
            last_miss_worker_loop_tick: self.last_miss_worker_loop_tick[rack]
                .load(Ordering::Acquire),
            protocol_rejections: self.protocol_rejections[rack].load(Ordering::Acquire),
            fallback_activations: self.fallback_activations[rack].load(Ordering::Acquire),
            gate_closed_blocks: self.gate_closed_blocks[rack].load(Ordering::Acquire),
        })
    }
}

/// Model identifiers resolved to fixed numeric realtime scene targets.
#[derive(Clone)]
pub struct PreparedProductScene {
    parameters: [SceneParameterTarget; MAX_SCENE_PARAMETERS],
    parameter_steps: [bool; MAX_SCENE_PARAMETERS],
    parameter_count: usize,
    gains: [Option<f32>; MAX_RACKS],
    mutes: [Option<bool>; MAX_RACKS],
    rack_bypasses: [Option<bool>; MAX_RACKS],
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
            parameter_steps: [false; MAX_SCENE_PARAMETERS],
            parameter_count: 0,
            gains: [None; MAX_RACKS],
            mutes: [None; MAX_RACKS],
            rack_bypasses: [None; MAX_RACKS],
            bypasses: [None; MAX_RACKS * MAX_SLOTS_PER_RACK],
            transition_ms,
        }
    }

    fn parameters(&self) -> &[SceneParameterTarget] {
        &self.parameters[..self.parameter_count]
    }

    fn parameter_steps(&self) -> &[bool] {
        &self.parameter_steps[..self.parameter_count]
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
            for bypass in &scene.rack_bypasses {
                if let Some(rack) = model
                    .racks
                    .iter()
                    .position(|rack| rack.id == bypass.rack_id)
                {
                    prepared.rack_bypasses[rack] = Some(bypass.bypassed);
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
                prepared.parameter_steps[prepared.parameter_count] =
                    parameter.transition == SceneParameterTransition::Step;
                prepared.parameter_count += 1;
            }
            prepared
        })
        .collect()
}

/// Resolves persisted normalized values for scene interpolation before audio starts.
#[must_use]
pub fn current_product_parameters(model: &Session) -> Vec<SceneParameterTarget> {
    let mut seen = BTreeSet::new();
    let mut current = Vec::new();
    for scene in &model.scenes {
        for parameter in scene.parameter_values.iter().take(MAX_SCENE_PARAMETERS) {
            let Some((rack, slot)) = resolve_slot(model, &parameter.rack_id, &parameter.slot_id)
            else {
                continue;
            };
            let Ok(parameter_id) = parameter.parameter_id.0.parse() else {
                continue;
            };
            if !seen.insert((rack, slot, parameter_id)) {
                continue;
            }
            if let Some(value) = model.racks[rack].slots[slot]
                .parameters
                .values
                .get(&parameter.parameter_id)
            {
                current.push(SceneParameterTarget {
                    rack_index: rack,
                    slot_index: slot,
                    parameter_id,
                    value: value.get(),
                });
            }
        }
    }
    current.sort_unstable_by_key(|target| scene_parameter_key(*target));
    current
}

/// Reserves a sorted cache entry for every scene target. NaN marks unknown plug-in state;
/// `ScenePlayer` applies those targets only at scene commit.
fn scene_parameter_cache(
    scenes: &[PreparedProductScene],
    current: Vec<SceneParameterTarget>,
) -> Vec<SceneParameterTarget> {
    let mut cache: BTreeMap<_, _> = current
        .into_iter()
        .map(|target| (scene_parameter_key(target), target))
        .collect();
    for scene in scenes {
        for &target in scene.parameters() {
            cache
                .entry(scene_parameter_key(target))
                .or_insert(SceneParameterTarget {
                    value: f32::NAN,
                    ..target
                });
        }
    }
    cache.into_values().collect()
}

const fn scene_parameter_key(target: SceneParameterTarget) -> (usize, usize, u32) {
    (target.rack_index, target.slot_index, target.parameter_id)
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
    rack_bypasses: [Option<bool>; MAX_RACKS],
    bypasses: [Option<bool>; MAX_RACKS * MAX_SLOTS_PER_RACK],
}

/// One value per rack, built on the heap: 64 racks of event and audio buffers are too large to
/// build on a stack.
#[allow(
    clippy::unnecessary_box_returns,
    reason = "the box is the point: the array never passes through a stack"
)]
fn per_rack<T: Clone>(value: T) -> Box<[T; MAX_RACKS]> {
    vec![value; MAX_RACKS]
        .into_boxed_slice()
        .try_into()
        .unwrap_or_else(|_| unreachable!("the vector holds exactly MAX_RACKS values"))
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
    automation: Box<[crate::RackAutomationEvents; MAX_RACKS]>,
    control: Option<ProductControlReceiver>,
    pending_control: Option<ProductCommand>,
    mapping_receiver: Option<MidiMappingReceiver>,
    mappings: MidiLearnTable,
    scenes: Vec<PreparedProductScene>,
    // A replaced scene set waits here if the return queue is full; it is never freed here.
    retired_scene_set: Option<Box<SceneSet>>,
    scene_player: ScenePlayer,
    scene_block: SceneRampBlock,
    pending_scene_parameters: [Option<SceneParameterTarget>; MAX_SCENE_PARAMETERS],
    pending_scene_parameter_count: usize,
    current_parameters: Vec<SceneParameterTarget>,
    active_rack_scene: Option<ActiveRackScene>,
    pending_scene_bypasses: [Option<bool>; MAX_RACKS * MAX_SLOTS_PER_RACK],
    sample_clock: u64,
    telemetry: Arc<ProductTelemetry>,
    ui_input_meters: [UiMeterAccumulator; MAX_RACKS],
    ui_rack_meters: [UiMeterAccumulator; MAX_RACKS],
    ui_output_meter: UiMeterAccumulator,
    rack_inputs: Box<[[f32; sp_engine::MAX_MIX_FRAMES * 2]; MAX_RACKS]>,
    /// Times each callback for [`ProductTelemetry::take_callback_load`].
    load_clock: Option<MonotonicClock>,
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
            automation: per_rack(crate::RackAutomationEvents::new()),
            control: None,
            pending_control: None,
            mapping_receiver: None,
            mappings: MidiLearnTable::default(),
            scenes: Vec::new(),
            retired_scene_set: None,
            scene_player: ScenePlayer::new(),
            scene_block: SceneRampBlock::new(),
            pending_scene_parameters: [None; MAX_SCENE_PARAMETERS],
            pending_scene_parameter_count: 0,
            current_parameters: Vec::new(),
            active_rack_scene: None,
            pending_scene_bypasses: [None; MAX_RACKS * MAX_SLOTS_PER_RACK],
            sample_clock: 0,
            telemetry: Arc::new(ProductTelemetry::new()),
            ui_input_meters: std::array::from_fn(|_| UiMeterAccumulator::new()),
            ui_rack_meters: std::array::from_fn(|_| UiMeterAccumulator::new()),
            ui_output_meter: UiMeterAccumulator::new(),
            rack_inputs: per_rack([0.0; sp_engine::MAX_MIX_FRAMES * 2]),
            load_clock: MonotonicClock::new().ok(),
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
        banks: Vec<(
            usize,
            [SharedMemoryRegion; 2],
            usize,
            Arc<RackRecoverySignal>,
        )>,
    ) -> io::Result<Self> {
        let dispatcher = crate::RackSharedMemoryDispatcher::with_rack_banks(banks)?;
        Ok(Self {
            mixer: RealtimeRackMixer::new(graph),
            dispatcher: Some(dispatcher),
            midi: None,
            midi_events: BoundedMidiEvents::new(),
            wire_midi: [MidiEvent::default(); sp_shared_memory::MAX_MIDI_EVENTS],
            automation: per_rack(crate::RackAutomationEvents::new()),
            control: None,
            pending_control: None,
            mapping_receiver: None,
            mappings: MidiLearnTable::default(),
            scenes: Vec::new(),
            retired_scene_set: None,
            scene_player: ScenePlayer::new(),
            scene_block: SceneRampBlock::new(),
            pending_scene_parameters: [None; MAX_SCENE_PARAMETERS],
            pending_scene_parameter_count: 0,
            current_parameters: Vec::new(),
            active_rack_scene: None,
            pending_scene_bypasses: [None; MAX_RACKS * MAX_SLOTS_PER_RACK],
            sample_clock: 0,
            telemetry: Arc::new(ProductTelemetry::new()),
            ui_input_meters: std::array::from_fn(|_| UiMeterAccumulator::new()),
            ui_rack_meters: std::array::from_fn(|_| UiMeterAccumulator::new()),
            ui_output_meter: UiMeterAccumulator::new(),
            rack_inputs: per_rack([0.0; sp_engine::MAX_MIX_FRAMES * 2]),
            load_clock: MonotonicClock::new().ok(),
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
        mut current_parameters: Vec<SceneParameterTarget>,
    ) -> Self {
        current_parameters = scene_parameter_cache(&scenes, current_parameters);
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
        self.receive_scene_set();
        for _ in 0..PRODUCT_CONTROL_CAPACITY {
            let Some(command) = self.pending_control.take().or_else(|| {
                self.control
                    .as_mut()
                    .and_then(|control| control.consumer.pop().ok())
            }) else {
                break;
            };
            match command {
                ProductCommand::TriggerScene(scene) => self.trigger_scene(scene),
                ProductCommand::ObserveParameter(target) => {
                    if let Ok(index) = self
                        .current_parameters
                        .binary_search_by_key(&scene_parameter_key(target), |current| {
                            scene_parameter_key(*current)
                        })
                    {
                        self.current_parameters[index].value = target.value;
                    }
                }
                ProductCommand::SetParameter {
                    rack,
                    slot,
                    parameter_id,
                    normalized,
                } => {
                    if !self.push_live_parameter(
                        SceneParameterTarget {
                            rack_index: rack,
                            slot_index: slot,
                            parameter_id,
                            value: normalized,
                        },
                        0,
                    ) {
                        self.pending_control = Some(command);
                        break;
                    }
                }
                ProductCommand::SetRackGain { rack, gain } => {
                    self.mixer.set_rack_gain(rack, gain);
                }
                ProductCommand::SetRackMuted { rack, muted } => {
                    self.mixer.set_rack_muted(rack, muted);
                }
                ProductCommand::SetRackBypassed { rack, bypassed } => {
                    self.mixer.set_rack_bypassed(rack, bypassed);
                }
                ProductCommand::SetRackControls {
                    rack,
                    gain,
                    muted,
                    bypassed,
                } => {
                    if let Some(settings) = self.mixer.rack_settings(rack) {
                        self.mixer.set_rack_settings(
                            rack,
                            RackSettings {
                                gain,
                                muted,
                                bypassed,
                                ..settings
                            },
                        );
                    }
                }
                ProductCommand::SetRackLatency { rack, samples } => {
                    self.mixer
                        .set_dry_delay_frames(rack, usize::try_from(samples).unwrap_or(usize::MAX));
                }
            }
        }
    }

    /// Installs a published scene set without allocating or freeing on the callback.
    fn receive_scene_set(&mut self) {
        let Some(control) = self.control.as_mut() else {
            return;
        };
        if let Some(retired) = self.retired_scene_set.take()
            && let Err(rtrb::PushError::Full(retired)) = control.retired_scene_sets.push(retired)
        {
            self.retired_scene_set = Some(retired);
            return;
        }
        let Ok(mut set) = control.scene_sets.pop() else {
            return;
        };
        control.applied_scene_sets.fetch_add(1, Ordering::Release);
        // Keep values the callback has observed since the control thread built its cache.
        for target in &mut set.current_parameters {
            if let Ok(index) = self
                .current_parameters
                .binary_search_by_key(&scene_parameter_key(*target), |current| {
                    scene_parameter_key(*current)
                })
                && self.current_parameters[index].value.is_finite()
            {
                target.value = self.current_parameters[index].value;
            }
        }
        if let Some(topology) = set.topology.as_mut() {
            self.apply_topology(topology);
        }
        std::mem::swap(&mut self.scenes, &mut set.scenes);
        std::mem::swap(&mut self.current_parameters, &mut set.current_parameters);
        let control = self.control.as_mut().expect("checked above");
        if let Err(rtrb::PushError::Full(set)) = control.retired_scene_sets.push(set) {
            self.retired_scene_set = Some(set);
        }
    }

    /// Moves per-rack callback state to the new layout without allocating: buffers are swapped,
    /// and new lanes move into capacity reserved on the control thread.
    fn apply_topology(&mut self, topology: &mut LiveTopology) {
        self.mixer
            .apply_topology(topology.graph, &topology.mixer_lanes);
        if let Some(dispatcher) = self.dispatcher.as_mut() {
            dispatcher.apply_topology(
                &topology.lane_sources,
                &mut topology.new_lanes,
                &mut topology.retired,
            );
        }
        std::mem::swap(&mut self.mappings, &mut topology.mappings);
        // Scene ramps address racks by position; settle them on the new layout at once.
        self.active_rack_scene = None;
        self.scene_player = ScenePlayer::new();
        self.pending_scene_parameters.fill(None);
        self.pending_scene_parameter_count = 0;
        self.pending_scene_bypasses.fill(None);
        let from = sp_engine::lane_permutation(&topology.mixer_lanes);
        sp_engine::permute_lanes(&mut self.ui_input_meters, &from);
        sp_engine::permute_lanes(&mut self.ui_rack_meters, &from);
    }

    fn trigger_scene(&mut self, scene_index: usize) {
        let Some(scene) = self.scenes.get(scene_index) else {
            return;
        };
        self.telemetry.scene.store(
            u32::try_from(scene_index).unwrap_or(u32::MAX),
            Ordering::Release,
        );
        self.pending_scene_parameters.fill(None);
        self.pending_scene_parameter_count = 0;
        self.pending_scene_bypasses.fill(None);
        self.scene_player.trigger_targets_sorted_with_steps(
            scene.parameters(),
            scene.parameter_steps(),
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
        self.active_rack_scene = Some(ActiveRackScene {
            start_sample: self.sample_clock,
            transition_samples: u64::from(scene.transition_ms)
                .saturating_mul(u64::from(PRODUCT_SAMPLE_RATE))
                .div_ceil(1_000)
                .max(1),
            starts,
            gains: scene.gains,
            mutes: scene.mutes,
            rack_bypasses: scene.rack_bypasses,
            bypasses: scene.bypasses,
        });
    }

    fn push_parameter(&mut self, target: SceneParameterTarget, frame_offset: u32) -> bool {
        let Some(events) = self.automation.get_mut(target.rack_index) else {
            return false;
        };
        if !events.push(BlockEvent {
            frame_offset,
            event_type: BLOCK_EVENT_PARAMETER,
            key: target.parameter_id,
            value: target.value.clamp(0.0, 1.0),
            flags: u32::try_from(target.slot_index.saturating_add(1)).unwrap_or(u32::MAX),
        }) {
            return false;
        }
        if let Ok(index) = self
            .current_parameters
            .binary_search_by_key(&scene_parameter_key(target), |current| {
                scene_parameter_key(*current)
            })
        {
            self.current_parameters[index].value = target.value;
        }
        true
    }

    fn push_live_parameter(&mut self, target: SceneParameterTarget, frame_offset: u32) -> bool {
        if !self.push_parameter(target, frame_offset) {
            return false;
        }
        if self.pending_scene_parameter_count == 0 {
            return true;
        }
        for pending in &mut self.pending_scene_parameters {
            if (*pending).is_some_and(|value| {
                value.rack_index == target.rack_index
                    && value.slot_index == target.slot_index
                    && value.parameter_id == target.parameter_id
            }) {
                *pending = None;
                self.pending_scene_parameter_count -= 1;
            }
        }
        true
    }

    fn service_scene(&mut self, frames: usize) {
        self.scene_player.render_block(
            self.sample_clock,
            u32::try_from(frames).unwrap_or(u32::MAX),
            &mut self.scene_block,
        );
        let scene_block = self.scene_block.clone();
        for (index, &target) in scene_block.targets().iter().enumerate() {
            let accepted = self.push_parameter(target, 0);
            if self.pending_scene_parameters[index].is_some() {
                self.pending_scene_parameter_count -= 1;
            }
            self.pending_scene_parameters[index] = (!accepted).then_some(target);
            self.pending_scene_parameter_count += usize::from(!accepted);
        }
        if !self.scene_player.is_active() && self.pending_scene_parameter_count > 0 {
            self.flush_scene_parameters();
        }

        let Some(active) = self.active_rack_scene.as_ref() else {
            self.flush_scene_bypasses();
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
        if elapsed < active.transition_samples
            || self.scene_player.is_active()
            || self.pending_scene_parameter_count > 0
        {
            self.flush_scene_bypasses();
            return;
        }
        let mutes = active.mutes;
        let rack_bypasses = active.rack_bypasses;
        let bypasses = active.bypasses;
        self.active_rack_scene = None;
        for (rack, muted) in mutes.into_iter().enumerate() {
            if let Some(muted) = muted {
                self.mixer.set_rack_muted(rack, muted);
            }
        }
        for (rack, bypassed) in rack_bypasses.into_iter().enumerate() {
            if let Some(bypassed) = bypassed {
                self.mixer.set_rack_bypassed(rack, bypassed);
            }
        }
        for (index, entry) in bypasses.into_iter().enumerate() {
            if entry.is_some() {
                self.pending_scene_bypasses[index] = entry;
            }
        }
        self.flush_scene_bypasses();
    }

    fn flush_scene_parameters(&mut self) {
        for index in 0..MAX_SCENE_PARAMETERS {
            if let Some(target) = self.pending_scene_parameters[index]
                && self.push_parameter(target, 0)
            {
                self.pending_scene_parameters[index] = None;
                self.pending_scene_parameter_count -= 1;
            }
        }
    }

    fn flush_scene_bypasses(&mut self) {
        if self.scene_player.is_active() || self.pending_scene_parameter_count > 0 {
            return;
        }
        for (index, entry) in self.pending_scene_bypasses.iter_mut().enumerate() {
            let Some(bypass_enabled) = *entry else {
                continue;
            };
            let rack = index / MAX_SLOTS_PER_RACK;
            let slot = index % MAX_SLOTS_PER_RACK;
            if self.automation[rack].push(BlockEvent {
                frame_offset: 0,
                event_type: BLOCK_EVENT_SLOT_BYPASS,
                key: 0,
                value: u8::from(bypass_enabled).into(),
                flags: u32::try_from(slot + 1).unwrap_or(u32::MAX),
            }) {
                *entry = None;
            }
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
            if !self.handle_midi_event(event, frame_offset) {
                self.wire_midi[midi_count] = MidiEvent {
                    frame_offset,
                    port: 0,
                    data_length: u32::from(event.len),
                    data: event.bytes,
                    flags: 0,
                };
                midi_count += 1;
            }
        }
        midi_count
    }

    fn handle_midi_event(&mut self, event: sp_midi::MidiEvent, frame_offset: u32) -> bool {
        match event.bytes[0] & 0xf0 {
            0xb0 if event.len == 3 => {
                let channel = (event.bytes[0] & 0x0f) + 1;
                if let Some(target) = self.mappings.target_for(channel, event.bytes[1]) {
                    self.push_live_parameter(
                        SceneParameterTarget {
                            value: map_controller_to_target(event.bytes[2], target),
                            rack_index: target.rack_index,
                            slot_index: target.slot_index,
                            parameter_id: target.parameter_id,
                        },
                        frame_offset,
                    );
                }
                false
            }
            0xc0 if event.len >= 2 => {
                if let Some(scene) = scene_index_for_program(event.bytes[1], self.scenes.len()) {
                    self.trigger_scene(scene);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    fn publish_telemetry(&mut self) {
        for rack in 0..MAX_RACKS {
            if let Some(snapshot) = self.mixer.rack_meter_snapshot(rack) {
                self.telemetry.racks[rack].publish(snapshot);
                self.ui_rack_meters[rack].publish(&self.telemetry.ui_racks[rack], snapshot);
            }
        }
        let output_snapshot = self.mixer.output_meter_snapshot();
        self.telemetry.output.publish(output_snapshot);
        self.ui_output_meter
            .publish(&self.telemetry.ui_output, output_snapshot);
        if let Some(dispatcher) = &self.dispatcher {
            for rack in 0..MAX_RACKS {
                if let Some(snapshot) = dispatcher.telemetry(rack) {
                    self.telemetry.completed[rack].store(snapshot.completed, Ordering::Release);
                    self.telemetry.wake_failures[rack]
                        .store(snapshot.wake_failures, Ordering::Release);
                    self.telemetry.max_wake_ticks[rack]
                        .store(snapshot.max_wake_ticks, Ordering::Release);
                    if snapshot.deadline_misses
                        != self.telemetry.deadline_misses[rack].load(Ordering::Relaxed)
                    {
                        self.telemetry.missed_unclaimed[rack]
                            .store(snapshot.missed_unclaimed, Ordering::Relaxed);
                        self.telemetry.missed_in_progress[rack]
                            .store(snapshot.missed_in_progress, Ordering::Relaxed);
                        self.telemetry.missed_completed_late[rack]
                            .store(snapshot.missed_completed_late, Ordering::Relaxed);
                        self.telemetry.last_miss_block_index[rack]
                            .store(snapshot.last_miss_block_index, Ordering::Relaxed);
                        self.telemetry.last_miss_sequence[rack]
                            .store(snapshot.last_miss_sequence, Ordering::Relaxed);
                        self.telemetry.last_miss_request_tick[rack]
                            .store(snapshot.last_miss_request_tick, Ordering::Relaxed);
                        self.telemetry.last_miss_claimed_tick[rack]
                            .store(snapshot.last_miss_claimed_tick, Ordering::Relaxed);
                        self.telemetry.last_miss_observed_tick[rack]
                            .store(snapshot.last_miss_observed_tick, Ordering::Relaxed);
                        self.telemetry.last_miss_worker_phase[rack]
                            .store(snapshot.last_miss_worker_phase, Ordering::Relaxed);
                        self.telemetry.last_miss_worker_wait_sequence[rack]
                            .store(snapshot.last_miss_worker_wait_sequence, Ordering::Relaxed);
                        self.telemetry.last_miss_wake_sequence[rack]
                            .store(snapshot.last_miss_wake_sequence, Ordering::Relaxed);
                        self.telemetry.last_miss_worker_loop_tick[rack]
                            .store(snapshot.last_miss_worker_loop_tick, Ordering::Relaxed);
                        self.telemetry.deadline_misses[rack]
                            .store(snapshot.deadline_misses, Ordering::Release);
                    }
                    self.telemetry.protocol_rejections[rack]
                        .store(snapshot.protocol_rejections, Ordering::Release);
                    self.telemetry.fallback_activations[rack]
                        .store(snapshot.fallback_activations, Ordering::Release);
                    self.telemetry.gate_closed_blocks[rack]
                        .store(snapshot.gate_closed_blocks, Ordering::Release);
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
        for rack in 0..self.mixer.prepared_graph().rack_count() {
            let snapshot = input_meter(input, frames);
            self.telemetry.input_racks[rack].publish(snapshot);
            self.ui_input_meters[rack].publish(&self.telemetry.ui_input_racks[rack], snapshot);
        }
        for events in self.automation.iter_mut() {
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
                Some(&self.mixer.sidechain_sources(input, 2)),
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

    fn render_multichannel(
        &mut self,
        input: &[f32],
        input_channels: usize,
        output: &mut [f32],
        output_channels: usize,
        frames: usize,
    ) -> RenderDisposition {
        if frames == 0
            || frames > sp_engine::MAX_MIX_FRAMES
            || input.len() < frames.saturating_mul(input_channels)
            || output.len() < frames.saturating_mul(output_channels)
            || self
                .mixer
                .validate_routes(input_channels, output_channels)
                .is_err()
        {
            output.fill(0.0);
            return RenderDisposition::Silence;
        }
        let graph = *self.mixer.prepared_graph();
        for rack in 0..graph.rack_count() {
            let destination = &mut self.rack_inputs[rack][..frames * 2];
            destination.fill(0.0);
            match graph
                .rack(rack)
                .and_then(sp_engine::PreparedRack::input_channels)
            {
                Some(PhysicalChannels::Mono { channel }) => {
                    for (frame, stereo) in destination.chunks_exact_mut(2).enumerate() {
                        let sample = input[frame * input_channels + usize::from(channel)];
                        stereo.copy_from_slice(&[sample, sample]);
                    }
                }
                Some(PhysicalChannels::Stereo { left, right }) => {
                    for (frame, stereo) in destination.chunks_exact_mut(2).enumerate() {
                        let offset = frame * input_channels;
                        stereo[0] = input[offset + usize::from(left)];
                        stereo[1] = input[offset + usize::from(right)];
                    }
                }
                None => {}
            }
            let snapshot = input_meter(destination, frames);
            self.telemetry.input_racks[rack].publish(snapshot);
            self.ui_input_meters[rack].publish(&self.telemetry.ui_input_racks[rack], snapshot);
        }
        for events in self.automation.iter_mut() {
            events.clear();
        }
        self.service_control();
        let midi_count = self.collect_midi(frames);
        self.service_scene(frames);
        let sources = if let Some(dispatcher) = self.dispatcher.as_mut() {
            let deadline = Duration::from_nanos(
                u64::try_from(frames)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(1_000_000_000)
                    .saturating_mul(3)
                    / (u64::from(PRODUCT_SAMPLE_RATE) * 4),
            );
            dispatcher.process_block_with_rack_inputs(
                &self.rack_inputs,
                Some(&self.mixer.sidechain_sources(input, input_channels)),
                &self.wire_midi[..midi_count],
                &self.automation,
                frames,
                deadline,
            );
            dispatcher.apply_to_mixer(&mut self.mixer);
            dispatcher.sources()
        } else {
            [sp_engine::RackAudioSource::None; MAX_RACKS]
        };
        if self
            .mixer
            .render_routed_block(&self.rack_inputs, &sources, output, output_channels, frames)
            .is_ok()
        {
            self.sample_clock = self
                .sample_clock
                .saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));
            self.publish_telemetry();
            RenderDisposition::Rendered
        } else {
            output.fill(0.0);
            RenderDisposition::Silence
        }
    }
}

impl ProductRenderer {
    /// Runs one callback render and publishes its duration as a fraction of the block period.
    /// Two continuous-clock reads and integer arithmetic; nothing allocates or waits.
    fn timed_render(
        &mut self,
        frames: usize,
        render: impl FnOnce(&mut Self) -> RenderDisposition,
    ) -> RenderDisposition {
        let Some(clock) = self.load_clock else {
            return render(self);
        };
        let started = clock.now_ticks();
        let disposition = render(self);
        let elapsed = clock.ticks_to_duration(clock.now_ticks().saturating_sub(started));
        // 1000 × elapsed / (frames / rate) = elapsed_ns × rate / (frames × 1_000_000).
        let divisor = u64::try_from(frames)
            .unwrap_or(u64::MAX)
            .max(1)
            .saturating_mul(1_000_000);
        let permille = u64::try_from(elapsed.as_nanos())
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(PRODUCT_SAMPLE_RATE))
            / divisor;
        self.telemetry.callback_load.fetch_max(
            u32::try_from(permille.saturating_add(1)).unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
        disposition
    }
}

impl OutputRenderer for ProductRenderer {
    fn render(&mut self, mut output: InterleavedStereoF32<'_>) -> RenderDisposition {
        let frames = output.frames().as_u32() as usize;
        let input = [0.0_f32; 512];
        // Legacy output-only fallback intentionally renders silence at the input boundary.
        self.timed_render(frames, |renderer| {
            renderer.render_samples(&input, output.samples_mut(), frames)
        })
    }
}

impl MultiChannelDuplexRenderer for ProductRenderer {
    fn render(&mut self, block: MultiChannelDuplexF32<'_>) -> RenderDisposition {
        let (input, output, input_channels, output_channels, frames) = block.into_parts();
        let frames = frames.as_u32() as usize;
        self.timed_render(frames, |renderer| {
            renderer.render_multichannel(
                input,
                usize::from(input_channels),
                output,
                usize::from(output_channels),
                frames,
            )
        })
    }
}

/// macOS product audio endpoint: 48 kHz, 1–64 physical output channels.
pub struct MacOsAudioEndpoint {
    pending_renderer: Option<Box<ProductRenderer>>,
    output: Option<ActiveOutput<ProductRenderer>>,
    duplex: Option<ActiveMultiChannelDuplex<ProductRenderer>>,
    active_format: Option<AudioFormat>,
    selected_route: Option<AudioRouteConfig>,
    retirement_failed: bool,
    allow_device_reconfiguration: bool,
}

/// Temporary proof that an endpoint has no native callback and owns its stopped renderer.
///
/// The borrow prevents restarting the endpoint while this proof is in use. Recovery identities
/// distinguish an old retained rack from a newly loaded worker at the same rack index.
pub struct StoppedRecoveryAccess<'endpoint> {
    attached: [Option<Arc<RackRecoverySignal>>; MAX_RACKS],
    endpoint: std::marker::PhantomData<&'endpoint mut MacOsAudioEndpoint>,
}

impl StoppedRecoveryAccess<'_> {
    /// Whether the retained dispatcher, rather than the app, owns this exact recovery signal.
    #[must_use]
    pub fn owns(&self, rack_index: usize, signal: &Arc<RackRecoverySignal>) -> bool {
        self.attached
            .get(rack_index)
            .and_then(Option::as_ref)
            .is_some_and(|attached| Arc::ptr_eq(attached, signal))
    }
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
            pending_renderer: Some(Box::new(renderer)),
            output: None,
            duplex: None,
            active_format: None,
            selected_route: None,
            retirement_failed: false,
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

    /// Whether native cleanup failed and this endpoint must be replaced.
    ///
    /// A faulted endpoint has no safely usable active format. Native storage may still be
    /// alive; it is retained instead of risking a callback into freed renderer memory.
    #[must_use]
    pub const fn is_faulted(&self) -> bool {
        self.retirement_failed
    }

    /// Services worker bank handoffs without audio after proven callback retirement.
    ///
    /// Returns a borrowed proof of the recovery identities still owned by the retained dispatcher.
    /// A caller may handle unattached workers separately, but must not acknowledge owned handoffs.
    /// Returns `None` while audio is active or native cleanup is uncertain. This never renders
    /// a block, starts a device, or discards the retained graph and control receiver.
    pub fn service_stopped_recoveries(&mut self) -> Option<StoppedRecoveryAccess<'_>> {
        if self.retirement_failed || self.output.is_some() || self.duplex.is_some() {
            return None;
        }
        let renderer = self.pending_renderer.as_mut()?;
        let attached = renderer.dispatcher.as_mut().map_or_else(
            || std::array::from_fn(|_| None),
            crate::RackSharedMemoryDispatcher::service_stopped_recoveries,
        );
        Some(StoppedRecoveryAccess {
            attached,
            endpoint: std::marker::PhantomData,
        })
    }

    /// Returns coherent callback counters while a stream is running.
    #[must_use]
    pub fn callback_telemetry(&self) -> Option<CallbackTelemetry> {
        self.duplex
            .as_ref()
            .map(ActiveMultiChannelDuplex::telemetry)
            .or_else(|| self.output.as_ref().map(ActiveOutput::telemetry))
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
                (device.capabilities.max_output_channels > 0).then_some(device.info)
            })
            .collect())
    }

    fn enumerate_inputs(
        &self,
    ) -> Result<Vec<AudioDeviceInfo>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(enumerate_devices()?
            .into_iter()
            .filter_map(|device| {
                (device.capabilities.max_input_channels > 0).then_some(device.info)
            })
            .collect())
    }

    fn start_route(
        &mut self,
        route: AudioRouteConfig,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let format = route.format.validate()?;
        if format.sample_rate_hz != PRODUCT_SAMPLE_RATE {
            return Err(format!("product audio requires {PRODUCT_SAMPLE_RATE} Hz").into());
        }
        if format.channel_count > 64 {
            return Err(format!(
                "product audio supports at most 64 output channels; requested {}",
                format.channel_count
            )
            .into());
        }
        if !matches!(format.max_frames_per_callback, 32 | 64 | 128 | 256) {
            return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                frames: format.max_frames_per_callback,
            }));
        }
        let devices = enumerate_devices().map_err(core_audio_box)?;
        let output_device = devices
            .iter()
            .find(|device| device.info.id == route.output)
            .ok_or_else(|| format!("selected output device {} was not found", route.output))?;
        if format.channel_count > output_device.capabilities.max_output_channels {
            return Err(format!(
                "selected output device {} has {} channels, but {} were requested",
                route.output, output_device.capabilities.max_output_channels, format.channel_count
            )
            .into());
        }
        let input_channels = if let Some(input_id) = &route.input {
            devices
                .iter()
                .find(|device| &device.info.id == input_id)
                .ok_or_else(|| format!("selected input device {input_id} was not found"))?
                .capabilities
                .max_input_channels
                .min(64)
        } else {
            0
        };
        if let Some(renderer) = self.pending_renderer.as_ref() {
            renderer.mixer.validate_routes(
                usize::from(input_channels),
                usize::from(format.channel_count),
            )?;
        }
        self.stop()?;
        let pending = self.pending_renderer.as_ref().ok_or(
            "audio renderer could not be safely recovered; rebuild the endpoint before starting",
        )?;
        pending.mixer.validate_routes(
            usize::from(input_channels),
            usize::from(format.channel_count),
        )?;
        let renderer = self
            .pending_renderer
            .take()
            .expect("pending renderer validated");
        let duplex = match ActiveMultiChannelDuplex::start_boxed(
            route.clone(),
            renderer,
            self.allow_device_reconfiguration,
        ) {
            Ok(duplex) => duplex,
            Err((error, renderer)) => {
                self.retirement_failed = renderer.is_none();
                self.pending_renderer = renderer;
                return Err(core_audio_box(error));
            }
        };
        self.active_format = Some(format);
        self.selected_route = Some(route);
        self.duplex = Some(duplex);
        Ok(())
    }

    fn poll_event(&mut self) -> Option<AudioEndpointEvent> {
        self.duplex
            .as_mut()
            .and_then(ActiveMultiChannelDuplex::poll_event)
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
            32 => BlockFrames::Frames32,
            64 => BlockFrames::Frames64,
            128 => BlockFrames::Frames128,
            256 => BlockFrames::Frames256,
            other => {
                return Err(Box::new(AudioFormatError::UnsupportedProductFrames {
                    frames: other,
                }));
            }
        };
        let mut config = OutputConfig::new(frames);
        if self.allow_device_reconfiguration {
            config = config.allow_device_reconfiguration();
        }
        let renderer = self.pending_renderer.take().ok_or(
            "audio renderer could not be safely recovered; rebuild the endpoint before starting",
        )?;
        let output = match ActiveOutput::start_boxed(config, renderer) {
            Ok(output) => output,
            Err((error, renderer)) => {
                self.retirement_failed = renderer.is_none();
                self.pending_renderer = renderer;
                return Err(core_audio_box(error));
            }
        };
        self.output = Some(output);
        self.active_format = Some(format);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.retirement_failed {
            return Err("native audio cleanup failed; this endpoint cannot be reused".into());
        }
        self.active_format = None;
        self.selected_route = None;
        if let Some(mut duplex) = self.duplex.take() {
            match duplex.stop_and_take_renderer() {
                Ok(renderer) => self.pending_renderer = renderer,
                Err(error) => {
                    self.retirement_failed = true;
                    return Err(core_audio_box(error));
                }
            }
        }
        if let Some(mut output) = self.output.take() {
            match output.stop_and_take_renderer() {
                Ok(renderer) => self.pending_renderer = renderer,
                Err(error) => {
                    self.retirement_failed = true;
                    return Err(core_audio_box(error));
                }
            }
        }
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
    use std::sync::atomic::Ordering;

    use sp_audio_io::{AudioEndpoint, AudioFormat};
    use sp_engine::{PreparedGraph, RackMeterSnapshot};
    use sp_midi::{
        MAX_SCENE_PARAMETERS, MidiLearnTable, MidiMappingPublisher, SceneParameterTarget,
    };
    use sp_model::{
        ChannelLayout, Endpoint, EndpointId, MAX_RACKS, MAX_SLOTS_PER_RACK, NormalizedParameters,
        NormalizedValue, ParameterId, PhysicalChannels, PluginDescriptor, PluginFingerprint,
        PluginIdentity, PluginInstanceId, PluginSlot, Rack, RackBypass, RackChannelRoute, RackId,
        RackTopology, Scene, SceneId, SceneParameterTransition, SceneParameterValue, Session,
        SlotSidechain, Source, SourceId,
    };
    use sp_shared_memory::{
        BLOCK_EVENT_PARAMETER, BLOCK_EVENT_SLOT_BYPASS, BlockEvent, MAX_EVENTS,
    };
    use sp_shared_memory_macos::SharedMemoryRegion;

    use super::{
        MacOsAudioEndpoint, PreparedProductScene, ProductControl, ProductRenderer,
        UiMeterAccumulator, UiMeterWindow,
    };
    use crate::{
        BlockFrames, MultiChannelDuplexF32, MultiChannelDuplexRenderer, RackSharedMemoryDispatcher,
    };

    fn meter(peak: f32, clipped: bool) -> RackMeterSnapshot {
        RackMeterSnapshot {
            peak: [peak; 2],
            rms: [peak; 2],
            clipped,
        }
    }

    fn sparse_scene_session(transition: SceneParameterTransition) -> Session {
        let mut session = Session::new();
        let rack_id = RackId("rack".into());
        let slot_id = PluginInstanceId("slot".into());
        let mut parameters = NormalizedParameters::default();
        for index in 0..300 {
            parameters.values.insert(
                ParameterId(index.to_string()),
                NormalizedValue::new(if index == 299 { 0.6 } else { 0.2 }).unwrap(),
            );
        }
        session.racks.push(Rack {
            id: rack_id.clone(),
            name: "Rack".into(),
            topology: RackTopology::Serial,
            source_id: SourceId("input".into()),
            endpoint_id: EndpointId("output".into()),
            gain_db: sp_model::GainDb::default(),
            muted: false,
            bypassed: false,
            slots: vec![PluginSlot {
                id: slot_id.clone(),
                bypassed: false,
                parameters,
                plugin: PluginDescriptor {
                    identity: PluginIdentity::default(),
                    fingerprint: PluginFingerprint {
                        algorithm: String::new(),
                        digest: String::new(),
                        plugin_version: String::new(),
                    },
                },
                sidechain: None,
            }],
        });
        session.scenes.push(Scene {
            id: SceneId("scene".into()),
            name: "Scene".into(),
            gains: Vec::new(),
            mutes: Vec::new(),
            rack_bypasses: vec![RackBypass {
                rack_id: rack_id.clone(),
                bypassed: true,
            }],
            bypasses: Vec::new(),
            parameter_values: vec![SceneParameterValue {
                rack_id,
                slot_id,
                parameter_id: ParameterId("299".into()),
                value: NormalizedValue::new(1.0).unwrap(),
                transition,
            }],
            transition_ms: 10,
        });
        session
    }

    #[test]
    fn ui_meter_keeps_transient_through_silence_and_resets_after_take() {
        let window = UiMeterWindow::new();
        let mut accumulator = UiMeterAccumulator::new();
        accumulator.publish(&window, meter(0.1, false));
        accumulator.publish(&window, meter(1.2, true));
        accumulator.publish(&window, meter(0.0, false));

        assert_eq!(
            window.take([0.0; 2]).peak.map(f32::to_bits),
            [0.1_f32.to_bits(); 2]
        );
        accumulator.publish(&window, meter(0.0, false));
        let captured = window.take([0.0; 2]);
        assert_eq!(captured.peak.map(f32::to_bits), [1.2_f32.to_bits(); 2]);
        assert!(captured.clipped);
        assert_eq!(
            window.take([0.0; 2]).peak.map(f32::to_bits),
            [0.0_f32.to_bits(); 2]
        );

        accumulator.publish(&window, meter(0.6, false));
        let next = window.take([0.0; 2]);
        assert_eq!(next.peak.map(f32::to_bits), [0.6_f32.to_bits(); 2]);
        assert!(!next.clipped);
    }

    #[test]
    fn ui_meter_peak_arriving_between_read_and_ack_is_not_lost() {
        let window = UiMeterWindow::new();
        let mut accumulator = UiMeterAccumulator::new();
        accumulator.publish(&window, meter(0.2, false));

        let sequence = window.sequence.load(Ordering::Acquire);
        assert_eq!(window.peak[0].load(Ordering::Relaxed), 0.2_f32.to_bits());
        accumulator.publish(&window, meter(1.1, true));
        window.acknowledged.store(sequence, Ordering::Release);
        accumulator.publish(&window, meter(0.0, false));

        let captured = window.take([0.0; 2]);
        assert_eq!(captured.peak.map(f32::to_bits), [1.1_f32.to_bits(); 2]);
        assert!(captured.clipped);
    }

    #[test]
    fn concurrent_ui_read_does_not_acknowledge_another_readers_window() {
        let window = UiMeterWindow::new();
        let mut accumulator = UiMeterAccumulator::new();
        accumulator.publish(&window, meter(1.1, true));

        window.reading.store(true, Ordering::Release);
        assert_eq!(
            window.take([0.0; 2]).peak.map(f32::to_bits),
            [0.0_f32.to_bits(); 2]
        );
        assert_eq!(window.acknowledged.load(Ordering::Acquire), 0);
        window.reading.store(false, Ordering::Release);

        let captured = window.take([0.0; 2]);
        assert_eq!(captured.peak.map(f32::to_bits), [1.1_f32.to_bits(); 2]);
        assert!(captured.clipped);
    }

    #[test]
    fn multichannel_renderer_gathers_each_rack_input_and_meters_pre_processing() {
        let mut session = Session::new();
        session.sources.push(Source {
            id: SourceId("in".into()),
            name: "In".into(),
            layout: ChannelLayout::Stereo,
        });
        session.endpoints.push(Endpoint {
            id: EndpointId("out".into()),
            name: "Out".into(),
            layout: ChannelLayout::Stereo,
        });
        for (index, (input_pair, output_pair)) in
            [((0, 1), (0, 1)), ((2, 3), (2, 3))].into_iter().enumerate()
        {
            let id = RackId(format!("rack-{index}"));
            session.racks.push(Rack {
                id: id.clone(),
                name: format!("Rack {index}"),
                topology: RackTopology::Serial,
                source_id: SourceId("in".into()),
                endpoint_id: EndpointId("out".into()),
                gain_db: sp_model::GainDb::default(),
                muted: false,
                bypassed: false,
                slots: Vec::new(),
            });
            session.rack_routes.insert(
                id,
                RackChannelRoute {
                    input: Some(PhysicalChannels::Stereo {
                        left: input_pair.0,
                        right: input_pair.1,
                    }),
                    output: PhysicalChannels::Stereo {
                        left: output_pair.0,
                        right: output_pair.1,
                    },
                },
            );
        }
        let graph = PreparedGraph::compile(&session).unwrap();
        let mut renderer = ProductRenderer::new(graph);
        renderer.mixer.set_rack_bypassed(0, true);
        renderer.mixer.set_rack_bypassed(1, true);
        let input = [0.25, 0.5, 0.75, 1.0].repeat(32);
        let mut output = [0.0; 32 * 4];
        for _ in 0..3 {
            renderer.render_multichannel(&input, 4, &mut output, 4, 32);
        }
        assert_eq!(&output[..4], &[0.25, 0.5, 0.75, 1.0]);
        assert_eq!(
            renderer
                .telemetry
                .rack_input_meter(0)
                .unwrap()
                .peak
                .map(f32::to_bits),
            [0.25_f32.to_bits(), 0.5_f32.to_bits()]
        );
        assert_eq!(
            renderer
                .telemetry
                .rack_input_meter(1)
                .unwrap()
                .peak
                .map(f32::to_bits),
            [0.75_f32.to_bits(), 1.0_f32.to_bits()]
        );
        assert_eq!(
            renderer
                .telemetry
                .rack_output_meter(0)
                .unwrap()
                .peak
                .map(f32::to_bits),
            [0.25_f32.to_bits(), 0.5_f32.to_bits()]
        );
    }

    /// The callback copies a physical sidechain into the aux region of only the slot that
    /// declares it, marks that slot in the request, and reports its own load.
    #[test]
    fn product_callback_publishes_slot_sidechains_and_reports_its_load() {
        let region = match SharedMemoryRegion::create(1) {
            Ok(region) => region,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("region: {error}"),
        };
        let bank = SharedMemoryRegion::open(region.name()).expect("inspection mapping");
        let mut session = Session::new();
        session.sources.push(Source {
            id: SourceId("in".into()),
            name: "In".into(),
            layout: ChannelLayout::Stereo,
        });
        session.endpoints.push(Endpoint {
            id: EndpointId("out".into()),
            name: "Out".into(),
            layout: ChannelLayout::Stereo,
        });
        let slot = |id: &str, sidechain| PluginSlot {
            id: PluginInstanceId(id.into()),
            plugin: PluginDescriptor {
                identity: PluginIdentity {
                    vendor: "Vendor".into(),
                    name: "Compressor".into(),
                    unique_id: "class".into(),
                },
                fingerprint: PluginFingerprint {
                    algorithm: "sha256".into(),
                    digest: "digest".into(),
                    plugin_version: "1".into(),
                },
            },
            bypassed: false,
            parameters: NormalizedParameters::default(),
            sidechain,
        };
        session.racks.push(Rack {
            id: RackId("rack".into()),
            name: "Rack".into(),
            topology: RackTopology::Serial,
            source_id: SourceId("in".into()),
            endpoint_id: EndpointId("out".into()),
            gain_db: sp_model::GainDb::default(),
            muted: false,
            bypassed: false,
            slots: vec![
                slot("eq", None),
                slot(
                    "compressor",
                    Some(SlotSidechain::PhysicalInput(PhysicalChannels::Stereo {
                        left: 2,
                        right: 3,
                    })),
                ),
            ],
        });
        let mut renderer = ProductRenderer::new(PreparedGraph::compile(&session).unwrap());
        renderer.dispatcher =
            Some(RackSharedMemoryDispatcher::new(vec![region]).expect("dispatcher"));
        let input = [0.25, -0.25, 0.5, -0.5].repeat(32);
        let mut output = [0.0; 32 * 2];
        MultiChannelDuplexRenderer::render(
            &mut renderer,
            MultiChannelDuplexF32 {
                input: &input,
                output: &mut output,
                input_channels: 4,
                output_channels: 2,
                frames: BlockFrames::Frames32,
            },
        );

        let request = bank.bank().slot(0).expect("first block slot");
        assert_eq!(request.metadata.sidechain_slots, 0b10);
        assert_eq!(request.input_audio[0][..32], [0.25; 32]);
        assert_eq!(request.sidechain_audio[1][0][..32], [0.5; 32]);
        assert_eq!(request.sidechain_audio[1][1][..32], [-0.5; 32]);
        assert!(renderer.telemetry.take_callback_load().is_some());
        assert_eq!(renderer.telemetry.take_callback_load(), None);
    }

    #[test]
    fn observed_parameter_updates_scene_baseline_without_automation_echo() {
        let (mut control, receiver) = ProductControl::new();
        let mut renderer = ProductRenderer::new(PreparedGraph::empty());
        renderer.control = Some(receiver);
        renderer.current_parameters.push(SceneParameterTarget {
            rack_index: 0,
            slot_index: 0,
            parameter_id: 42,
            value: 0.1,
        });
        assert!(control.observe_parameter(0, 0, 42, 0.75));
        assert!(!control.observe_parameter(0, 0, 42, f32::NAN));
        renderer.service_control();
        assert_eq!(
            renderer.current_parameters[0].value.to_bits(),
            0.75_f32.to_bits()
        );
        assert!(renderer.automation[0].as_slice().is_empty());
        let mut scene = PreparedProductScene::empty(100);
        scene.parameters[0] = SceneParameterTarget {
            rack_index: 0,
            slot_index: 0,
            parameter_id: 42,
            value: 1.0,
        };
        scene.parameter_count = 1;
        renderer.scenes.push(scene);
        renderer.trigger_scene(0);
        renderer.service_scene(64);
        assert_eq!(
            renderer.automation[0].as_slice()[0].value.to_bits(),
            0.75_f32.to_bits()
        );
    }

    #[test]
    fn endpoint_stop_preserves_renderer_state_and_control_channel() {
        let (mut control, receiver) = ProductControl::new();
        let mut renderer = ProductRenderer::new(PreparedGraph::empty());
        renderer.control = Some(receiver);
        renderer.sample_clock = 48_000;
        renderer.mixer.set_rack_gain(0, 0.4);
        let output = crate::ActiveOutput::start_test(
            crate::OutputConfig::new(crate::BlockFrames::Frames64),
            renderer,
            crate::TestFakeConfig::new(crate::BlockFrames::Frames64),
            std::sync::Arc::new(crate::TestHook::new()),
        )
        .expect("fake output starts");
        let pointer = std::ptr::from_ref(output.renderer.as_ref().unwrap().as_ref());
        let mut endpoint = MacOsAudioEndpoint::new();
        endpoint.pending_renderer = None;
        endpoint.output = Some(output);
        endpoint.active_format = Some(AudioFormat::product_stereo(64).unwrap());
        endpoint.stop().expect("native callback retires");
        assert!(endpoint.active_format().is_none());
        let access = endpoint
            .service_stopped_recoveries()
            .expect("retirement proof");
        assert!(!access.owns(
            0,
            &std::sync::Arc::new(sp_shared_memory_macos::RackRecoverySignal::new())
        ));
        let renderer = endpoint
            .pending_renderer
            .as_mut()
            .expect("original renderer returned");
        assert_eq!(std::ptr::from_ref(renderer.as_ref()), pointer);
        assert_eq!(renderer.sample_clock, 48_000);
        assert_eq!(
            renderer.mixer.rack_settings(0).unwrap().gain.to_bits(),
            0.4_f32.to_bits()
        );
        assert!(control.set_rack_gain(0, 0.8));
        renderer.service_control();
        assert_eq!(
            renderer.mixer.rack_settings(0).unwrap().gain.to_bits(),
            0.8_f32.to_bits()
        );
        endpoint
            .stop()
            .expect("repeated stop preserves pending renderer");
        assert_eq!(
            std::ptr::from_ref(endpoint.pending_renderer.as_ref().unwrap().as_ref()),
            pointer
        );
    }

    #[test]
    fn fresh_renderer_preserves_recovered_bank_index_and_rejects_unfinished_handoff() {
        use sp_shared_memory_macos::{MappedBankLifecycle, RackRecoverySignal, SharedMemoryRegion};

        for unfinished in [false, true] {
            let [bank_zero, bank_one] = match [
                SharedMemoryRegion::create(11),
                SharedMemoryRegion::create(21),
            ] {
                [Ok(first), Ok(second)] => [first, second],
                [Err(error), _] | [_, Err(error)]
                    if error.kind() == std::io::ErrorKind::PermissionDenied =>
                {
                    return;
                }
                [Err(error), _] | [_, Err(error)] => panic!("mapped banks: {error}"),
            };
            let recovery = std::sync::Arc::new(RackRecoverySignal::new());
            if unfinished {
                assert!(recovery.request_quiesce());
            }
            let result = ProductRenderer::with_rack_banks(
                PreparedGraph::empty(),
                vec![(
                    3,
                    [bank_zero, bank_one],
                    1,
                    std::sync::Arc::clone(&recovery),
                )],
            );
            if unfinished {
                let Err(error) = result else {
                    panic!("unfinished handoff must not create a second bank selector");
                };
                assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                continue;
            }
            let renderer = result.expect("recovered renderer");
            let metadata = renderer
                .dispatcher
                .as_ref()
                .unwrap()
                .bank_metadata(3)
                .unwrap();
            assert_eq!(metadata[0].identity.generation, 11);
            assert_eq!(metadata[0].lifecycle, MappedBankLifecycle::Inactive);
            assert_eq!(metadata[1].identity.generation, 21);
            assert_eq!(metadata[1].lifecycle, MappedBankLifecycle::Active);
            let mut endpoint = MacOsAudioEndpoint::with_renderer(renderer);
            let access = endpoint
                .service_stopped_recoveries()
                .expect("stopped proof");
            assert!(access.owns(3, &recovery));
            let new_worker = std::sync::Arc::new(RackRecoverySignal::new());
            assert!(!access.owns(3, &new_worker));
        }
    }

    #[test]
    fn endpoint_failed_retirement_stays_faulted() {
        let output = crate::ActiveOutput::start_test(
            crate::OutputConfig::new(crate::BlockFrames::Frames64),
            ProductRenderer::new(PreparedGraph::empty()),
            crate::TestFakeConfig::new(crate::BlockFrames::Frames64)
                .fail_cleanup(crate::CoreAudioOperation::Stop, -70),
            std::sync::Arc::new(crate::TestHook::new()),
        )
        .expect("fake output starts");
        let mut endpoint = MacOsAudioEndpoint::new();
        endpoint.pending_renderer = None;
        endpoint.output = Some(output);
        endpoint.active_format = Some(AudioFormat::product_stereo(64).unwrap());
        assert!(endpoint.stop().is_err());
        assert!(endpoint.is_faulted());
        assert!(endpoint.service_stopped_recoveries().is_none());
        assert!(endpoint.active_format().is_none());
        assert!(endpoint.pending_renderer.is_none());
        assert!(endpoint.stop().is_err());
        assert!(
            endpoint
                .start(AudioFormat::product_stereo(64).unwrap())
                .is_err()
        );
    }

    #[test]
    fn full_automation_defers_parameter_writes_in_order() {
        let (mut control, receiver) = ProductControl::new();
        assert!(control.set_parameter(0, 0, 42, 0.25));
        assert!(control.set_parameter(0, 0, 42, 0.75));
        let mut renderer = ProductRenderer::new(PreparedGraph::empty());
        renderer.control = Some(receiver);
        renderer.current_parameters.push(SceneParameterTarget {
            rack_index: 0,
            slot_index: 0,
            parameter_id: 42,
            value: 0.0,
        });
        for _ in 0..MAX_EVENTS {
            assert!(renderer.automation[0].push(BlockEvent::default()));
        }

        renderer.service_control();
        assert!(renderer.pending_control.is_some());
        assert_eq!(
            renderer.current_parameters[0].value.to_bits(),
            0.0_f32.to_bits()
        );

        renderer.automation[0].clear();
        renderer.service_control();
        let events = renderer.automation[0].as_slice();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].value.to_bits(), 0.25_f32.to_bits());
        assert_eq!(events[1].value.to_bits(), 0.75_f32.to_bits());
        assert!(renderer.pending_control.is_none());
        assert_eq!(
            renderer.current_parameters[0].value.to_bits(),
            0.75_f32.to_bits()
        );

        renderer.automation[0].clear();
        renderer.service_control();
        assert!(renderer.automation[0].as_slice().is_empty());
    }

    #[test]
    fn published_scenes_install_at_a_block_boundary_and_keep_observed_values() {
        let model = sparse_scene_session(SceneParameterTransition::Ramp);
        let (mut control, receiver) = ProductControl::new();
        let (_, mapping) = MidiMappingPublisher::new();
        let mut renderer = ProductRenderer::new(PreparedGraph::empty()).with_live_control(
            receiver,
            mapping,
            MidiLearnTable::default(),
            Vec::new(),
            Vec::new(),
        );
        assert!(control.observe_parameter(0, 0, 299, 0.4));
        renderer.service_control();

        assert!(control.publish_scenes(&model));
        assert!(
            !control.publish_scenes(&model),
            "one replacement may be pending"
        );
        renderer.service_control();
        assert_eq!(renderer.scenes.len(), 1);
        assert_eq!(renderer.current_parameters.len(), 1);

        // The value the callback observed before the swap must survive it.
        assert!(control.observe_parameter(0, 0, 299, 0.3));
        renderer.service_control();
        assert!(control.publish_scenes(&model));
        renderer.service_control();
        assert_eq!(
            renderer.current_parameters[0].value.to_bits(),
            0.3_f32.to_bits()
        );
        renderer.trigger_scene(0);
        renderer.service_scene(128);
        assert_eq!(renderer.automation[0].as_slice()[0].key, 299);
        assert_eq!(
            renderer.automation[0].as_slice()[0].value.to_bits(),
            0.3_f32.to_bits()
        );
    }

    #[test]
    fn rack_controls_are_one_all_or_nothing_queue_entry() {
        let (mut control, receiver) = ProductControl::new();
        let mut renderer = ProductRenderer::new(PreparedGraph::empty());
        renderer.control = Some(receiver);
        let initial = renderer
            .mixer
            .rack_settings(0)
            .expect("fixed rack settings");

        let mut queued = 0;
        while control.trigger_scene(usize::MAX) {
            queued += 1;
            assert!(queued <= super::PRODUCT_CONTROL_CAPACITY);
        }
        assert!(queued > 0);
        assert!(!control.set_rack_controls(0, 0.5, true, true));
        renderer.service_control();
        assert_eq!(renderer.mixer.rack_settings(0), Some(initial));

        assert!(control.set_rack_controls(0, 0.5, true, true));
        renderer.service_control();
        let applied = renderer
            .mixer
            .rack_settings(0)
            .expect("fixed rack settings");
        assert_eq!(applied.gain.to_bits(), 0.5_f32.to_bits());
        assert!(applied.muted);
        assert!(applied.bypassed);
        assert_eq!(applied.latency_frames, initial.latency_frames);
    }

    #[test]
    fn only_bound_program_change_is_consumed_by_host_scene() {
        let mut renderer = ProductRenderer::new(PreparedGraph::empty());
        renderer.scenes = vec![PreparedProductScene::empty(0)];
        let unbound = sp_midi::MidiEvent::from_bytes(0, &[0xc0, 2]).unwrap();
        assert!(!renderer.handle_midi_event(unbound, 0));
        assert!(!renderer.scene_player.is_active());
        let bound = sp_midi::MidiEvent::from_bytes(0, &[0xc0, 0]).unwrap();
        assert!(renderer.handle_midi_event(bound, 0));
        assert!(renderer.scene_player.is_active());
    }

    #[test]
    fn sparse_scene_uses_actual_saved_value_and_commits_rack_bypass() {
        let model = sparse_scene_session(SceneParameterTransition::Ramp);
        let current = super::current_product_parameters(&model);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].parameter_id, 299);
        assert_eq!(current[0].value.to_bits(), 0.6_f32.to_bits());
        let prepared = super::prepare_product_scenes(&model);
        assert_eq!(prepared[0].rack_bypasses[0], Some(true));
        let (_, control) = ProductControl::new();
        let (_, mapping) = MidiMappingPublisher::new();
        let mut renderer = ProductRenderer::new(PreparedGraph::empty()).with_live_control(
            control,
            mapping,
            MidiLearnTable::default(),
            prepared,
            current,
        );
        renderer.trigger_scene(0);
        renderer.service_scene(128);
        assert_eq!(renderer.automation[0].as_slice()[0].key, 299);
        assert_eq!(
            renderer.automation[0].as_slice()[0].value.to_bits(),
            0.6_f32.to_bits()
        );
        assert!(!renderer.mixer.rack_settings(0).unwrap().bypassed);
        renderer.sample_clock = 480;
        renderer.automation[0].clear();
        renderer.service_scene(128);
        assert_eq!(
            renderer.automation[0].as_slice()[0].value.to_bits(),
            1.0_f32.to_bits()
        );
        assert!(renderer.mixer.rack_settings(0).unwrap().bypassed);
    }

    #[test]
    fn prepared_step_scene_emits_only_at_commit() {
        let model = sparse_scene_session(SceneParameterTransition::Step);
        let prepared = super::prepare_product_scenes(&model);
        assert_eq!(prepared[0].parameter_steps(), &[true]);
        let (_, control) = ProductControl::new();
        let (_, mapping) = MidiMappingPublisher::new();
        let mut renderer = ProductRenderer::new(PreparedGraph::empty()).with_live_control(
            control,
            mapping,
            MidiLearnTable::default(),
            prepared,
            super::current_product_parameters(&model),
        );
        renderer.trigger_scene(0);
        renderer.service_scene(128);
        assert!(renderer.automation[0].as_slice().is_empty());
        renderer.sample_clock = 384;
        renderer.service_scene(128);
        assert!(renderer.automation[0].as_slice().is_empty());
        assert!(!renderer.mixer.rack_settings(0).unwrap().bypassed);
        renderer.sample_clock = 480;
        renderer.service_scene(128);
        assert_eq!(renderer.automation[0].as_slice().len(), 1);
        assert_eq!(
            renderer.automation[0].as_slice()[0].value.to_bits(),
            1.0_f32.to_bits()
        );
        assert!(renderer.mixer.rack_settings(0).unwrap().bypassed);
    }

    #[test]
    fn live_parameter_write_becomes_bounded_event_and_scene_start() {
        let target = SceneParameterTarget {
            rack_index: 0,
            slot_index: 1,
            parameter_id: 42,
            value: 1.0,
        };
        let mut scene = PreparedProductScene::empty(1_000);
        scene.parameters[0] = target;
        scene.parameter_count = 1;
        let (mut control, receiver) = ProductControl::new();
        assert!(!control.set_parameter(MAX_RACKS, 1, 42, 0.75));
        assert!(!control.set_parameter(0, MAX_SLOTS_PER_RACK, 42, 0.75));
        assert!(!control.set_parameter(0, 1, 42, f32::NAN));
        assert!(!control.set_parameter(0, 1, 42, 1.1));
        assert!(control.set_parameter(0, 1, 42, 0.75));
        assert!(control.trigger_scene(0));

        let (_mapping_publisher, mapping_receiver) = MidiMappingPublisher::new();
        let mut renderer = ProductRenderer::new(PreparedGraph::empty()).with_live_control(
            receiver,
            mapping_receiver,
            MidiLearnTable::default(),
            vec![scene],
            Vec::new(),
        );
        assert!(renderer.current_parameters[0].value.is_nan());
        renderer.service_control();
        assert_eq!(
            renderer.current_parameters[0].value.to_bits(),
            0.75_f32.to_bits()
        );
        let write = renderer.automation[0].as_slice()[0];
        assert_eq!(write.event_type, BLOCK_EVENT_PARAMETER);
        assert_eq!(write.key, 42);
        assert_eq!(write.flags, 2);
        assert_eq!(write.value.to_bits(), 0.75_f32.to_bits());

        renderer.service_scene(128);
        let scene_start = renderer.automation[0].as_slice()[1];
        assert_eq!(scene_start.value.to_bits(), 0.75_f32.to_bits());
    }

    #[test]
    fn full_scene_delivers_final_parameters_and_bypass_for_snap_and_ramp() {
        assert_eq!(MAX_SCENE_PARAMETERS, MAX_EVENTS);
        for transition_ms in [0, 10] {
            let mut scene = PreparedProductScene::empty(transition_ms);
            for (index, target) in scene.parameters.iter_mut().enumerate() {
                *target = SceneParameterTarget {
                    rack_index: 0,
                    slot_index: 0,
                    parameter_id: u32::try_from(index).unwrap(),
                    value: 1.0,
                };
            }
            scene.parameter_count = MAX_SCENE_PARAMETERS;
            scene.bypasses[0] = Some(true);
            let mut renderer = ProductRenderer::new(PreparedGraph::empty());
            renderer.scenes = vec![scene];
            renderer.trigger_scene(0);

            let mut final_parameters_seen = false;
            let mut bypass_count = 0;
            for _ in 0..8 {
                renderer.automation[0].clear();
                renderer.service_scene(128);
                let events = renderer.automation[0].as_slice();
                final_parameters_seen |= events.iter().any(|event| {
                    event.event_type == BLOCK_EVENT_PARAMETER
                        && event.key == u32::try_from(MAX_SCENE_PARAMETERS - 1).unwrap()
                        && event.value.to_bits() == 1.0_f32.to_bits()
                });
                let bypass_events = events.iter().filter(|event| {
                    event.event_type == BLOCK_EVENT_SLOT_BYPASS
                        && event.flags == 1
                        && event.value.to_bits() == 1.0_f32.to_bits()
                });
                bypass_count += bypass_events.count();
                if bypass_count > 0 {
                    assert!(final_parameters_seen, "transition_ms={transition_ms}");
                }
                renderer.sample_clock += 128;
            }
            assert!(final_parameters_seen, "transition_ms={transition_ms}");
            assert_eq!(bypass_count, 1, "transition_ms={transition_ms}");
            assert_eq!(renderer.pending_scene_bypasses[0], None);
        }
    }

    #[test]
    fn saturated_final_scene_block_retries_all_targets_before_bypass() {
        let mut scene = PreparedProductScene::empty(0);
        for (index, target) in scene.parameters.iter_mut().enumerate() {
            *target = SceneParameterTarget {
                rack_index: 0,
                slot_index: 0,
                parameter_id: u32::try_from(index).unwrap(),
                value: 1.0,
            };
        }
        scene.parameter_count = MAX_SCENE_PARAMETERS;
        scene.bypasses[0] = Some(true);
        let (mut control, receiver) = ProductControl::new();
        let mut renderer = ProductRenderer::new(PreparedGraph::empty());
        renderer.control = Some(receiver);
        renderer.scenes = vec![scene];
        renderer.trigger_scene(0);

        renderer.service_scene(128);
        renderer.sample_clock += 128;
        renderer.automation[0].clear();
        assert!(control.set_parameter(0, 0, 999, 0.5));
        renderer.service_control();
        assert_eq!(renderer.automation[0].as_slice()[0].key, 999);
        for _ in 1..MAX_EVENTS {
            assert!(renderer.automation[0].push(BlockEvent::default()));
        }
        renderer.service_scene(128);
        assert!(
            renderer
                .pending_scene_parameters
                .iter()
                .all(Option::is_some)
        );
        assert_eq!(
            renderer.active_rack_scene.as_ref().unwrap().bypasses[0],
            Some(true)
        );

        renderer.sample_clock += 128;
        renderer.automation[0].clear();
        renderer.service_scene(128);
        let events = renderer.automation[0].as_slice();
        assert_eq!(events.len(), MAX_SCENE_PARAMETERS);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.event_type, BLOCK_EVENT_PARAMETER);
            assert_eq!(event.key, u32::try_from(index).unwrap());
            assert_eq!(event.value.to_bits(), 1.0_f32.to_bits());
        }
        assert!(
            renderer
                .pending_scene_parameters
                .iter()
                .all(Option::is_none)
        );
        assert_eq!(renderer.pending_scene_bypasses[0], Some(true));

        renderer.sample_clock += 128;
        renderer.automation[0].clear();
        renderer.service_scene(128);
        assert_eq!(renderer.automation[0].as_slice().len(), 1);
        assert_eq!(
            renderer.automation[0].as_slice()[0].event_type,
            BLOCK_EVENT_SLOT_BYPASS
        );
        assert_eq!(renderer.pending_scene_bypasses[0], None);
    }

    #[test]
    fn new_scene_discards_pending_values_from_replaced_scene() {
        let target = SceneParameterTarget {
            rack_index: 0,
            slot_index: 0,
            parameter_id: 42,
            value: 1.0,
        };
        let mut old_scene = PreparedProductScene::empty(0);
        old_scene.parameters[0] = target;
        old_scene.parameter_count = 1;
        let mut new_scene = PreparedProductScene::empty(0);
        new_scene.parameters[0] = SceneParameterTarget {
            value: 0.25,
            ..target
        };
        new_scene.parameter_count = 1;
        let mut renderer = ProductRenderer::new(PreparedGraph::empty());
        renderer.scenes = vec![old_scene, new_scene];
        renderer.trigger_scene(0);
        renderer.service_scene(128);
        renderer.sample_clock += 128;
        renderer.automation[0].clear();
        for _ in 0..MAX_EVENTS {
            assert!(renderer.automation[0].push(BlockEvent::default()));
        }
        renderer.service_scene(128);
        assert_eq!(
            renderer.pending_scene_parameters[0].map(|value| value.value),
            Some(1.0)
        );

        renderer.sample_clock += 128;
        renderer.automation[0].clear();
        renderer.trigger_scene(1);
        renderer.service_scene(128);
        renderer.sample_clock += 128;
        renderer.automation[0].clear();
        renderer.service_scene(128);
        assert_eq!(renderer.automation[0].as_slice().len(), 1);
        assert_eq!(
            renderer.automation[0].as_slice()[0].value.to_bits(),
            0.25_f32.to_bits()
        );
        assert!(
            renderer
                .pending_scene_parameters
                .iter()
                .all(Option::is_none)
        );
    }

    #[test]
    fn newer_live_write_cancels_deferred_scene_value() {
        let mut scene = PreparedProductScene::empty(0);
        scene.parameters[0] = SceneParameterTarget {
            rack_index: 0,
            slot_index: 0,
            parameter_id: 42,
            value: 1.0,
        };
        scene.parameter_count = 1;
        let (mut control, receiver) = ProductControl::new();
        let mut renderer = ProductRenderer::new(PreparedGraph::empty());
        renderer.control = Some(receiver);
        renderer.scenes = vec![scene];
        renderer.trigger_scene(0);
        renderer.service_scene(128);
        renderer.sample_clock += 128;
        renderer.automation[0].clear();
        for _ in 0..MAX_EVENTS {
            assert!(renderer.automation[0].push(BlockEvent::default()));
        }
        renderer.service_scene(128);
        assert_eq!(renderer.pending_scene_parameter_count, 1);

        renderer.sample_clock += 128;
        renderer.automation[0].clear();
        assert!(control.set_parameter(0, 0, 42, 0.75));
        renderer.service_control();
        renderer.service_scene(128);
        assert_eq!(renderer.pending_scene_parameter_count, 0);
        assert_eq!(renderer.automation[0].as_slice().len(), 1);
        assert_eq!(
            renderer.automation[0].as_slice()[0].value.to_bits(),
            0.75_f32.to_bits()
        );
    }

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
