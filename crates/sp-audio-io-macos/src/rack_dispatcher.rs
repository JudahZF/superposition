//! Fixed shared-memory rack dispatch for the product audio callback.

use std::{
    io,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use sp_engine::{
    FallbackReason, GateOutcome, RackAudioSource, RackGate, RackGateState, RealtimeRackMixer,
    WorkerObservation,
};
use sp_shared_memory::{BLOCK_SLOT_COUNT, BlockRequest, BlockTicket, MAX_RACKS, ProtocolError};
use sp_shared_memory_macos::{MonotonicClock, SharedMemoryRegion};

const STEREO_CHANNELS: u32 = 2;
const MAX_SAMPLES: usize = sp_engine::MAX_MIX_FRAMES * sp_engine::MIX_CHANNELS;

/// Lock-free counters for one rack, suitable for control-plane polling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RackDispatchTelemetry {
    /// Requests published to this rack bank.
    pub published: u64,
    /// Valid completions consumed from this rack bank.
    pub completed: u64,
    /// Requests that did not complete before the block deadline.
    pub deadline_misses: u64,
    /// Malformed or otherwise invalid protocol observations.
    pub protocol_rejections: u64,
    /// Completion tickets that did not match the live ticket.
    pub stale_completions: u64,
    /// Transitions to rack-local fallback.
    pub fallback_activations: u64,
}

#[derive(Default)]
struct AtomicRackDispatchTelemetry {
    published: AtomicU64,
    completed: AtomicU64,
    deadline_misses: AtomicU64,
    protocol_rejections: AtomicU64,
    stale_completions: AtomicU64,
    fallback_activations: AtomicU64,
}

