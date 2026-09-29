//! Allocation-free product realtime mix path.
//!
//! Construction and graph staging may allocate. [`RealtimeRackMixer::render_block`] is a
//! fixed-capacity callback operation: it does not allocate, lock, or perform control IPC.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use sp_model::{MAX_RACKS, PhysicalChannels};

use crate::{
    BlockActivation, DryDelayError, FallbackAudio, FallbackReason, GateOutcome, GraphArena,
    LiveBlockPlanner, PreparedChannelConversion, PreparedChannelLayout, PreparedGraph,
    PreparedSidechain, PreparedSlot, RackBlockAction,
};

/// Maximum frames supported by the product mixer (256-frame mode).
pub const MAX_MIX_FRAMES: usize = 256;
/// Largest latency the preallocated dry path can match exactly. Higher reported values are
/// retained in rack settings, but bypass and dry fallback render silence.
pub const MAX_DRY_DELAY_FRAMES: usize = 65_536;
/// Stereo channel count used by the hardware output mixer.
pub const MIX_CHANNELS: usize = 2;
/// Minimum click-bounded transition length.
pub const MIN_TRANSITION_FRAMES: usize = 64;
/// Maximum click-bounded transition length.
pub const MAX_TRANSITION_FRAMES: usize = 128;
/// Fade length when a layout change replaces a rack's worker, both to dry and to the new chain.
pub const JOIN_FADE_FRAMES: usize = 16;
/// Number of consecutive valid wet blocks required after a fallback before wet recovery.
pub const WET_RECOVERY_BLOCKS: u8 = 3;
// The mixer writes an entire block before reading delayed frames, so the ring needs one full
// callback block beyond the maximum supported delay. Eight stereo rings use about 4.2 MB,
// allocated when the mixer is built; the audio callback never allocates.
const DRY_DELAY_CAPACITY_FRAMES: usize = MAX_DRY_DELAY_FRAMES + MAX_MIX_FRAMES;
const SILENT_RACK_INPUT: [f32; MAX_MIX_FRAMES * MIX_CHANNELS] =
    [0.0; MAX_MIX_FRAMES * MIX_CHANNELS];

/// Live rack controls applied by the realtime mixer at the next render call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RackSettings {
    /// Linear gain (`1.0` is unity).
    pub gain: f32,
    /// Suppress this rack's contribution.
    pub muted: bool,
    /// Route latency-matched dry audio instead of worker wet output.
    pub bypassed: bool,
    /// Reported rack latency. Values above [`MAX_DRY_DELAY_FRAMES`] make dry output silent.
    pub latency_frames: usize,
}

impl Default for RackSettings {
    fn default() -> Self {
        Self {
            gain: 1.0,
            muted: false,
            bypassed: false,
            latency_frames: 0,
        }
    }
}

/// How one rack position is filled after a live topology change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixerLane {
    /// Previous position whose gain, fades, and dry history continue here. `None` starts a
    /// fresh lane that fades in from silence.
    pub from: Option<usize>,
    /// Live settings for a fresh lane. Continued lanes keep their current settings.
    pub settings: RackSettings,
    /// Whether this position may fall back to delayed dry audio.
    pub dry_fallback: bool,
    /// The rack's plug-in chain was replaced: fade to dry, then fade to the new chain once it
    /// delivers [`WET_RECOVERY_BLOCKS`] valid blocks. Both fades last [`JOIN_FADE_FRAMES`].
    pub restart: bool,
}

impl MixerLane {
    /// A fresh lane with default settings and dry fallback.
    pub const FRESH: Self = Self {
        from: None,
        settings: RackSettings {
            gain: 1.0,
            muted: false,
            bypassed: false,
            latency_frames: 0,
        },
        dry_fallback: true,
        restart: false,
    };
}

/// Per-rack contribution supplied for one block. Wet data is interleaved hardware stereo;
/// a mono endpoint reads the left lane and duplicates it for playback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RackAudioSource<'a> {
    /// Validated wet output from a worker completion.
    Wet(&'a [f32]),
    /// No valid worker output was supplied.
    None,
}

/// Lock-free meter snapshot for one rack or the final output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RackMeterSnapshot {
    /// Peak absolute value for left and right hardware-output channels.
    pub peak: [f32; MIX_CHANNELS],
    /// RMS value for left and right hardware-output channels.
    pub rms: [f32; MIX_CHANNELS],
    /// A finite sample at or above full scale occurred in this block.
    pub clipped: bool,
}

struct AtomicMeter {
    peak: [AtomicU32; MIX_CHANNELS],
    rms: [AtomicU32; MIX_CHANNELS],
    clipped: AtomicBool,
}

impl AtomicMeter {
    fn new() -> Self {
        Self {
            peak: std::array::from_fn(|_| AtomicU32::new(0)),
            rms: std::array::from_fn(|_| AtomicU32::new(0)),
            clipped: AtomicBool::new(false),
        }
    }

