//! Allocation-free product realtime mix path.
//!
//! Construction and graph staging may allocate. [`RealtimeRackMixer::render_block`] is a
//! fixed-capacity callback operation: it does not allocate, lock, or perform control IPC.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use sp_model::MAX_RACKS;

use crate::{
    BlockActivation, DryDelayError, FallbackAudio, FallbackReason, GateOutcome, GraphArena,
    LiveBlockPlanner, PreparedChannelConversion, PreparedChannelLayout, PreparedGraph,
    RackBlockAction,
};

/// Maximum frames supported by the product mixer (256-frame mode).
pub const MAX_MIX_FRAMES: usize = 256;
/// Stereo channel count used by the hardware output mixer.
pub const MIX_CHANNELS: usize = 2;
/// Minimum click-bounded transition length.
pub const MIN_TRANSITION_FRAMES: usize = 64;
/// Maximum click-bounded transition length.
pub const MAX_TRANSITION_FRAMES: usize = 128;
/// Number of consecutive valid wet blocks required after a fallback before wet recovery.
pub const WET_RECOVERY_BLOCKS: u8 = 3;
const DRY_DELAY_CAPACITY_FRAMES: usize = 4_096;

/// Live rack controls applied by the realtime mixer at the next render call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RackSettings {
    /// Linear gain (`1.0` is unity).
    pub gain: f32,
    /// Suppress this rack's contribution.
    pub muted: bool,
    /// Route latency-matched dry audio instead of worker wet output.
    pub bypassed: bool,
    /// Rack latency used by the local dry fallback and bypass path.
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

/// Per-rack contribution supplied for one block. Wet data uses the prepared endpoint layout.
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
    scratch: [f32; MAX_MIX_FRAMES * MIX_CHANNELS],
    last_wet: Box<[Box<[f32]>; MAX_RACKS]>,
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
            scratch: [0.0; MAX_MIX_FRAMES * MIX_CHANNELS],
            last_wet: Box::new(std::array::from_fn(|_| {
                vec![0.0_f32; MAX_MIX_FRAMES * MIX_CHANNELS].into_boxed_slice()
            })),
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

    /// Sets the fixed wet/dry transition length. Values are clamped to 64--128 frames.
    pub fn set_transition_frames(&mut self, frames: usize) {
        self.transition_frames = frames.clamp(MIN_TRANSITION_FRAMES, MAX_TRANSITION_FRAMES);
    }

    /// Selects delayed dry fallback for compatible effects or silence for instruments.
    pub fn set_dry_fallback_available(&mut self, rack_index: usize, available: bool) {
        self.planner.set_dry_delay_available(rack_index, available);
    }

    /// Replaces all live settings for one rack. Non-finite gain becomes silence and latency is bounded.
    pub fn set_rack_settings(&mut self, rack_index: usize, mut settings: RackSettings) {
        if let Some(slot) = self.settings.get_mut(rack_index) {
            settings.gain = if settings.gain.is_finite() {
                settings.gain
            } else {
                0.0
            };
            settings.latency_frames = settings.latency_frames.min(DRY_DELAY_CAPACITY_FRAMES - 1);
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
    /// Returns a lock-free snapshot of the final hardware-output meter.
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
            if activation.acknowledgement().is_some() && self.rendered_source[rack_index] != target
            {
                self.start_transition(rack_index, self.rendered_source[rack_index]);
            }
            if self.rendered_source[rack_index] != target && !self.transitions[rack_index].active()
            {
                self.start_transition(rack_index, self.rendered_source[rack_index]);
            }
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
            mix_scaled(&self.scratch[..samples], output, gain);
            self.rack_meters[rack_index].publish(meter(&self.scratch[..samples], frames, gain));
        }
        self.output_meter
            .publish(meter(&output[..samples], frames, 1.0));
        Ok(activation)
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

    fn start_transition(&mut self, rack: usize, from: RenderSource) {
        self.transitions[rack] = TransitionState {
            from,
            position: 0,
            frames: self.transition_frames,
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
                    let sample = samples.get(frame).copied().unwrap_or(0.0);
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
            );
            let (to_l, to_r) = self.source_frame(
                rack_index,
                source_layout,
                endpoint_layout,
                conversion,
                wet,
                target,
                frame,
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
    ) -> (f32, f32) {
        match source {
            RenderSource::Silence => (0.0, 0.0),
            RenderSource::Dry => self.dry_frame(rack, source_layout, conversion, frame),
            RenderSource::Wet => match wet {
                RackAudioSource::Wet(samples) => match endpoint_layout {
                    PreparedChannelLayout::Mono => {
                        let value = samples.get(frame).copied().unwrap_or(0.0);
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
    ) -> (f32, f32) {
        let written = (self.dry_write_frame[rack] + DRY_DELAY_CAPACITY_FRAMES
            - transition_frame_offset(frame))
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

/// The dry ring write index already advanced past this block, so reading frame `n` of the
/// current block means stepping back `frames - n` frames, i.e. an offset of `frame + 1` from
/// the end.
const fn transition_frame_offset(frame: usize) -> usize {
    frame.wrapping_add(1)
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

/// Errors from [`RealtimeRackMixer::render_block`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixError {
    /// The requested callback frame count is zero or exceeds [`MAX_MIX_FRAMES`].
    FrameCount {
        /// The unsupported frame count.
        frames: usize,
    },
    /// An input, output, or wet buffer cannot represent the requested block.
    BufferLength,
    /// The prepared dry-delay geometry is invalid.
    DryDelay(DryDelayError),
}
impl std::fmt::Display for MixError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
    use super::{RackAudioSource, RealtimeRackMixer, WET_RECOVERY_BLOCKS};
    use crate::{FallbackReason, GateOutcome, PreparedGraph};
    use sp_model::{
        ChannelLayout, Endpoint, EndpointId, Rack, RackId, RackTopology, Session, Source, SourceId,
    };
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
