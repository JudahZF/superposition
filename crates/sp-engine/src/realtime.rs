//! Allocation-free product realtime mix path.
//!
//! After construction, [`RealtimeRackMixer::render_block`] only copies samples, advances
//! dry-delay indexes, and swaps graph generations. Callers supply per-rack wet audio and
//! gate outcomes; shared-memory publish/observe stays outside this crate.

use sp_model::MAX_RACKS;

use crate::{
    BlockActivation, DryDelayError, FallbackAudio, FallbackReason, GateOutcome, GraphArena,
    LiveBlockPlanner, PreparedGraph, RackBlockAction,
};

/// Maximum frames supported by the product mixer (256-frame mode).
pub const MAX_MIX_FRAMES: usize = 256;
/// Stereo channel count used by the product mixer.
pub const MIX_CHANNELS: usize = 2;
/// Dry-delay capacity frames (must exceed the largest reported rack latency).
const DRY_DELAY_CAPACITY_FRAMES: usize = 4_096;

/// Per-rack contribution supplied for one block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RackAudioSource<'a> {
    /// Validated wet output from a worker completion (interleaved stereo).
    Wet(&'a [f32]),
    /// No wet audio; mixer selects dry-delay or silence from the planner.
    None,
}

/// Fixed-capacity realtime mixer for the product `CoreAudio` path.
pub struct RealtimeRackMixer {
    arena: GraphArena,
    gate_outcomes: [GateOutcome; MAX_RACKS],
    planner: LiveBlockPlanner,
    dry_storage: Box<[Box<[f32]>; MAX_RACKS]>,
    dry_write_frame: [usize; MAX_RACKS],
    dry_delay_frames: [usize; MAX_RACKS],
    rack_gains: [f32; MAX_RACKS],
    rack_muted: [bool; MAX_RACKS],
    scratch: [f32; MAX_MIX_FRAMES * MIX_CHANNELS],
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
            dry_delay_frames: [0; MAX_RACKS],
            rack_gains: [1.0; MAX_RACKS],
            rack_muted: [false; MAX_RACKS],
            scratch: [0.0; MAX_MIX_FRAMES * MIX_CHANNELS],
        }
    }

    /// Stages a replacement graph for the next block boundary.
    pub fn stage_graph(&mut self, graph: PreparedGraph) {
        let rack_count = graph.rack_count();
        self.arena.stage(graph);
        for index in 0..rack_count {
            self.planner.set_dry_delay_available(index, true);
        }
    }

    /// Records the gate outcome observed for `rack_index` during this block.
    pub fn set_gate_outcome(&mut self, rack_index: usize, outcome: GateOutcome) {
        if let Some(slot) = self.gate_outcomes.get_mut(rack_index) {
            *slot = outcome;
        }
    }

    /// Sets the local dry-delay length used when a rack falls back.
    pub fn set_dry_delay_frames(&mut self, rack_index: usize, delay_frames: usize) {
        if let Some(slot) = self.dry_delay_frames.get_mut(rack_index) {
            *slot = delay_frames.min(DRY_DELAY_CAPACITY_FRAMES.saturating_sub(1));
        }
    }

    /// Sets linear gain for a rack (`1.0` = unity).
    pub fn set_rack_gain(&mut self, rack_index: usize, gain: f32) {
        if let Some(slot) = self.rack_gains.get_mut(rack_index) {
            *slot = if gain.is_finite() { gain } else { 0.0 };
        }
    }

    /// Mutes or unmutes a rack.
    pub fn set_rack_muted(&mut self, rack_index: usize, muted: bool) {
        if let Some(slot) = self.rack_muted.get_mut(rack_index) {
            *slot = muted;
        }
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
    /// `input` is the hardware/input bus used for dry-delay fallback.
    /// `sources` length should match the active rack count; missing entries are treated as
    /// [`RackAudioSource::None`].
    ///
    /// # Errors
    ///
    /// Returns [`MixError`] when buffer lengths are inconsistent with `frames`.
    pub fn render_block(
        &mut self,
        input: &[f32],
        sources: &[RackAudioSource<'_>],
        output: &mut [f32],
        frames: usize,
    ) -> Result<BlockActivation, MixError> {
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
            self.advance_dry(rack_index, input, frames)?;
            if self.rack_muted.get(rack_index).copied().unwrap_or(false) {
                continue;
            }
            let gain = self.rack_gains.get(rack_index).copied().unwrap_or(1.0);
            let action = self.action_for(rack_index);
            let source = sources
                .get(rack_index)
                .copied()
                .unwrap_or(RackAudioSource::None);
            match (action, source) {
                (
                    RackBlockAction::AcceptWorkerResult | RackBlockAction::Dispatch,
                    RackAudioSource::Wet(wet),
                ) => {
                    if wet.len() < samples {
                        return Err(MixError::BufferLength);
                    }
                    mix_scaled(wet, output, samples, gain);
                }
                (RackBlockAction::UseFallback(FallbackAudio::Silence), _) => {}
                (
                    RackBlockAction::UseFallback(FallbackAudio::DelayedDry)
                    | RackBlockAction::AwaitCompletion
                    | RackBlockAction::Dispatch
                    | RackBlockAction::AcceptWorkerResult,
                    _,
                ) => {
                    self.read_dry_into_scratch(rack_index, frames);
                    mix_scaled(&self.scratch[..samples], output, samples, gain);
                }
            }
        }
        Ok(activation)
    }

    fn advance_dry(
        &mut self,
        rack_index: usize,
        input: &[f32],
        frames: usize,
    ) -> Result<(), MixError> {
        let delay_frames = self.dry_delay_frames[rack_index];
        let capacity_frames = DRY_DELAY_CAPACITY_FRAMES;
        if delay_frames >= capacity_frames {
            return Err(MixError::DryDelay(DryDelayError::DelayExceedsCapacity {
                delay_frames,
                capacity_frames,
            }));
        }
        let storage = &mut self.dry_storage[rack_index];
        let mut write_frame = self.dry_write_frame[rack_index];
        for frame in 0..frames {
            for channel in 0..MIX_CHANNELS {
                let in_index = frame * MIX_CHANNELS + channel;
                let write_index = write_frame * MIX_CHANNELS + channel;
                storage[write_index] = input[in_index];
            }
            write_frame += 1;
            if write_frame == capacity_frames {
                write_frame = 0;
            }
        }
        self.dry_write_frame[rack_index] = write_frame;
        Ok(())
    }

    fn read_dry_into_scratch(&mut self, rack_index: usize, frames: usize) {
        let delay_frames = self.dry_delay_frames[rack_index];
        let capacity_frames = DRY_DELAY_CAPACITY_FRAMES;
        let write_frame = self.dry_write_frame[rack_index];
        let storage = &self.dry_storage[rack_index];
        for frame in 0..frames {
            let written_frame = (write_frame + capacity_frames - frames + frame) % capacity_frames;
            let read_frame = (written_frame + capacity_frames - delay_frames) % capacity_frames;
            for channel in 0..MIX_CHANNELS {
                let out_index = frame * MIX_CHANNELS + channel;
                let read_index = read_frame * MIX_CHANNELS + channel;
                self.scratch[out_index] = storage[read_index];
            }
        }
    }
}