    fn publish(&self, snapshot: RackMeterSnapshot) {
        for channel in 0..MIX_CHANNELS {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderSource {
    Wet,
    Dry,
    Silence,
    /// The last frame a replaced plug-in chain played, held and faded out during a worker
    /// handoff so the join to the new source is continuous.
    Held,
}

#[derive(Clone, Copy, Debug)]
struct TransitionState {
    from: RenderSource,
    position: usize,
    frames: usize,
}

impl TransitionState {
    const fn idle() -> Self {
        Self {
            from: RenderSource::Silence,
            position: 0,
            frames: 0,
        }
    }

    const fn active(self) -> bool {
        self.position < self.frames
    }
}

/// Fixed-capacity realtime mixer for the product `CoreAudio` path.
pub struct RealtimeRackMixer {
    arena: GraphArena,
    gate_outcomes: [GateOutcome; MAX_RACKS],
    planner: LiveBlockPlanner,
    dry_storage: Box<[Box<[f32]>; MAX_RACKS]>,
    dry_write_frame: [usize; MAX_RACKS],
    settings: [RackSettings; MAX_RACKS],
    transitions: [TransitionState; MAX_RACKS],
    rendered_source: [RenderSource; MAX_RACKS],
    valid_wet_blocks: [u8; MAX_RACKS],
    /// A new worker was placed by a layout change; its wet output fades in over
    /// [`JOIN_FADE_FRAMES`].
    joining: [bool; MAX_RACKS],
    scratch: [f32; MAX_MIX_FRAMES * MIX_CHANNELS],
    last_wet: Box<[Box<[f32]>; MAX_RACKS]>,
    /// Last frame each rack rendered, before gain.
    last_frame: [[f32; MIX_CHANNELS]; MAX_RACKS],
    /// Each rack's post-fader output from the latest block, read by rack-output sidechains
    /// before the next block is mixed. One block per rack lane, `MAX_RACKS` long, on the heap
    /// because 64 lanes are too large for the stack.
    rack_outputs: Box<[[f32; MAX_MIX_FRAMES * MIX_CHANNELS]]>,
    /// Frames held in `rack_outputs`; zero until the rack renders.
    rack_output_frames: [usize; MAX_RACKS],
    rack_meters: [AtomicMeter; MAX_RACKS],
    output_meter: AtomicMeter,
    transition_frames: usize,
}

impl RealtimeRackMixer {
    /// Creates a mixer with an initial prepared graph.
    #[must_use]
    pub fn new(initial: PreparedGraph) -> Self {
        let mut planner = LiveBlockPlanner::new();
        for index in 0..initial.rack_count() {
            planner.set_dry_delay_available(index, true);
        }
        let dry_storage = Box::new(std::array::from_fn(|_| {
            vec![0.0_f32; DRY_DELAY_CAPACITY_FRAMES * MIX_CHANNELS].into_boxed_slice()
        }));
        Self {
            arena: GraphArena::new(initial),
            gate_outcomes: [GateOutcome::DispatchAllowed; MAX_RACKS],
            planner,
            dry_storage,
            dry_write_frame: [0; MAX_RACKS],
            settings: [RackSettings::default(); MAX_RACKS],
            transitions: [TransitionState::idle(); MAX_RACKS],
            rendered_source: [RenderSource::Silence; MAX_RACKS],
            valid_wet_blocks: [0; MAX_RACKS],
            joining: [false; MAX_RACKS],
            scratch: [0.0; MAX_MIX_FRAMES * MIX_CHANNELS],
            last_wet: Box::new(std::array::from_fn(|_| {
                vec![0.0_f32; MAX_MIX_FRAMES * MIX_CHANNELS].into_boxed_slice()
            })),
            last_frame: [[0.0; MIX_CHANNELS]; MAX_RACKS],
            rack_outputs: vec![[0.0; MAX_MIX_FRAMES * MIX_CHANNELS]; MAX_RACKS].into_boxed_slice(),
            rack_output_frames: [0; MAX_RACKS],
            rack_meters: std::array::from_fn(|_| AtomicMeter::new()),
            output_meter: AtomicMeter::new(),
            transition_frames: MIN_TRANSITION_FRAMES,
        }
    }

    /// Stages a replacement graph for activation at the next block boundary.
    pub fn stage_graph(&mut self, graph: PreparedGraph) {
        let rack_count = graph.rack_count();
        self.arena.stage(graph);
        for index in 0..rack_count {
            self.planner.set_dry_delay_available(index, true);
        }
    }

    /// Moves per-rack state to new positions and stages `graph`, at a callback block boundary.
    ///
    /// Continued lanes keep gain, fades, and dry history, so untouched racks play on without a
    /// gap. Fresh lanes start silent and fade in to dry. A new worker's chain fades in over
    /// [`JOIN_FADE_FRAMES`] after [`WET_RECOVERY_BLOCKS`] valid blocks. This performs no
    /// allocation.
    pub fn apply_topology(&mut self, graph: PreparedGraph, lanes: &[MixerLane; MAX_RACKS]) {
        let from = lane_permutation(lanes);
        permute_lanes(&mut self.gate_outcomes, &from);
        permute_lanes(&mut *self.dry_storage, &from);
        permute_lanes(&mut self.dry_write_frame, &from);
        permute_lanes(&mut self.settings, &from);
        permute_lanes(&mut self.transitions, &from);
        permute_lanes(&mut self.rendered_source, &from);
        permute_lanes(&mut self.valid_wet_blocks, &from);
        permute_lanes(&mut self.joining, &from);
        permute_lanes(&mut *self.last_wet, &from);
        permute_lanes(&mut self.last_frame, &from);
        permute_lanes(&mut self.rack_outputs, &from);
        permute_lanes(&mut self.rack_output_frames, &from);
        permute_lanes(&mut self.rack_meters, &from);
        for (index, lane) in lanes.iter().enumerate() {
            self.planner
                .set_dry_delay_available(index, lane.dry_fallback);
            if lane.from.is_some() {
                if lane.restart {
                    // The old chain is gone. Play dry until the new one proves itself.
                    self.gate_outcomes[index] = GateOutcome::DispatchAllowed;
                    self.valid_wet_blocks[index] = 0;
                    self.joining[index] = true;
                    self.transitions[index] = TransitionState {
                        from: RenderSource::Held,
                        position: 0,
                        frames: JOIN_FADE_FRAMES,
                    };
                    self.rendered_source[index] = if lane.dry_fallback {
                        RenderSource::Dry
                    } else {
                        RenderSource::Silence
                    };
                }
                continue;
            }
            self.gate_outcomes[index] = GateOutcome::DispatchAllowed;
            self.dry_storage[index].fill(0.0);
            self.dry_write_frame[index] = 0;
            self.set_rack_settings(index, lane.settings);
            self.transitions[index] = TransitionState::idle();
            self.rendered_source[index] = RenderSource::Silence;
            self.valid_wet_blocks[index] = 0;
            self.joining[index] = true;
            self.last_wet[index].fill(0.0);
            self.last_frame[index] = [0.0; MIX_CHANNELS];
            self.rack_output_frames[index] = 0;
        }
        self.arena.stage(graph);
    }

    /// Sets the fixed wet/dry transition length. Values are clamped to 64--128 frames.
    pub fn set_transition_frames(&mut self, frames: usize) {
        self.transition_frames = frames.clamp(MIN_TRANSITION_FRAMES, MAX_TRANSITION_FRAMES);
    }

    /// Selects delayed dry fallback for compatible effects or silence for instruments.
    pub fn set_dry_fallback_available(&mut self, rack_index: usize, available: bool) {
        self.planner.set_dry_delay_available(rack_index, available);
    }

    /// Replaces all live settings for one rack. Non-finite gain becomes silence. Reported
    /// latency is retained; an unsupported delay renders silence on dry paths.
    pub fn set_rack_settings(&mut self, rack_index: usize, mut settings: RackSettings) {
        if let Some(slot) = self.settings.get_mut(rack_index) {
            settings.gain = if settings.gain.is_finite() {
                settings.gain
            } else {
                0.0
            };
            *slot = settings;
        }
    }

    /// Returns the current live settings for a rack.
    #[must_use]
    pub fn rack_settings(&self, rack_index: usize) -> Option<RackSettings> {
        self.settings.get(rack_index).copied()
    }

    /// Records the gate outcome observed for `rack_index` during this block.
    pub fn set_gate_outcome(&mut self, rack_index: usize, outcome: GateOutcome) {
        if let Some(slot) = self.gate_outcomes.get_mut(rack_index) {
            *slot = outcome;
        }
    }
    /// Sets local fallback latency.
    pub fn set_dry_delay_frames(&mut self, rack_index: usize, delay_frames: usize) {
        if let Some(settings) = self.rack_settings(rack_index) {
            self.set_rack_settings(
                rack_index,
                RackSettings {
                    latency_frames: delay_frames,
                    ..settings
                },
            );
        }
    }
    /// Sets linear rack gain.
    pub fn set_rack_gain(&mut self, rack_index: usize, gain: f32) {
        if let Some(settings) = self.rack_settings(rack_index) {
            self.set_rack_settings(rack_index, RackSettings { gain, ..settings });
        }
    }
    /// Mutes or unmutes a rack.
    pub fn set_rack_muted(&mut self, rack_index: usize, muted: bool) {
        if let Some(settings) = self.rack_settings(rack_index) {
            self.set_rack_settings(rack_index, RackSettings { muted, ..settings });
        }
    }
    /// Bypasses or re-enables a rack using its latency-matched dry path.
    pub fn set_rack_bypassed(&mut self, rack_index: usize, bypassed: bool) {
        if let Some(settings) = self.rack_settings(rack_index) {
            self.set_rack_settings(
                rack_index,
                RackSettings {
                    bypassed,
                    ..settings
                },
            );
        }
    }

    /// Returns a lock-free snapshot of one rack's most recent meter publication.
    #[must_use]
    pub fn rack_meter_snapshot(&self, rack_index: usize) -> Option<RackMeterSnapshot> {
        self.rack_meters.get(rack_index).map(AtomicMeter::snapshot)
    }
    /// Returns a lock-free snapshot of the hardware output. For devices with more than two
    /// channels, each lane reports the maximum peak and RMS among physical channels of
    /// matching even/odd index; clipping includes every physical channel.
    #[must_use]
    pub fn output_meter_snapshot(&self) -> RackMeterSnapshot {
        self.output_meter.snapshot()
    }

    /// Returns the planner action for a rack given its last recorded gate outcome.
    #[must_use]
    pub fn action_for(&self, rack_index: usize) -> RackBlockAction {
        let outcome =
            self.gate_outcomes
                .get(rack_index)
                .copied()
                .unwrap_or(GateOutcome::UseFallback(
                    FallbackReason::InvalidProtocolState,
                ));
        self.planner.action_for(rack_index, outcome)
    }

    /// Returns the graph that the next block will use.
    #[must_use]
    pub fn prepared_graph(&self) -> &PreparedGraph {
        self.arena.prepared_graph()
    }

    /// Checks selected channels, including physical sidechain pairs, against the opened input
    /// and output devices.
    ///
    /// # Errors
    /// Returns [`MixError::PhysicalChannelUnavailable`] for an out-of-range route, or
    /// [`MixError::SidechainChannelUnavailable`] for an out-of-range sidechain pair.
    ///
    /// # Panics
    /// Panics only if the prepared graph misreports its rack count.
    pub fn validate_routes(
        &self,
        input_channels: usize,
        output_channels: usize,
    ) -> Result<(), MixError> {
        for rack_index in 0..self.prepared_graph().rack_count() {
            let rack = self
                .prepared_graph()
                .rack(rack_index)
                .expect("prepared rack");
            if let Some(input) = rack.input_channels() {
                check_physical_channels(input, input_channels, rack_index, true)?;
            }
            check_physical_channels(rack.output_channels(), output_channels, rack_index, false)?;
            for slot_index in 0..rack.slot_count() {
                let Some(PreparedSidechain::PhysicalInput { left, right }) =
                    rack.slot(slot_index).and_then(PreparedSlot::sidechain)
                else {
                    continue;
                };
                if let Some(channel) = [left, right]
                    .into_iter()
                    .find(|&channel| usize::from(channel) >= input_channels)
                {
                    return Err(MixError::SidechainChannelUnavailable {
                        rack_index,
                        slot_index,
                        channel,
                        available: input_channels,
                    });
                }
            }
        }
        Ok(())
    }

    /// Returns the sidechain audio available to the block about to be dispatched: `input` is
    /// that block's interleaved callback input with `input_channels` channels.
    #[must_use]
    pub fn sidechain_sources<'a>(
        &'a self,
        input: &'a [f32],
        input_channels: usize,
    ) -> SidechainSources<'a> {
        SidechainSources {
            graph: self.arena.prepared_graph(),
            input,
            input_channels,
            rack_outputs: &self.rack_outputs,
            rack_output_frames: &self.rack_output_frames,
        }
    }

    /// Mixes independent rack input blocks into interleaved physical device channels.
    /// Each rack input block uses stereo lanes; a mono selected input occupies both lanes.
    ///
    /// # Errors
    /// Returns [`MixError`] for invalid buffer geometry or output routes.
    ///
    /// # Panics
    /// Panics only if the prepared graph misreports its rack count.
    pub fn render_routed_block(
        &mut self,
        inputs: &[[f32; MAX_MIX_FRAMES * MIX_CHANNELS]; MAX_RACKS],
        sources: &[RackAudioSource<'_>],
        output: &mut [f32],
        output_channels: usize,
        frames: usize,
    ) -> Result<BlockActivation, MixError> {
        #[cfg(test)]
        let _guard = realtime_allocation_guard::Operation::enter();
        if frames == 0 || frames > MAX_MIX_FRAMES {
            return Err(MixError::FrameCount { frames });
        }
        if output_channels == 0 {
            return Err(MixError::BufferLength);
        }
        let samples = frames
            .checked_mul(output_channels)
            .ok_or(MixError::BufferLength)?;
        if output.len() < samples {
            return Err(MixError::BufferLength);
        }
        let activation = self.arena.activate_audio_thread().activate_block();
        let graph = activation.graph();
        for rack_index in 0..graph.rack_count() {
            let rack = graph.rack(rack_index).expect("prepared rack");
            check_physical_channels(rack.output_channels(), output_channels, rack_index, false)?;
        }
        output[..samples].fill(0.0);
        for (rack_index, rack_input) in inputs.iter().enumerate().take(graph.rack_count()) {
            let rack = graph.rack(rack_index).expect("prepared rack");
            let input = if rack.input_channels().is_some() {
                rack_input
            } else {
                &SILENT_RACK_INPUT
            };
            self.advance_dry(rack_index, input, frames);
            let source = sources
                .get(rack_index)
                .copied()
                .unwrap_or(RackAudioSource::None);
            let target = self.select_source(rack_index, source);
            self.begin_source_change(rack_index, target);
            self.capture_wet(rack_index, rack.endpoint_layout(), source, frames);
            self.render_rack(
                rack_index,
                rack.source_layout(),
                rack.endpoint_layout(),
                rack.conversion(),
                source,
                target,
                frames,
            );
            let gain = if self.settings[rack_index].muted {
                0.0
            } else {
                self.settings[rack_index].gain
            };
            self.capture_rack_output(rack_index, gain, frames);
            for frame in 0..frames {
                let scratch = frame * MIX_CHANNELS;
                let output_frame = frame * output_channels;
                match rack.output_channels() {
                    PhysicalChannels::Mono { channel } => {
                        output[output_frame + usize::from(channel)] += self.scratch[scratch] * gain;
                    }
                    PhysicalChannels::Stereo { left, right } => {
                        output[output_frame + usize::from(left)] += self.scratch[scratch] * gain;
                        output[output_frame + usize::from(right)] +=
                            self.scratch[scratch + 1] * gain;
                    }
                }
            }
            self.rack_meters[rack_index].publish(meter(
                &self.scratch[..frames * MIX_CHANNELS],
                frames,
                gain,
            ));
        }
        self.output_meter.publish(physical_output_meter(
            &output[..samples],
            output_channels,
            frames,
        ));
        Ok(activation)
    }

    /// Activates any staged graph and mixes rack contributions into `output`.
    ///
    /// # Errors
    /// Returns [`MixError`] for unsupported frame counts or undersized buffers.
    ///
    /// # Panics
    /// Panics only if the active prepared graph misreports its own rack count.
    pub fn render_block(
        &mut self,
        input: &[f32],
        sources: &[RackAudioSource<'_>],
        output: &mut [f32],
        frames: usize,
    ) -> Result<BlockActivation, MixError> {
        #[cfg(test)]
        let _guard = realtime_allocation_guard::Operation::enter();
        if frames == 0 || frames > MAX_MIX_FRAMES {
            return Err(MixError::FrameCount { frames });
        }
        let samples = frames
            .checked_mul(MIX_CHANNELS)
            .ok_or(MixError::BufferLength)?;
        if input.len() < samples || output.len() < samples {
            return Err(MixError::BufferLength);
        }
        let activation = self.arena.activate_audio_thread().activate_block();
        let rack_count = activation.graph().rack_count();
        output[..samples].fill(0.0);
        for rack_index in 0..rack_count {
            let rack = activation
                .graph()
                .rack(rack_index)
                .expect("prepared active rack");
            self.advance_dry(rack_index, input, frames);
            let source = sources
                .get(rack_index)
                .copied()
                .unwrap_or(RackAudioSource::None);
            let target = self.select_source(rack_index, source);
            self.begin_source_change(rack_index, target);
            self.capture_wet(rack_index, rack.endpoint_layout(), source, frames);
            self.render_rack(
                rack_index,
                rack.source_layout(),
                rack.endpoint_layout(),
                rack.conversion(),
                source,
                target,
                frames,
            );
            let gain = if self.settings[rack_index].muted {
                0.0
            } else {
                self.settings[rack_index].gain
            };
            self.capture_rack_output(rack_index, gain, frames);
            mix_scaled(&self.scratch[..samples], output, gain);
            self.rack_meters[rack_index].publish(meter(&self.scratch[..samples], frames, gain));
        }
        self.output_meter
            .publish(meter(&output[..samples], frames, 1.0));
        Ok(activation)
    }

    /// Keeps the rack's post-fader output for rack-output sidechains in the next block.
    fn capture_rack_output(&mut self, rack_index: usize, gain: f32, frames: usize) {
        let samples = frames * MIX_CHANNELS;
        for (output, sample) in self.rack_outputs[rack_index][..samples]
            .iter_mut()
            .zip(&self.scratch[..samples])
        {
            *output = sample * gain;
        }
        self.rack_output_frames[rack_index] = frames;
    }

    fn select_source(&mut self, rack_index: usize, supplied: RackAudioSource<'_>) -> RenderSource {
        let valid_wet = matches!(supplied, RackAudioSource::Wet(_))
            && matches!(
                self.action_for(rack_index),
                RackBlockAction::AcceptWorkerResult | RackBlockAction::Dispatch
            );
        if self.settings[rack_index].bypassed {
            return RenderSource::Dry;
        }
        if valid_wet {
            self.valid_wet_blocks[rack_index] = self.valid_wet_blocks[rack_index].saturating_add(1);
            if self.rendered_source[rack_index] == RenderSource::Wet
                || self.valid_wet_blocks[rack_index] >= WET_RECOVERY_BLOCKS
            {
                RenderSource::Wet
            } else if self.planner.dry_delay_available(rack_index) {
                RenderSource::Dry
            } else {
                RenderSource::Silence
            }
        } else {
            self.valid_wet_blocks[rack_index] = 0;
            match self.action_for(rack_index) {
                RackBlockAction::UseFallback(FallbackAudio::Silence) => RenderSource::Silence,
                _ => RenderSource::Dry,
            }
        }
    }

    /// Fades to a new source. A joining worker's chain uses the shorter join fade.
    fn begin_source_change(&mut self, rack: usize, target: RenderSource) {
        let from = self.rendered_source[rack];
        if from == target || self.transitions[rack].active() {
            return;
        }
        self.transitions[rack] = TransitionState {
            from,
            position: 0,
            frames: if self.joining[rack] && target == RenderSource::Wet {
                JOIN_FADE_FRAMES
            } else {
                self.transition_frames
            },
        };
    }

    fn capture_wet(
        &mut self,
        rack: usize,
        endpoint_layout: PreparedChannelLayout,
        wet: RackAudioSource<'_>,
        frames: usize,
    ) {
        let RackAudioSource::Wet(samples) = wet else {
            return;
        };
        for frame in 0..frames {
            let destination = frame * MIX_CHANNELS;
            match endpoint_layout {
                PreparedChannelLayout::Mono => {
                    let sample = samples.get(frame * MIX_CHANNELS).copied().unwrap_or(0.0);
                    self.last_wet[rack][destination] = sample;
                    self.last_wet[rack][destination + 1] = sample;
                }
                PreparedChannelLayout::Stereo => {
                    self.last_wet[rack][destination] =
                        samples.get(destination).copied().unwrap_or(0.0);
                    self.last_wet[rack][destination + 1] =
                        samples.get(destination + 1).copied().unwrap_or(0.0);
                }
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "per-rack render parameters stay on the stack of the realtime callback"
    )]
    fn render_rack(
        &mut self,
        rack_index: usize,
        source_layout: PreparedChannelLayout,
        endpoint_layout: PreparedChannelLayout,
        conversion: PreparedChannelConversion,
        wet: RackAudioSource<'_>,
        target: RenderSource,
        frames: usize,
    ) {
        let transition = self.transitions[rack_index];
        for frame in 0..frames {
            let (from_l, from_r) = self.source_frame(
                rack_index,
                source_layout,
                endpoint_layout,
                conversion,
                wet,
                transition.from,
                frame,
                frames,
            );
            let (to_l, to_r) = self.source_frame(
                rack_index,
                source_layout,
                endpoint_layout,
                conversion,
                wet,
                target,
                frame,
                frames,
            );
            let (left, right) = if transition.active() {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "transition positions are bounded well below f32 precision limits"
                )]
                let amount = (transition.position + frame).min(transition.frames) as f32
                    / transition.frames as f32;
                (
                    from_l + (to_l - from_l) * amount,
                    from_r + (to_r - from_r) * amount,
                )
            } else {
                (to_l, to_r)
            };
            self.scratch[frame * 2] = left;
            self.scratch[frame * 2 + 1] = right;
        }
        if frames > 0 {
            let last = (frames - 1) * 2;
            self.last_frame[rack_index] = [self.scratch[last], self.scratch[last + 1]];
        }
        if transition.active() {
            let updated = transition.position.saturating_add(frames);
            if updated >= transition.frames {
                self.transitions[rack_index] = TransitionState::idle();
                self.rendered_source[rack_index] = target;
            } else {
                self.transitions[rack_index].position = updated;
            }
        } else {
            self.rendered_source[rack_index] = target;
        }
        if self.rendered_source[rack_index] == RenderSource::Wet {
            self.joining[rack_index] = false;
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "per-frame source selection stays on the stack of the realtime callback"
    )]
    fn source_frame(
        &self,
        rack: usize,
        source_layout: PreparedChannelLayout,
        endpoint_layout: PreparedChannelLayout,
        conversion: PreparedChannelConversion,
        wet: RackAudioSource<'_>,
        source: RenderSource,
        frame: usize,
        frames: usize,
    ) -> (f32, f32) {
        match source {
            RenderSource::Silence => (0.0, 0.0),
            RenderSource::Held => (self.last_frame[rack][0], self.last_frame[rack][1]),
            RenderSource::Dry => self.dry_frame(rack, source_layout, conversion, frame, frames),
            RenderSource::Wet => match wet {
                RackAudioSource::Wet(samples) => match endpoint_layout {
                    PreparedChannelLayout::Mono => {
                        let value = samples.get(frame * MIX_CHANNELS).copied().unwrap_or(0.0);
                        (value, value)
                    }
                    PreparedChannelLayout::Stereo => {
                        let index = frame * 2;
                        (
                            samples.get(index).copied().unwrap_or(0.0),
                            samples.get(index + 1).copied().unwrap_or(0.0),
                        )
                    }
                },
                RackAudioSource::None => {
                    let index = frame * MIX_CHANNELS;
                    (self.last_wet[rack][index], self.last_wet[rack][index + 1])
                }
            },
        }
    }

    fn advance_dry(&mut self, rack: usize, input: &[f32], frames: usize) {
        let storage = &mut self.dry_storage[rack];
        let mut write = self.dry_write_frame[rack];
        for frame in 0..frames {
            let index = frame * 2;
            let write_index = write * 2;
            storage[write_index] = input[index];
            storage[write_index + 1] = input[index + 1];
            write = (write + 1) % DRY_DELAY_CAPACITY_FRAMES;
        }
        self.dry_write_frame[rack] = write;
    }

    fn dry_frame(
        &self,
        rack: usize,
        input_layout: PreparedChannelLayout,
        conversion: PreparedChannelConversion,
        frame: usize,
        frames: usize,
    ) -> (f32, f32) {
        if self.settings[rack].latency_frames > MAX_DRY_DELAY_FRAMES {
            return (0.0, 0.0);
        }
        let written = (self.dry_write_frame[rack] + DRY_DELAY_CAPACITY_FRAMES - frames + frame)
            % DRY_DELAY_CAPACITY_FRAMES;
        let read = (written + DRY_DELAY_CAPACITY_FRAMES - self.settings[rack].latency_frames)
            % DRY_DELAY_CAPACITY_FRAMES;
        let base = read * 2;
        let left = self.dry_storage[rack][base];
        let right = self.dry_storage[rack][base + 1];
        let _ = conversion;
        match input_layout {
            PreparedChannelLayout::Stereo => (left, right),
            PreparedChannelLayout::Mono => (left, left),
        }
    }
}

/// Completes the lanes' `from` choices into a bijection. Unused previous positions fill the
/// fresh positions, so every per-rack buffer is moved rather than dropped or allocated.
///
/// # Panics
/// Never in practice: each unfilled lane has a matching unused position by construction.
#[must_use]
pub fn lane_permutation(lanes: &[MixerLane; MAX_RACKS]) -> [usize; MAX_RACKS] {
    let mut used = [false; MAX_RACKS];
    let mut from = [usize::MAX; MAX_RACKS];
    for (index, lane) in lanes.iter().enumerate() {
        if let Some(previous) = lane
            .from
            .filter(|&previous| previous < MAX_RACKS && !used[previous])
        {
            used[previous] = true;
            from[index] = previous;
        }
    }
    let mut spare = (0..MAX_RACKS).filter(|&previous| !used[previous]);
    for slot in &mut from {
        if *slot == usize::MAX {
            *slot = spare.next().expect("unused positions match unfilled lanes");
        }
    }
    from
}

/// Reorders `items` so that `items[i]` becomes the previous `items[from[i]]`, by swaps only.
/// Allocation-free; safe on the audio callback.
pub fn permute_lanes<T>(items: &mut [T], from: &[usize; MAX_RACKS]) {
    let mut done = [false; MAX_RACKS];
    for start in 0..items.len().min(MAX_RACKS) {
        let mut index = start;
        while !done[index] {
            done[index] = true;
            let next = from[index];
            if next == start {
                break;
            }
            items.swap(index, next);
            index = next;
        }
    }
}

/// Sidechain audio for one callback block, read while its rack requests are written.
///
/// A physical source copies two channels of the block's own input, so it is sample aligned.
/// A rack source copies that rack's post-fader output from the previous block: one buffer of
/// latency, which keeps racks independent and needs no cycle check. What a rack contributes to
/// the mix is its output, including dry fallback; a muted rack gives silence. A rack that has
/// not rendered a block of this length yet also gives silence.
#[derive(Clone, Copy)]
pub struct SidechainSources<'a> {
    graph: &'a PreparedGraph,
    input: &'a [f32],
    input_channels: usize,
    rack_outputs: &'a [[f32; MAX_MIX_FRAMES * MIX_CHANNELS]],
    rack_output_frames: &'a [usize; MAX_RACKS],
}

impl SidechainSources<'_> {
    /// Writes `frames` of the sidechain for plug-in `slot` of rack `rack` into planar
    /// `destination`. Returns `false`, writing nothing, when that slot has no sidechain.
    /// Bounded and allocation-free; channels outside the input read as silence.
    pub fn fill(
        &self,
        rack: usize,
        slot: usize,
        frames: usize,
        destination: &mut [[f32; MAX_MIX_FRAMES]; MIX_CHANNELS],
    ) -> bool {
        let Some(source) = self
            .graph
            .rack(rack)
            .and_then(|rack| rack.slot(slot))
            .and_then(PreparedSlot::sidechain)
        else {
            return false;
        };
        let frames = frames.min(MAX_MIX_FRAMES);
        match source {
            PreparedSidechain::PhysicalInput { left, right } => {
                for (plane, channel) in destination.iter_mut().zip([left, right]) {
                    let channel = usize::from(channel);
                    for (frame, sample) in plane[..frames].iter_mut().enumerate() {
                        *sample = if channel < self.input_channels {
                            self.input
                                .get(frame * self.input_channels + channel)
                                .copied()
                                .unwrap_or(0.0)
                        } else {
                            0.0
                        };
                    }
                }
            }
            PreparedSidechain::RackOutput(source) => {
                let previous = self
                    .rack_outputs
                    .get(source)
                    .filter(|_| self.rack_output_frames.get(source) == Some(&frames));
                for (channel, plane) in destination.iter_mut().enumerate() {
                    for (frame, sample) in plane[..frames].iter_mut().enumerate() {
                        *sample =
                            previous.map_or(0.0, |output| output[frame * MIX_CHANNELS + channel]);
                    }
                }
            }
        }
        true
    }
}