impl AtomicRackDispatchTelemetry {
    fn snapshot(&self) -> RackDispatchTelemetry {
        RackDispatchTelemetry {
            published: self.published.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            deadline_misses: self.deadline_misses.load(Ordering::Relaxed),
            protocol_rejections: self.protocol_rejections.load(Ordering::Relaxed),
            stale_completions: self.stale_completions.load(Ordering::Relaxed),
            fallback_activations: self.fallback_activations.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy)]
struct LiveRequest {
    ticket: BlockTicket,
    slot_index: usize,
}

struct RackDispatchState {
    region: SharedMemoryRegion,
    gate: RackGate,
    live: Option<LiveRequest>,
    outcome: GateOutcome,
    wet: [f32; MAX_SAMPLES],
    wet_valid: bool,
    telemetry: AtomicRackDispatchTelemetry,
}

/// Preallocated host-side dispatcher for one shared-memory bank per rack.
///
/// It follows the same protocol sequence proven by `xtask phase1`: publish a timed request,
/// observe a completion through `completion_snapshot`, then consume only the exact live ticket.
pub struct RackSharedMemoryDispatcher {
    clock: MonotonicClock,
    racks: Vec<RackDispatchState>,
    block_index: u64,
}

impl RackSharedMemoryDispatcher {
    /// Creates a dispatcher from control-plane-created per-rack bank mappings.
    ///
    /// All heap storage and clock setup happen here, before the callback receives this value.
    ///
    /// # Errors
    ///
    /// Returns an error when there are more than the fixed rack capacity or macOS cannot
    /// initialize its shared monotonic clock.
    pub fn new(regions: Vec<SharedMemoryRegion>) -> io::Result<Self> {
        if regions.len() > MAX_RACKS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many rack shared-memory banks",
            ));
        }
        let clock = MonotonicClock::new()?;
        let mut racks = Vec::with_capacity(regions.len());
        for region in regions {
            racks.push(RackDispatchState {
                region,
                gate: RackGate::new(),
                live: None,
                outcome: GateOutcome::DispatchAllowed,
                wet: [0.0; MAX_SAMPLES],
                wet_valid: false,
                telemetry: AtomicRackDispatchTelemetry::default(),
            });
        }
        Ok(Self {
            clock,
            racks,
            block_index: 0,
        })
    }

    /// Returns the number of configured rack banks.
    #[must_use]
    pub fn rack_count(&self) -> usize {
        self.racks.len()
    }

    /// Returns a lock-free telemetry snapshot for one rack.
    #[must_use]
    pub fn telemetry(&self, rack_index: usize) -> Option<RackDispatchTelemetry> {
        self.racks
            .get(rack_index)
            .map(|rack| rack.telemetry.snapshot())
    }

    /// Dispatches one stereo input block and observes completions until `deadline`.
    ///
    /// This callback operation never allocates, locks, logs, sleeps, launches a process, or
    /// performs control IPC. `deadline` is a bounded per-block budget, not a worker wait.
    pub fn process_block(&mut self, input: &[f32], frames: usize, deadline: Duration) {
        if frames == 0 || frames > sp_engine::MAX_MIX_FRAMES || input.len() < frames * 2 {
            self.close_all_invalid();
            return;
        }

        for rack in &mut self.racks {
            rack.wet_valid = false;
        }
        let deadline_tick = self
            .clock
            .now_ticks()
            .saturating_add(self.clock.duration_to_ticks(deadline));
        self.observe_until(frames, deadline_tick);
        self.dispatch_open(input, frames);
        self.block_index = self.block_index.wrapping_add(1);
    }

    /// Applies the fixed gate outcomes to the existing realtime mixer.
    pub fn apply_to_mixer(&self, mixer: &mut RealtimeRackMixer) {
        for (index, rack) in self.racks.iter().enumerate() {
            mixer.set_gate_outcome(index, rack.outcome);
        }
    }

    /// Returns fixed wet buffers for the current callback block.
    #[must_use]
    pub fn sources(&self) -> [RackAudioSource<'_>; MAX_RACKS] {
        let mut sources = [RackAudioSource::None; MAX_RACKS];
        for (index, rack) in self.racks.iter().enumerate() {
            if rack.wet_valid {
                sources[index] = RackAudioSource::Wet(&rack.wet);
            }
        }
        sources
    }

    fn observe_until(&mut self, frames: usize, deadline_tick: u64) {
        loop {
            let mut awaiting = false;
            for rack in &mut self.racks {
                if rack.live.is_none() {
                    continue;
                }
                awaiting = true;
                observe_rack(rack, self.block_index, frames);
            }
            if !awaiting || self.clock.now_ticks() >= deadline_tick {
                break;
            }
        }
        for rack in &mut self.racks {
            if matches!(rack.gate.state(), RackGateState::Awaiting { .. }) {
                let prior = rack.gate.state();
                rack.outcome = rack.gate.deadline_expired(self.block_index);
                record_outcome(rack, prior, rack.outcome);
            }
        }
    }

    fn dispatch_open(&mut self, input: &[f32], frames: usize) {
        let request = BlockRequest {
            frame_count: u32::try_from(frames).unwrap_or(u32::MAX),
            input_channel_count: STEREO_CHANNELS,
            output_channel_count: STEREO_CHANNELS,
            midi_event_count: 0,
            event_count: 0,
            flags: 0,
        };
        let slot_index = usize::try_from(self.block_index).unwrap_or(usize::MAX) % BLOCK_SLOT_COUNT;
        for rack in &mut self.racks {
            if !matches!(rack.gate.state(), RackGateState::Open) {
                continue;
            }
            let Some(slot) = rack.region.bank_mut().slot_mut(slot_index) else {
                close_dispatch(rack, self.block_index);
                continue;
            };
            copy_interleaved_stereo_to_planar(input, frames, slot);
            match rack.region.bank_mut().request_block_at(
                slot_index,
                request,
                self.clock.now_ticks(),
            ) {
                Ok(ticket) => {
                    let prior = rack.gate.state();
                    rack.outcome = rack.gate.dispatch(ticket, self.block_index);
                    record_outcome(rack, prior, rack.outcome);
                    if rack.outcome == GateOutcome::DispatchAllowed {
                        rack.live = Some(LiveRequest { ticket, slot_index });
                        rack.telemetry.published.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(_) => close_dispatch(rack, self.block_index),
            }
        }
    }

    fn close_all_invalid(&mut self) {
        for rack in &mut self.racks {
            close_dispatch(rack, self.block_index);
        }
    }
}

fn copy_interleaved_stereo_to_planar(
    input: &[f32],
    frames: usize,
    slot: &mut sp_shared_memory::BlockSlot,
) {
    for frame in 0..frames {
        slot.input_audio[0][frame] = input[frame * 2];
        slot.input_audio[1][frame] = input[frame * 2 + 1];
    }
}

fn observe_rack(rack: &mut RackDispatchState, block_index: u64, frames: usize) {
    let live = rack.live.expect("live request checked before observation");
    let slot = rack
        .region
        .bank()
        .slot(live.slot_index)
        .expect("fixed slot index is in range");
    let observation = match slot.completion_snapshot() {
        Ok(Some(snapshot)) if snapshot.ticket != live.ticket => {
            WorkerObservation::Completed(snapshot.ticket)
        }
        Ok(Some(snapshot)) if !is_expected_stereo(snapshot.request, frames) => {
            WorkerObservation::ProtocolFault(ProtocolError::MalformedCompletion)
        }
        Ok(Some(_)) if !copy_finite_output(slot, &mut rack.wet, frames) => {
            WorkerObservation::ProtocolFault(ProtocolError::MalformedCompletion)
        }
        Ok(Some(_)) => match slot.consume_completion_timing(live.ticket) {
            Ok(_) => WorkerObservation::Completed(live.ticket),
            Err(ProtocolError::UnexpectedState | ProtocolError::Owned) => {
                WorkerObservation::Pending
            }
            Err(error) => WorkerObservation::ProtocolFault(error),
        },
        Ok(None) | Err(ProtocolError::Owned) => WorkerObservation::Pending,
        Err(error) => WorkerObservation::ProtocolFault(error),
    };
    let prior = rack.gate.state();
    rack.outcome = rack.gate.observe(observation, block_index);
    match rack.outcome {
        GateOutcome::WorkerResultAccepted => {
            rack.live = None;
            rack.wet_valid = true;
            rack.telemetry.completed.fetch_add(1, Ordering::Relaxed);
        }
        GateOutcome::Awaiting => {}
        GateOutcome::UseFallback(_) => {
            if matches!(observation, WorkerObservation::ProtocolFault(_)) {
                rack.telemetry
                    .protocol_rejections
                    .fetch_add(1, Ordering::Relaxed);
            }
            if matches!(observation, WorkerObservation::Completed(_)) {
                rack.telemetry
                    .stale_completions
                    .fetch_add(1, Ordering::Relaxed);
            }
            record_outcome(rack, prior, rack.outcome);
        }
        GateOutcome::DispatchAllowed => unreachable!("observation cannot open a dispatch gate"),
    }
}

fn is_expected_stereo(request: BlockRequest, frames: usize) -> bool {
    request.frame_count == u32::try_from(frames).unwrap_or(u32::MAX)
        && request.input_channel_count == STEREO_CHANNELS
        && request.output_channel_count == STEREO_CHANNELS
        && request.midi_event_count == 0
        && request.event_count == 0
}

fn copy_finite_output(
    slot: &sp_shared_memory::BlockSlot,
    wet: &mut [f32; MAX_SAMPLES],
    frames: usize,
) -> bool {
    for frame in 0..frames {
        let left = slot.output_audio[0][frame];
        let right = slot.output_audio[1][frame];
        if !left.is_finite() || !right.is_finite() {
            return false;
        }
        wet[frame * 2] = left;
        wet[frame * 2 + 1] = right;
    }
    true
}

fn close_dispatch(rack: &mut RackDispatchState, block_index: u64) {
    let prior = rack.gate.state();
    rack.outcome = rack.gate.dispatch(
        BlockTicket {
            generation: 0,
            sequence: 0,
        },
        block_index,
    );
    record_outcome(rack, prior, rack.outcome);
}

fn record_outcome(rack: &RackDispatchState, prior: RackGateState, outcome: GateOutcome) {
    if !matches!(prior, RackGateState::Closed { .. })
        && let GateOutcome::UseFallback(reason) = outcome
    {
        rack.telemetry
            .fallback_activations
            .fetch_add(1, Ordering::Relaxed);
        if reason == FallbackReason::DeadlineMiss {
            rack.telemetry
                .deadline_misses
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::Ordering, time::Duration};

    use sp_shared_memory_macos::SharedMemoryRegion;

    use super::{RackAudioSource, RackSharedMemoryDispatcher};

    fn dispatcher(racks: usize) -> Option<RackSharedMemoryDispatcher> {
        let regions = match (0..racks)
            .map(|index| SharedMemoryRegion::create(u64::try_from(index + 1).unwrap_or(u64::MAX)))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(regions) => regions,
            // The macOS sandbox used by CI can forbid POSIX shared-memory creation. The same
            // test remains deterministic on a normal macOS host and needs no audio device.
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("regions: {error}"),
        };
        Some(RackSharedMemoryDispatcher::new(regions).expect("dispatcher"))
    }

    fn complete(dispatcher: &mut RackSharedMemoryDispatcher, rack_index: usize, samples: [f32; 4]) {
        let live = dispatcher.racks[rack_index].live.expect("live request");
        let claimed_tick = dispatcher.clock.now_ticks();
        let slot = dispatcher.racks[rack_index]
            .region
            .bank_mut()
            .slot_mut(live.slot_index)
            .expect("slot");
        let ticket = slot
            .claim_for_processing_at(
                u32::try_from(rack_index + 1).unwrap_or(u32::MAX),
                claimed_tick,
            )
            .expect("claim");
        assert_eq!(ticket, live.ticket);
        slot.output_audio[0][0] = samples[0];
        slot.output_audio[1][0] = samples[1];
        slot.output_audio[0][1] = samples[2];
        slot.output_audio[1][1] = samples[3];
        slot.publish_completion_at(
            u32::try_from(rack_index + 1).unwrap_or(u32::MAX),
            ticket,
            dispatcher.clock.now_ticks(),
        )
        .expect("complete");
    }

    #[test]
    fn publishes_interleaved_input_as_planar_and_consumes_valid_stereo_wet() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let input = [0.25, -0.25, 0.5, -0.5];
        dispatcher.process_block(&input, 2, Duration::ZERO);
        let live = dispatcher.racks[0].live.expect("request");
        let slot = dispatcher.racks[0]
            .region
            .bank()
            .slot(live.slot_index)
            .expect("slot");
        assert_eq!(&slot.input_audio[0][..2], &[0.25, 0.5]);
        assert_eq!(&slot.input_audio[1][..2], &[-0.25, -0.5]);

        complete(&mut dispatcher, 0, [1.0, -1.0, 0.5, -0.5]);
        dispatcher.process_block(&input, 2, Duration::ZERO);
        let sources = dispatcher.sources();
        let RackAudioSource::Wet(wet) = sources[0] else {
            panic!("valid completion must provide wet audio");
        };
        assert_eq!(&wet[..4], &[1.0, -1.0, 0.5, -0.5]);
        assert_eq!(dispatcher.telemetry(0).expect("telemetry").completed, 1);
    }

    #[test]
    fn rejects_nonfinite_worker_output_without_poisoning_another_rack() {
        let Some(mut dispatcher) = dispatcher(2) else {
            return;
        };
        let input = [0.0; 4];
        dispatcher.process_block(&input, 2, Duration::ZERO);
        complete(&mut dispatcher, 0, [0.25, -0.25, 0.5, -0.5]);

        let live = dispatcher.racks[1].live.expect("live request");
        let slot = dispatcher.racks[1]
            .region
            .bank_mut()
            .slot_mut(live.slot_index)
            .expect("slot");
        let ticket = slot
            .claim_for_processing_at(2, dispatcher.clock.now_ticks())
            .expect("claim");
        slot.output_audio[0][0] = f32::NAN;
        slot.publish_completion_at(2, ticket, dispatcher.clock.now_ticks())
            .expect("complete");

        dispatcher.process_block(&input, 2, Duration::ZERO);
        let sources = dispatcher.sources();
        assert!(matches!(sources[0], RackAudioSource::Wet(_)));
        assert!(matches!(sources[1], RackAudioSource::None));
        assert_eq!(dispatcher.telemetry(0).expect("rack 0").completed, 1);
        let rack_one = dispatcher.telemetry(1).expect("rack 1");
        assert_eq!(rack_one.protocol_rejections, 1);
        assert_eq!(rack_one.fallback_activations, 1);
    }

    #[test]
    fn rejects_a_stale_completion_ticket() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let input = [0.0; 4];
        dispatcher.process_block(&input, 2, Duration::ZERO);
        let live = dispatcher.racks[0].live.expect("live request");
        complete(&mut dispatcher, 0, [0.0; 4]);
        dispatcher.racks[0]
            .region
            .bank_mut()
            .slot_mut(live.slot_index)
            .expect("slot")
            .metadata
            .completion_generation
            .store(live.ticket.generation.saturating_add(1), Ordering::Relaxed);

        dispatcher.process_block(&input, 2, Duration::ZERO);
        let telemetry = dispatcher.telemetry(0).expect("telemetry");
        assert_eq!(telemetry.stale_completions, 1);
        assert_eq!(telemetry.fallback_activations, 1);
    }
}