fn mix_scaled(source: &[f32], output: &mut [f32], samples: usize, gain: f32) {
    for index in 0..samples {
        output[index] += source[index] * gain;
    }
}

/// Errors from [`RealtimeRackMixer::render_block`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixError {
    /// Frame count is zero or exceeds [`MAX_MIX_FRAMES`].
    FrameCount {
        /// Requested frames.
        frames: usize,
    },
    /// Input/output/wet slice length does not match frames × channels.
    BufferLength,
    /// Dry-delay geometry failed.
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

#[cfg(test)]
mod tests {
    use sp_model::{
        ChannelLayout, Endpoint, EndpointId, Rack, RackId, RackTopology, Session, Source, SourceId,
    };

    use super::{RackAudioSource, RealtimeRackMixer};
    use crate::{FallbackReason, GateOutcome, PreparedGraph};

    fn one_rack_session() -> Session {
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
        session.racks.push(Rack {
            id: RackId("r0".into()),
            name: "Rack 0".into(),
            topology: RackTopology::Serial,
            source_id: SourceId("in".into()),
            endpoint_id: EndpointId("out".into()),
            slots: Vec::new(),
        });
        session
    }

    #[test]
    fn silence_when_no_racks() {
        let graph = PreparedGraph::compile(&Session::new()).expect("empty graph");
        let mut mixer = RealtimeRackMixer::new(graph);
        let input = [0.25_f32, -0.25, 0.5, -0.5];
        let mut output = [1.0; 4];
        mixer
            .render_block(&input, &[], &mut output, 2)
            .expect("render");
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));
    }

    #[test]
    fn wet_mix_scales_and_sums() {
        let graph = PreparedGraph::compile(&one_rack_session()).expect("graph");
        let mut mixer = RealtimeRackMixer::new(graph);
        mixer.set_gate_outcome(0, GateOutcome::WorkerResultAccepted);
        mixer.set_rack_gain(0, 0.5);
        let input = [0.0_f32; 4];
        let wet = [0.5_f32, -0.5, 1.0, -1.0];
        let mut output = [0.0; 4];
        mixer
            .render_block(&input, &[RackAudioSource::Wet(&wet)], &mut output, 2)
            .expect("render");
        assert!((output[0] - 0.25).abs() < f32::EPSILON);
        assert!((output[1] + 0.25).abs() < f32::EPSILON);
        assert!((output[2] - 0.5).abs() < f32::EPSILON);
        assert!((output[3] + 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn delayed_dry_fallback_reads_prior_input() {
        let graph = PreparedGraph::compile(&one_rack_session()).expect("graph");
        let mut mixer = RealtimeRackMixer::new(graph);
        mixer.set_dry_delay_frames(0, 1);
        mixer.set_gate_outcome(0, GateOutcome::UseFallback(FallbackReason::DeadlineMiss));

        let block1 = [0.5_f32, -0.5];
        let mut out1 = [0.0; 2];
        mixer
            .render_block(&block1, &[RackAudioSource::None], &mut out1, 1)
            .expect("block1");
        assert!(out1.iter().all(|sample| sample.abs() < f32::EPSILON));

        let block2 = [1.0_f32, -1.0];
        let mut out2 = [0.0; 2];
        mixer
            .render_block(&block2, &[RackAudioSource::None], &mut out2, 1)
            .expect("block2");
        assert!((out2[0] - 0.5).abs() < f32::EPSILON);
        assert!((out2[1] + 0.5).abs() < f32::EPSILON);
    }
}