fn check_physical_channels(
    channels: PhysicalChannels,
    available: usize,
    rack_index: usize,
    input: bool,
) -> Result<(), MixError> {
    let check = |channel: u8| {
        if usize::from(channel) < available {
            Ok(())
        } else {
            Err(MixError::PhysicalChannelUnavailable {
                rack_index,
                channel,
                available,
                input,
            })
        }
    };
    match channels {
        PhysicalChannels::Mono { channel } => check(channel),
        PhysicalChannels::Stereo { left, right } => {
            check(left)?;
            check(right)
        }
    }
}

fn mix_scaled(source: &[f32], output: &mut [f32], gain: f32) {
    for (out, sample) in output.iter_mut().zip(source) {
        *out += *sample * gain;
    }
}
#[allow(
    clippy::cast_precision_loss,
    reason = "frame counts are bounded far below f32 precision limits"
)]
fn meter(samples: &[f32], frames: usize, gain: f32) -> RackMeterSnapshot {
    let mut peak = [0.0_f32; 2];
    let mut sum = [0.0_f32; 2];
    let mut clipped = false;
    for frame in 0..frames {
        for channel in 0..2 {
            let sample = samples[frame * 2 + channel] * gain;
            let abs = sample.abs();
            peak[channel] = peak[channel].max(abs);
            sum[channel] += sample * sample;
            clipped |= sample.is_finite() && abs >= 1.0;
        }
    }
    RackMeterSnapshot {
        peak,
        rms: [
            (sum[0] / frames as f32).sqrt(),
            (sum[1] / frames as f32).sqrt(),
        ],
        clipped,
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "frame counts are bounded far below f32 precision limits"
)]
fn physical_output_meter(samples: &[f32], channels: usize, frames: usize) -> RackMeterSnapshot {
    let mut peaks = [0.0_f32; 64];
    let mut sums = [0.0_f32; 64];
    let mut clipped = false;
    for frame in 0..frames {
        for channel in 0..channels.min(64) {
            let sample = samples[frame * channels + channel];
            let absolute = sample.abs();
            peaks[channel] = peaks[channel].max(absolute);
            sums[channel] += sample * sample;
            clipped |= sample.is_finite() && absolute >= 1.0;
        }
    }
    let mut peak = [0.0_f32; MIX_CHANNELS];
    let mut rms = [0.0_f32; MIX_CHANNELS];
    for channel in 0..channels.min(64) {
        let lane = channel % MIX_CHANNELS;
        peak[lane] = peak[lane].max(peaks[channel]);
        rms[lane] = rms[lane].max((sums[channel] / frames as f32).sqrt());
    }
    if channels == 1 {
        peak[1] = peak[0];
        rms[1] = rms[0];
    }
    RackMeterSnapshot { peak, rms, clipped }
}

