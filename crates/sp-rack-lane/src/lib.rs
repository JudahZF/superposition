//! One rack lane, shared by Superposition's dispatcher and Lambda's insert task.
//!
//! A lane connects a host's real-time loop to one rack worker process through two stable
//! shared-memory banks. It holds:
//!
//! - [`RackLane`]: the bank pair, the [`RackGate`], the live ticket, the accepted wet output,
//!   non-blocking publish and consume, and the bank handoff for worker replacement;
//! - [`LaneFader`]: the latency-matched dry delay and the fades between dry and wet.
//!
//! The crate has no engine, session, or model type, so each host drives its lanes from its own
//! loop: Superposition from one audio callback, Lambda from the tasks of its DSP pool. Publish
//! and consume do not allocate, lock, or block.
//!
//! A request can carry the host's read deadline ([`RackLane::publish`]). Workers of a mixed rack
//! derive their deadlines from it ([`sp_shared_memory_os::Hop`]).

mod fader;
mod gate;
mod lane;

pub use fader::{
    FaderBlock, FaderSettings, JOIN_FADE_FRAMES, LaneFader, LaneFallback, LaneLayout,
    MAX_DRY_DELAY_FRAMES, MAX_TRANSITION_FRAMES, MIN_TRANSITION_FRAMES, WET_RECOVERY_BLOCKS,
};
pub use gate::{FallbackReason, GateOutcome, RackGate, RackGateState, WorkerObservation};
pub use lane::{LaneTelemetry, LiveRequest, RackLane};

/// Frames in the largest lane block: the shared-memory slot capacity.
pub const MAX_LANE_FRAMES: usize = sp_shared_memory::MAX_FRAMES;
/// Lane buffers are interleaved stereo; a mono lane uses the left channel.
pub const LANE_CHANNELS: usize = 2;
/// Samples in one interleaved lane block.
pub const LANE_SAMPLES: usize = MAX_LANE_FRAMES * LANE_CHANNELS;