/// Errors from [`RealtimeRackMixer::render_block`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixError {
    /// A selected physical channel does not exist on the opened device.
    PhysicalChannelUnavailable {
        /// Rack position in the prepared graph.
        rack_index: usize,
        /// Zero-based selected channel.
        channel: u8,
        /// Number of channels exposed by the device.
        available: usize,
        /// `true` for the input device, `false` for the output device.
        input: bool,
    },
    /// The requested callback frame count is zero or exceeds [`MAX_MIX_FRAMES`].
    FrameCount {
        /// The unsupported frame count.
        frames: usize,
    },
    /// A physical sidechain channel does not exist on the opened input device.
    SidechainChannelUnavailable {
        /// Rack position in the prepared graph.
        rack_index: usize,
        /// Plug-in slot position in the rack.
        slot_index: usize,
        /// Zero-based selected input channel.
        channel: u8,
        /// Number of channels exposed by the input device.
        available: usize,
    },
    /// An input, output, or wet buffer cannot represent the requested block.
    BufferLength,
    /// The prepared dry-delay geometry is invalid.
    DryDelay(DryDelayError),
}
impl std::fmt::Display for MixError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PhysicalChannelUnavailable {
                rack_index,
                channel,
                available,
                input,
            } => {
                let direction = if *input { "input" } else { "output" };
                write!(
                    formatter,
                    "rack {rack_index} {direction} channel {channel} is unavailable on a {available}-channel device"
                )
            }
            Self::SidechainChannelUnavailable {
                rack_index,
                slot_index,
                channel,
                available,
            } => write!(
                formatter,
                "rack {rack_index} slot {slot_index} sidechain input channel {channel} is unavailable on a {available}-channel device"
            ),
            Self::FrameCount { frames } => write!(formatter, "unsupported frame count {frames}"),
            Self::BufferLength => formatter.write_str("mix buffer length mismatch"),
            Self::DryDelay(error) => write!(formatter, "dry delay error: {error}"),
        }
    }
}
impl std::error::Error for MixError {}

/// Test-only scope instrumentation for allocation-guard harnesses. A test allocator can query
/// [`is_active`] and fail immediately if it services an allocation during renderer/mixer work.
#[cfg(test)]
pub mod realtime_allocation_guard {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    pub struct Operation;
    impl Operation {
        pub(crate) fn enter() -> Self {
            ACTIVE.fetch_add(1, Ordering::SeqCst);
            Self
        }
    }
    impl Drop for Operation {
        fn drop(&mut self) {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }
    #[must_use]
    pub fn is_active() -> bool {
        ACTIVE.load(Ordering::SeqCst) != 0
    }

    #[test]
    fn reports_scope_activity() {
        assert!(!is_active());
        let _operation = Operation::enter();
        assert!(is_active());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JOIN_FADE_FRAMES, MAX_DRY_DELAY_FRAMES, MAX_MIX_FRAMES, MIX_CHANNELS, MixError,
        RackAudioSource, RealtimeRackMixer, WET_RECOVERY_BLOCKS,
    };
    use crate::{FallbackReason, GateOutcome, PreparedGraph};
    use sp_model::{
        ChannelLayout, Endpoint, EndpointId, NormalizedParameters, PhysicalChannels,
        PluginDescriptor, PluginFingerprint, PluginIdentity, PluginInstanceId, PluginSlot, Rack,
        RackChannelRoute, RackId, RackTopology, Session, SlotSidechain, Source, SourceId,
    };
    /// Per-rack mixer inputs on the heap: 64 racks of stereo blocks are too large for a stack
    /// array.
    fn rack_inputs(value: f32) -> Box<[[f32; MAX_MIX_FRAMES * MIX_CHANNELS]; sp_model::MAX_RACKS]> {
        vec![[value; MAX_MIX_FRAMES * MIX_CHANNELS]; sp_model::MAX_RACKS]
            .into_boxed_slice()
            .try_into()
            .expect("one input per rack")
    }

    fn session(layout: ChannelLayout, endpoint_layout: ChannelLayout) -> Session {
        let mut s = Session::new();
        s.sources.push(Source {
            id: SourceId("in".into()),
            name: "In".into(),
            layout,
        });
        s.endpoints.push(Endpoint {
            id: EndpointId("out".into()),
            name: "Out".into(),
            layout: endpoint_layout,
        });
        s.racks.push(Rack {
            id: RackId("r0".into()),
            name: "Rack".into(),
            topology: RackTopology::Serial,
            source_id: SourceId("in".into()),
            endpoint_id: EndpointId("out".into()),
            gain_db: sp_model::GainDb::default(),
            muted: false,
            bypassed: false,
            slots: Vec::new(),
        });
        s
    }
    #[test]
    fn live_topology_keeps_continuing_racks_and_fades_in_new_ones() {
        use super::{MixerLane, RackSettings};
        let mut three = session(ChannelLayout::Stereo, ChannelLayout::Stereo);
        for id in ["r1", "r2"] {
            let mut rack = three.racks[0].clone();
            rack.id = sp_model::RackId(id.into());
            three.racks.push(rack);
        }
        let mut mixer = RealtimeRackMixer::new(PreparedGraph::compile(&three).unwrap());
        for (rack, gain) in [(0, 0.5), (1, 0.25), (2, 0.125)] {
            mixer.set_rack_gain(rack, gain);
            mixer.set_rack_bypassed(rack, true);
        }
        let input = [1.0_f32; 128 * MIX_CHANNELS];
        let mut output = [0.0_f32; 128 * MIX_CHANNELS];
        for _ in 0..4 {
            mixer.render_block(&input, &[], &mut output, 128).unwrap();
        }
        assert!((output[254] - 0.875).abs() < 1e-6, "three racks summed");

        // Remove the middle rack and add a new one at the end, while running.
        let mut two_plus_one = three.clone();
        two_plus_one.racks.remove(1);
        let mut added = three.racks[0].clone();
        added.id = sp_model::RackId("r3".into());
        two_plus_one.racks.push(added);
        let mut lanes = [MixerLane::FRESH; sp_model::MAX_RACKS];
        lanes[0].from = Some(0);
        lanes[1].from = Some(2);
        lanes[2] = MixerLane {
            settings: RackSettings {
                gain: 1.0,
                bypassed: true,
                ..RackSettings::default()
            },
            ..MixerLane::FRESH
        };
        mixer.apply_topology(PreparedGraph::compile(&two_plus_one).unwrap(), &lanes);
        assert_eq!(
            mixer.rack_settings(0).unwrap().gain.to_bits(),
            0.5_f32.to_bits()
        );
        assert_eq!(
            mixer.rack_settings(1).unwrap().gain.to_bits(),
            0.125_f32.to_bits()
        );

        mixer.render_block(&input, &[], &mut output, 128).unwrap();
        // Continuing racks play at full level on the first block; the new rack fades in.
        assert!(
            (output[0] - 0.625).abs() < 1e-6,
            "no gap for continuing racks"
        );
        assert!(output[254] > 0.625 && output[254] <= 1.625);
        for _ in 0..2 {
            mixer.render_block(&input, &[], &mut output, 128).unwrap();
        }
        assert!(
            (output[254] - 1.625).abs() < 1e-6,
            "new rack reached full level"
        );
    }

    #[test]
    fn live_reroute_moves_output_without_a_silent_block() {
        use super::MixerLane;
        let mut routed = session(ChannelLayout::Stereo, ChannelLayout::Stereo);
        let route = |channel| RackChannelRoute {
            input: Some(PhysicalChannels::Mono { channel: 0 }),
            output: PhysicalChannels::Mono { channel },
        };
        routed.rack_routes.insert(RackId("r0".into()), route(1));
        let mut mixer = RealtimeRackMixer::new(PreparedGraph::compile(&routed).unwrap());
        mixer.set_rack_bypassed(0, true);
        let mut inputs = rack_inputs(0.0);
        inputs[0].fill(0.5);
        let mut output = [0.0; 128 * 4];
        for _ in 0..2 {
            mixer
                .render_routed_block(&inputs, &[], &mut output, 4, 128)
                .unwrap();
        }
        assert!((output[1] - 0.5).abs() < 1e-6);

        routed.rack_routes.insert(RackId("r0".into()), route(3));
        let mut lanes = [MixerLane::FRESH; sp_model::MAX_RACKS];
        lanes[0].from = Some(0);
        mixer.apply_topology(PreparedGraph::compile(&routed).unwrap(), &lanes);
        mixer
            .render_routed_block(&inputs, &[], &mut output, 4, 128)
            .unwrap();
        // The first block after the change is already on the new channel at full level.
        assert!(output[1].abs() < 1e-6, "old channel released");
        assert!((output[3] - 0.5).abs() < 1e-6, "new channel plays at once");
    }

    /// A rebuilt plug-in chain fades from its last played frame to dry, then fades to the new
    /// chain on its third valid block. Each fade lasts exactly `JOIN_FADE_FRAMES`.
    #[test]
    fn rebuilt_chain_fades_to_dry_then_to_the_new_chain_after_three_valid_blocks() {
        use super::MixerLane;
        let graph =
            PreparedGraph::compile(&session(ChannelLayout::Stereo, ChannelLayout::Stereo)).unwrap();
        let mut mixer = RealtimeRackMixer::new(graph);
        let frames = 128;
        let input = [0.5_f32; MAX_MIX_FRAMES * MIX_CHANNELS];
        let render = |mixer: &mut RealtimeRackMixer, wet_gain: Option<f32>| {
            let wet = input.map(|sample| sample * wet_gain.unwrap_or(0.0));
            let sources = [wet_gain.map_or(RackAudioSource::None, |_| RackAudioSource::Wet(&wet))];
            mixer.set_gate_outcome(
                0,
                if wet_gain.is_some() {
                    GateOutcome::WorkerResultAccepted
                } else {
                    GateOutcome::Awaiting
                },
            );
            let mut output = [0.0_f32; MAX_MIX_FRAMES * MIX_CHANNELS];
            mixer
                .render_block(&input[..frames * 2], &sources, &mut output, frames)
                .unwrap();
            output
        };
        for _ in 0..8 {
            render(&mut mixer, Some(0.3));
        }
        let mut lanes = [MixerLane::FRESH; sp_model::MAX_RACKS];
        lanes[0] = MixerLane {
            from: Some(0),
            restart: true,
            ..MixerLane::FRESH
        };
        mixer.apply_topology(
            PreparedGraph::compile(&session(ChannelLayout::Stereo, ChannelLayout::Stereo)).unwrap(),
            &lanes,
        );
        // Checks a fade that starts at `from` and reaches `to` at `JOIN_FADE_FRAMES`.
        let fades = |output: [f32; MAX_MIX_FRAMES * MIX_CHANNELS], from: f32, to: f32| {
            (output[0] - from).abs() < 1e-6
                // Interleaved stereo: the halfway frame starts at sample JOIN_FADE_FRAMES.
                && (output[JOIN_FADE_FRAMES] - f32::midpoint(from, to)).abs() < 1e-6
                && output[JOIN_FADE_FRAMES * 2..frames * 2]
                    .iter()
                    .all(|sample| (sample - to).abs() < 1e-6)
        };
        assert!(
            fades(render(&mut mixer, None), 0.15, 0.5),
            "old chain fades to dry"
        );
        for _ in 1..WET_RECOVERY_BLOCKS {
            let output = render(&mut mixer, Some(1.2));
            assert!(
                output[..frames * 2]
                    .iter()
                    .all(|sample| (sample - 0.5).abs() < 1e-6)
            );
        }
        assert!(
            fades(render(&mut mixer, Some(1.2)), 0.5, 0.6),
            "new chain fades in"
        );
    }

    /// A physical sidechain reads the block being dispatched. A rack sidechain reads the source
    /// rack's post-fader output from the block before; a muted source gives silence.
    #[test]
    #[allow(
        clippy::large_stack_arrays,
        reason = "the mixer API uses fixed per-rack input arrays"
    )]
    fn sidechains_read_this_blocks_input_and_the_previous_blocks_rack_output() {
        let mut session = session(ChannelLayout::Stereo, ChannelLayout::Stereo);
        let mut second = session.racks[0].clone();
        second.id = RackId("r1".into());
        session.racks.push(second);
        let slot = |sidechain| PluginSlot {
            id: PluginInstanceId("slot".into()),
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
            sidechain: Some(sidechain),
        };
        session.racks[0]
            .slots
            .push(slot(SlotSidechain::PhysicalInput(
                PhysicalChannels::Stereo { left: 2, right: 3 },
            )));
        session.racks[1]
            .slots
            .push(slot(SlotSidechain::RackOutput(RackId("r0".into()))));
        let mut mixer = RealtimeRackMixer::new(PreparedGraph::compile(&session).unwrap());
        assert!(mixer.validate_routes(4, 2).is_ok());
        assert_eq!(
            mixer.validate_routes(3, 2),
            Err(MixError::SidechainChannelUnavailable {
                rack_index: 0,
                slot_index: 0,
                channel: 3,
                available: 3,
            })
        );
        mixer.set_rack_bypassed(0, true);
        mixer.set_rack_gain(0, 0.5);

        let frames = 128;
        let device_input = |left: f32, right: f32| [0.0, 0.0, left, right].repeat(frames);
        let mut rack_inputs = rack_inputs(0.0);
        let mut output = [0.0; MAX_MIX_FRAMES * MIX_CHANNELS];
        let mut sidechain = [[1.0; MAX_MIX_FRAMES]; MIX_CHANNELS];
        let filled = |mixer: &RealtimeRackMixer, input: &[f32], rack, sidechain: &mut _| {
            assert!(
                mixer
                    .sidechain_sources(input, 4)
                    .fill(rack, 0, frames, sidechain)
            );
        };
        let holds = |sidechain: &[[f32; MAX_MIX_FRAMES]; MIX_CHANNELS], left: f32, right: f32| {
            sidechain
                .iter()
                .zip([left, right])
                .all(|(plane, expected)| {
                    plane[..frames]
                        .iter()
                        .all(|sample| (sample - expected).abs() < f32::EPSILON)
                })
        };

        // Nothing has been mixed yet, so the rack source is silent.
        filled(&mixer, &device_input(0.75, -0.75), 1, &mut sidechain);
        assert!(holds(&sidechain, 0.0, 0.0));
        assert!(
            !mixer
                .sidechain_sources(&[], 4)
                .fill(0, 1, frames, &mut sidechain)
        );
        for (block, (value, next)) in [(0.5, 0.125), (0.25, 0.0625)].into_iter().enumerate() {
            for frame in rack_inputs[0][..frames * 2].chunks_exact_mut(2) {
                frame.copy_from_slice(&[value, -value]);
            }
            mixer
                .render_routed_block(&rack_inputs, &[], &mut output, 2, frames)
                .unwrap();
            let input = device_input(next, -next);
            filled(&mixer, &input, 0, &mut sidechain);
            assert!(holds(&sidechain, next, -next), "same block");
            filled(&mixer, &input, 1, &mut sidechain);
            // The first block fades in from silence; after it the output is exact.
            if block > 0 {
                assert!(
                    holds(&sidechain, value * 0.5, -value * 0.5),
                    "previous block"
                );
            }
        }

        mixer.set_rack_muted(0, true);
        mixer
            .render_routed_block(&rack_inputs, &[], &mut output, 2, frames)
            .unwrap();
        filled(&mixer, &device_input(0.0, 0.0), 1, &mut sidechain);
        assert!(holds(&sidechain, 0.0, 0.0), "muted source");
    }

    #[test]
    fn routed_mixer_uses_selected_output_channel_and_validates_device() {
        let mut session = session(ChannelLayout::Stereo, ChannelLayout::Stereo);
        session.rack_routes.insert(
            RackId("r0".into()),
            RackChannelRoute {
                input: Some(PhysicalChannels::Mono { channel: 2 }),
                output: PhysicalChannels::Mono { channel: 3 },
            },
        );
        let graph = PreparedGraph::compile(&session).unwrap();
        let mut mixer = RealtimeRackMixer::new(graph);
        assert!(mixer.validate_routes(3, 4).is_ok());
        assert!(mixer.validate_routes(2, 4).is_err());
        assert!(mixer.validate_routes(3, 3).is_err());
        mixer.set_rack_bypassed(0, true);
        let mut inputs = rack_inputs(0.0);
        inputs[0].fill(0.25);
        let mut output = [0.0; 128 * 4];
        for _ in 0..2 {
            mixer
                .render_routed_block(&inputs, &[], &mut output, 4, 128)
                .unwrap();
        }
        for frame in 0..128 {
            assert_eq!(&output[frame * 4..frame * 4 + 4], &[0.0, 0.0, 0.0, 0.25]);
        }
    }
    #[test]
    fn routed_racks_keep_dry_inputs_independent_and_sum_shared_outputs() {
        let mut session = session(ChannelLayout::Stereo, ChannelLayout::Stereo);
        let mut second = session.racks[0].clone();
        second.id = RackId("r1".into());
        session.racks.push(second);
        for rack_id in ["r0", "r1"] {
            session.rack_routes.insert(
                RackId(rack_id.into()),
                RackChannelRoute {
                    input: Some(PhysicalChannels::Mono { channel: 0 }),
                    output: PhysicalChannels::Mono { channel: 1 },
                },
            );
        }
        let mut mixer = RealtimeRackMixer::new(PreparedGraph::compile(&session).unwrap());
        mixer.set_rack_bypassed(0, true);
        mixer.set_rack_bypassed(1, true);
        let mut inputs = rack_inputs(0.0);
        inputs[0].fill(0.25);
        inputs[1].fill(0.5);
        let mut output = [0.0; 128 * 4];
        for _ in 0..2 {
            mixer
                .render_routed_block(&inputs, &[], &mut output, 4, 128)
                .unwrap();
        }
        for frame in 0..128 {
            assert_eq!(&output[frame * 4..frame * 4 + 4], &[0.0, 0.75, 0.0, 0.0]);
        }
    }
    #[test]
    fn output_meter_includes_physical_channels_three_and_four() {
        let mut session = session(ChannelLayout::Stereo, ChannelLayout::Stereo);
        session.rack_routes.insert(
            RackId("r0".into()),
            RackChannelRoute {
                input: Some(PhysicalChannels::Stereo { left: 0, right: 1 }),
                output: PhysicalChannels::Stereo { left: 2, right: 3 },
            },
        );
        let mut mixer = RealtimeRackMixer::new(PreparedGraph::compile(&session).unwrap());
        mixer.set_rack_bypassed(0, true);
        let mut inputs = rack_inputs(0.0);
        for frame in 0..128 {
            inputs[0][frame * 2] = 0.5;
            inputs[0][frame * 2 + 1] = 1.25;
        }
        let mut output = [0.0; 128 * 4];
        for _ in 0..2 {
            mixer
                .render_routed_block(&inputs, &[], &mut output, 4, 128)
                .unwrap();
        }
        let meter = mixer.output_meter_snapshot();
        assert_eq!(
            meter.peak.map(f32::to_bits),
            [0.5_f32.to_bits(), 1.25_f32.to_bits()]
        );
        assert_eq!(
            meter.rms.map(f32::to_bits),
            [0.5_f32.to_bits(), 1.25_f32.to_bits()]
        );
        assert!(meter.clipped);
    }
    #[test]
    fn instrument_route_keeps_bypass_silent_without_input() {
        let mut session = session(ChannelLayout::Stereo, ChannelLayout::Stereo);
        session.rack_routes.insert(
            RackId("r0".into()),
            RackChannelRoute {
                input: None,
                output: PhysicalChannels::Stereo { left: 0, right: 1 },
            },
        );
        let mut mixer = RealtimeRackMixer::new(PreparedGraph::compile(&session).unwrap());
        mixer.set_rack_bypassed(0, true);
        let inputs = rack_inputs(1.0);
        let mut output = [0.0; 128 * 2];
        mixer
            .render_routed_block(&inputs, &[], &mut output, 2, 128)
            .unwrap();
        assert!(output.iter().all(|sample| *sample == 0.0));
    }
    #[test]
    fn wet_mix_scales_and_sums() {
        let graph =
            PreparedGraph::compile(&session(ChannelLayout::Stereo, ChannelLayout::Stereo)).unwrap();
        let mut mixer = RealtimeRackMixer::new(graph);
        mixer.set_gate_outcome(0, GateOutcome::WorkerResultAccepted);
        let wet = [0.5; 4];
        let input = [0.0; 4];
        let mut out = [0.0; 4];
        for _ in 0..WET_RECOVERY_BLOCKS {
            mixer
                .render_block(&input, &[RackAudioSource::Wet(&wet)], &mut out, 2)
                .unwrap();
        }
        assert!(out.iter().any(|x| *x > 0.0));
    }

    #[test]
    fn dry_bypass_preserves_frame_order_and_latency() {
        let graph =
            PreparedGraph::compile(&session(ChannelLayout::Stereo, ChannelLayout::Stereo)).unwrap();
        let mut mixer = RealtimeRackMixer::new(graph);
        mixer.set_rack_bypassed(0, true);
        let silence = [0.0; 256];
        let mut output = [0.0; 256];
        mixer.render_block(&silence, &[], &mut output, 128).unwrap();

        let mut input = [0.0; 256];
        for frame in 0..128 {
            let value = f32::from(u16::try_from(frame + 1).expect("frame fits in u16"));
            input[frame * 2] = value;
            input[frame * 2 + 1] = -value;
        }
        mixer.render_block(&input, &[], &mut output, 128).unwrap();
        assert_eq!(output.map(f32::to_bits), input.map(f32::to_bits));

        mixer.set_dry_delay_frames(0, 2);
        mixer.render_block(&input, &[], &mut output, 128).unwrap();
        assert_eq!(&output[..4], &[127.0, -127.0, 128.0, -128.0]);
        assert_eq!(&output[4..], &input[..252]);
    }

    #[test]
    fn dry_delay_preserves_impulse_across_large_and_variable_blocks() {
        let graph =
            PreparedGraph::compile(&session(ChannelLayout::Stereo, ChannelLayout::Stereo)).unwrap();
        for delay in [3_841, 4_095, MAX_DRY_DELAY_FRAMES] {
            for block_sizes in [&[MAX_MIX_FRAMES][..], &[64, 128, MAX_MIX_FRAMES][..]] {
                let mut mixer = RealtimeRackMixer::new(graph);
                mixer.set_rack_bypassed(0, true);
                mixer.set_dry_delay_frames(0, delay);
                let mut input = [0.0; MAX_MIX_FRAMES * MIX_CHANNELS];
                input[0] = 0.75;
                input[1] = -0.5;
                let mut output = [0.0; MAX_MIX_FRAMES * MIX_CHANNELS];
                let mut elapsed = 0;
                let mut block = 0;
                while elapsed <= delay + MAX_MIX_FRAMES {
                    let frames = block_sizes[block % block_sizes.len()];
                    mixer
                        .render_block(
                            &input[..frames * MIX_CHANNELS],
                            &[],
                            &mut output[..frames * MIX_CHANNELS],
                            frames,
                        )
                        .unwrap();
                    for frame in 0..frames {
                        let expected = if elapsed + frame == delay {
                            (0.75_f32, -0.5_f32)
                        } else {
                            (0.0_f32, 0.0_f32)
                        };
                        assert_eq!(
                            (output[frame * 2], output[frame * 2 + 1]),
                            expected,
                            "delay={delay}, elapsed={elapsed}, frame={frame}"
                        );
                    }
                    input.fill(0.0);
                    elapsed += frames;
                    block += 1;
                }
            }
        }
    }

    #[test]
    fn unsupported_latency_is_retained_and_never_reads_dry_audio() {
        let graph =
            PreparedGraph::compile(&session(ChannelLayout::Stereo, ChannelLayout::Stereo)).unwrap();
        for latency in [MAX_DRY_DELAY_FRAMES + 1, usize::try_from(u32::MAX).unwrap()] {
            let mut mixer = RealtimeRackMixer::new(graph);
            mixer.set_rack_bypassed(0, true);
            let input = [1.0; MAX_MIX_FRAMES * MIX_CHANNELS];
            let mut output = [0.0; MAX_MIX_FRAMES * MIX_CHANNELS];
            mixer
                .render_block(&input, &[], &mut output, MAX_MIX_FRAMES)
                .unwrap();
            assert!(output.iter().any(|sample| *sample > 0.0));

            mixer.set_dry_delay_frames(0, latency);
            assert_eq!(mixer.rack_settings(0).unwrap().latency_frames, latency);
            mixer
                .render_block(&input, &[], &mut output, MAX_MIX_FRAMES)
                .unwrap();
            assert!(output.iter().all(|sample| *sample == 0.0));

            mixer.set_rack_bypassed(0, false);
            mixer.set_gate_outcome(0, GateOutcome::WorkerResultAccepted);
            let wet = [0.5; MAX_MIX_FRAMES * MIX_CHANNELS];
            for _ in 0..WET_RECOVERY_BLOCKS {
                mixer
                    .render_block(
                        &input,
                        &[RackAudioSource::Wet(&wet)],
                        &mut output,
                        MAX_MIX_FRAMES,
                    )
                    .unwrap();
            }
            // The wet transition starts from Dry. Its first sample must still be silent.
            assert_eq!(output[0].to_bits(), 0.0_f32.to_bits());
            assert!(output.iter().all(|sample| (0.0..=0.5).contains(sample)));
        }
    }

    #[test]
    fn mono_rack_reads_left_lane_of_interleaved_worker_output() {
        let graph =
            PreparedGraph::compile(&session(ChannelLayout::Mono, ChannelLayout::Mono)).unwrap();
        let mut mixer = RealtimeRackMixer::new(graph);
        mixer.set_gate_outcome(0, GateOutcome::WorkerResultAccepted);
        let input = [0.0; 256];
        let mut wet = [0.0; 256];
        for frame in 0..128 {
            wet[frame * 2] = 0.25;
            wet[frame * 2 + 1] = 0.75;
        }
        let mut output = [0.0; 256];
        for _ in 0..=WET_RECOVERY_BLOCKS {
            mixer
                .render_block(&input, &[RackAudioSource::Wet(&wet)], &mut output, 128)
                .unwrap();
        }
        assert!(
            output
                .iter()
                .all(|sample| sample.to_bits() == 0.25_f32.to_bits())
        );
    }

    #[test]
    fn fallback_recovers_only_after_consecutive_wet_blocks() {
        let graph =
            PreparedGraph::compile(&session(ChannelLayout::Stereo, ChannelLayout::Stereo)).unwrap();
        let mut mixer = RealtimeRackMixer::new(graph);
        mixer.set_gate_outcome(0, GateOutcome::UseFallback(FallbackReason::DeadlineMiss));
        let input = [1.0; 2];
        let mut out = [0.0; 2];
        mixer
            .render_block(&input, &[RackAudioSource::None], &mut out, 1)
            .unwrap();
        mixer.set_gate_outcome(0, GateOutcome::WorkerResultAccepted);
        for _ in 0..WET_RECOVERY_BLOCKS - 1 {
            mixer
                .render_block(&input, &[RackAudioSource::Wet(&[0.0; 2])], &mut out, 1)
                .unwrap();
        }
        assert!(out[0] > 0.0);
    }
}
