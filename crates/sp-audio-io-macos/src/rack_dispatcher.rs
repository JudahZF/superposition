//! Fixed shared-memory rack dispatch for the product audio callback.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use sp_engine::{
    FallbackReason, GateOutcome, RackAudioSource, RackGate, RackGateState, RealtimeRackMixer,
    SidechainSources, WorkerObservation,
};
use sp_shared_memory::{
    BlockEvent, BlockRequest, BlockTicket, MAX_EVENTS, MAX_MIDI_EVENTS, MAX_RACKS, MidiEvent,
    ProtocolError, SlotState,
};
use sp_shared_memory_macos::{
    MappedBankMetadata, MappedRackBanks, MonotonicClock, RackRecoverySignal, RackRecoveryState,
    SharedMemoryRegion,
};

const STEREO_CHANNELS: u32 = 2;
const MAX_SAMPLES: usize = sp_engine::MAX_MIX_FRAMES * sp_engine::MIX_CHANNELS;

#[derive(Clone, Copy)]
enum RackInputs<'a> {
    Shared(&'a [f32]),
    PerRack(&'a [[f32; MAX_SAMPLES]; MAX_RACKS]),
}

/// Fixed callback-owned automation payload for one product rack.
#[derive(Clone, Copy)]
pub struct RackAutomationEvents {
    events: [BlockEvent; MAX_EVENTS],
    len: usize,
}

impl RackAutomationEvents {
    /// Creates an empty event payload.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            events: [BlockEvent {
                frame_offset: 0,
                event_type: 0,
                key: 0,
                value: 0.0,
                flags: 0,
            }; MAX_EVENTS],
            len: 0,
        }
    }

    /// Removes all events without touching the fixed backing storage.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Appends one event when capacity remains.
    pub fn push(&mut self, event: BlockEvent) -> bool {
        let Some(destination) = self.events.get_mut(self.len) else {
            return false;
        };
        *destination = event;
        self.len += 1;
        true
    }

    pub(crate) fn as_slice(&self) -> &[BlockEvent] {
        &self.events[..self.len]
    }
}

impl Default for RackAutomationEvents {
    fn default() -> Self {
        Self::new()
    }
}

static EMPTY_RACK_AUTOMATION: [RackAutomationEvents; MAX_RACKS] =
    [RackAutomationEvents::new(); MAX_RACKS];

/// Lock-free counters for one rack, suitable for control-plane polling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RackDispatchTelemetry {
    /// Requests published to this rack bank.
    pub published: u64,
    /// Failed kernel wake calls after publishing a request.
    pub wake_failures: u64,
    /// Longest observed kernel wake call in shared monotonic clock ticks.
    pub max_wake_ticks: u64,
    /// Valid completions consumed from this rack bank.
    pub completed: u64,
    /// Requests that did not complete before the block deadline.
    pub deadline_misses: u64,
    /// Deadline misses where the worker had not claimed the request.
    pub missed_unclaimed: u64,
    /// Deadline misses where the worker owned or was processing the request.
    pub missed_in_progress: u64,
    /// Deadline misses where completion became visible after the final sweep.
    pub missed_completed_late: u64,
    /// Callback block index of the most recent deadline miss.
    pub last_miss_block_index: u64,
    /// Request ticket sequence of the most recent deadline miss.
    pub last_miss_sequence: u64,
    /// Shared monotonic tick when the most recent missed request was published.
    pub last_miss_request_tick: u64,
    /// Shared monotonic tick when the worker claimed the most recent missed request, or zero.
    pub last_miss_claimed_tick: u64,
    /// Shared monotonic tick when the callback observed the most recent deadline miss.
    pub last_miss_observed_tick: u64,
    /// Worker loop phase observed at the last miss (0 scan, 1 control, 2 editor, 3 wait).
    pub last_miss_worker_phase: u64,
    /// Wake sequence last observed by the worker at the last miss.
    pub last_miss_worker_wait_sequence: u64,
    /// Producer wake sequence at the last miss.
    pub last_miss_wake_sequence: u64,
    /// Most recent worker loop start at the last miss.
    pub last_miss_worker_loop_tick: u64,
    /// Malformed or otherwise invalid protocol observations.
    pub protocol_rejections: u64,
    /// Completion tickets that did not match the live ticket.
    pub stale_completions: u64,
    /// Transitions to rack-local fallback.
    pub fallback_activations: u64,
    /// Callback blocks rendered while this rack's gate is closed.
    pub gate_closed_blocks: u64,
    /// Completion-observation sweeps that inspected this rack.
    pub completion_observation_passes: u64,
    /// Observations that stopped because the absolute deadline was reached.
    pub completion_observation_deadline_stops: u64,
    /// Timed-out requested slots explicitly marked abandoned and retained for retirement.
    pub abandoned_requests: u64,
}

#[derive(Default)]
struct AtomicRackDispatchTelemetry {
    published: AtomicU64,
    wake_failures: AtomicU64,
    max_wake_ticks: AtomicU64,
    completed: AtomicU64,
    deadline_misses: AtomicU64,
    missed_unclaimed: AtomicU64,
    missed_in_progress: AtomicU64,
    missed_completed_late: AtomicU64,
    last_miss_block_index: AtomicU64,
    last_miss_sequence: AtomicU64,
    last_miss_request_tick: AtomicU64,
    last_miss_claimed_tick: AtomicU64,
    last_miss_observed_tick: AtomicU64,
    last_miss_worker_phase: AtomicU64,
    last_miss_worker_wait_sequence: AtomicU64,
    last_miss_wake_sequence: AtomicU64,
    last_miss_worker_loop_tick: AtomicU64,
    protocol_rejections: AtomicU64,
    stale_completions: AtomicU64,
    fallback_activations: AtomicU64,
    gate_closed_blocks: AtomicU64,
    completion_observation_passes: AtomicU64,
    completion_observation_deadline_stops: AtomicU64,
    abandoned_requests: AtomicU64,
}

impl AtomicRackDispatchTelemetry {
    fn snapshot(&self) -> RackDispatchTelemetry {
        let deadline_misses = self.deadline_misses.load(Ordering::Relaxed);
        let (
            missed_unclaimed,
            missed_in_progress,
            missed_completed_late,
            last_miss_block_index,
            last_miss_sequence,
            last_miss_request_tick,
            last_miss_claimed_tick,
            last_miss_observed_tick,
        ) = if deadline_misses == 0 {
            (0, 0, 0, 0, 0, 0, 0, 0)
        } else {
            (
                self.missed_unclaimed.load(Ordering::Relaxed),
                self.missed_in_progress.load(Ordering::Relaxed),
                self.missed_completed_late.load(Ordering::Relaxed),
                self.last_miss_block_index.load(Ordering::Relaxed),
                self.last_miss_sequence.load(Ordering::Relaxed),
                self.last_miss_request_tick.load(Ordering::Relaxed),
                self.last_miss_claimed_tick.load(Ordering::Relaxed),
                self.last_miss_observed_tick.load(Ordering::Relaxed),
            )
        };
        RackDispatchTelemetry {
            published: self.published.load(Ordering::Relaxed),
            wake_failures: self.wake_failures.load(Ordering::Relaxed),
            max_wake_ticks: self.max_wake_ticks.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            deadline_misses,
            missed_unclaimed,
            missed_in_progress,
            missed_completed_late,
            last_miss_block_index,
            last_miss_sequence,
            last_miss_request_tick,
            last_miss_claimed_tick,
            last_miss_observed_tick,
            last_miss_worker_phase: self.last_miss_worker_phase.load(Ordering::Relaxed),
            last_miss_worker_wait_sequence: self
                .last_miss_worker_wait_sequence
                .load(Ordering::Relaxed),
            last_miss_wake_sequence: self.last_miss_wake_sequence.load(Ordering::Relaxed),
            last_miss_worker_loop_tick: self.last_miss_worker_loop_tick.load(Ordering::Relaxed),
            protocol_rejections: self.protocol_rejections.load(Ordering::Relaxed),
            stale_completions: self.stale_completions.load(Ordering::Relaxed),
            fallback_activations: self.fallback_activations.load(Ordering::Relaxed),
            gate_closed_blocks: self.gate_closed_blocks.load(Ordering::Relaxed),
            completion_observation_passes: self
                .completion_observation_passes
                .load(Ordering::Relaxed),
            completion_observation_deadline_stops: self
                .completion_observation_deadline_stops
                .load(Ordering::Relaxed),
            abandoned_requests: self.abandoned_requests.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy)]
struct LiveRequest {
    bank_index: usize,
    ticket: BlockTicket,
    slot_index: usize,
    request: BlockRequest,
}

struct RackDispatchState {
    rack_index: usize,
    banks: MappedRackBanks,
    recovery: Option<Arc<RackRecoverySignal>>,
    gate: RackGate,
    live: Option<LiveRequest>,
    outcome: GateOutcome,
    wet: [f32; MAX_SAMPLES],
    wet_valid: bool,
    telemetry: AtomicRackDispatchTelemetry,
}

impl RackDispatchState {
    fn new(
        rack_index: usize,
        banks: MappedRackBanks,
        recovery: Option<Arc<RackRecoverySignal>>,
    ) -> Self {
        Self {
            rack_index,
            banks,
            recovery,
            gate: RackGate::new(),
            live: None,
            outcome: GateOutcome::DispatchAllowed,
            wet: [0.0; MAX_SAMPLES],
            wet_valid: false,
            telemetry: AtomicRackDispatchTelemetry::default(),
        }
    }
}

/// One rack's dispatch lane, built on the control thread and moved into a running callback.
///
/// Dropping a lane unmaps its banks, so lanes the callback retires come back to the control
/// thread before they are dropped. Boxed so the callback only moves a pointer.
pub struct PreparedRackLane(Box<RackDispatchState>);

impl PreparedRackLane {
    /// Maps a live worker's two banks for callback dispatch.
    ///
    /// # Errors
    /// Returns an error for an unfinished handoff or an invalid bank pair.
    pub fn new(
        regions: [SharedMemoryRegion; 2],
        active_index: usize,
        recovery: Arc<RackRecoverySignal>,
    ) -> io::Result<Self> {
        if recovery.state() != RackRecoveryState::Idle {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "finish the rack bank handoff before adding it to live audio",
            ));
        }
        // SAFETY: the control plane supplies the index owned by the live worker; the opposite
        // bank has no worker.
        let banks = unsafe { MappedRackBanks::from_indexed_regions(regions, active_index) }?;
        Ok(Self(Box::new(RackDispatchState::new(
            0,
            banks,
            Some(recovery),
        ))))
    }

    /// The worker recovery signal that identifies this lane's worker.
    #[must_use]
    pub fn recovery(&self) -> Option<&Arc<RackRecoverySignal>> {
        self.0.recovery.as_ref()
    }
}

/// Where one rack position gets its dispatch lane after a live topology change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaneSource {
    /// No worker: the rack passes dry audio, or has no rack at all.
    Empty,
    /// The lane previously at this position keeps its worker.
    Keep(usize),
    /// The next lane from the change's new lanes, in position order.
    New,
}

/// Preallocated host-side dispatcher for two stable shared-memory banks per rack.
///
/// It follows the shared-memory block protocol: publish a timed request,
/// observe a completion through `completion_snapshot`, then consume only the exact live ticket.
pub struct RackSharedMemoryDispatcher {
    clock: MonotonicClock,
    #[allow(
        clippy::vec_box,
        reason = "live layout changes move lanes on the callback; a box moves one pointer"
    )]
    racks: Vec<Box<RackDispatchState>>,
    block_index: u64,
}

impl RackSharedMemoryDispatcher {
    /// Creates a dispatcher from control-plane-created active mappings.
    ///
    /// A distinct inactive mapping is allocated beside every supplied active mapping here,
    /// before the callback receives this value. New product code may instead construct the
    /// complete pairs and call [`Self::with_mapped_banks`].
    ///
    /// # Errors
    ///
    /// Returns an error when there are more than the fixed rack capacity, a backup mapping
    /// cannot be created, or macOS cannot initialize its shared monotonic clock.
    pub fn new(regions: Vec<SharedMemoryRegion>) -> io::Result<Self> {
        Self::new_indexed(regions.into_iter().enumerate().collect())
    }

    /// Creates a dispatcher while preserving each mapping's product rack index.
    ///
    /// # Errors
    ///
    /// Returns an error for a duplicate/out-of-range index or if a backup mapping cannot be
    /// created.
    pub fn new_indexed(regions: Vec<(usize, SharedMemoryRegion)>) -> io::Result<Self> {
        let mut banks = Vec::with_capacity(regions.len());
        for (rack_index, region) in regions {
            banks.push((rack_index, MappedRackBanks::with_active_region(region)?));
        }
        Self::with_indexed_mapped_banks(banks)
    }

    /// Creates a dispatcher from already-created stable active/inactive bank pairs.
    ///
    /// All mappings, heap storage, and clock setup happen before the callback receives this
    /// value. Each pair keeps both callback-visible mapping addresses stable across worker
    /// replacement.
    ///
    /// # Errors
    ///
    /// Returns an error when the rack count exceeds capacity or macOS cannot initialize its
    /// shared monotonic clock.
    pub fn with_mapped_banks(banks: Vec<MappedRackBanks>) -> io::Result<Self> {
        Self::with_indexed_mapped_banks(banks.into_iter().enumerate().collect())
    }

    /// Creates a dispatcher with stable bank pairs and lock-free recovery handshakes.
    ///
    /// # Errors
    /// Returns an error when the monotonic clock is unavailable.
    pub fn new_indexed_recoverable(
        banks: Vec<(usize, MappedRackBanks, Arc<RackRecoverySignal>)>,
    ) -> io::Result<Self> {
        Self::with_recovery_mapped_banks(
            banks
                .into_iter()
                .map(|(index, banks, recovery)| (index, banks, Some(recovery)))
                .collect(),
        )
    }

    /// Installs the control plane's fixed-order bank pairs and current active indices.
    ///
    /// The control plane must supply the live worker's index after retiring the opposite
    /// bank. Only one dispatcher may drive these mappings and recovery signals at a time.
    /// Construction does not start an audio device or dispatch an audio block.
    ///
    /// # Errors
    /// Returns an error for an unfinished handoff, invalid bank pair or rack index, or an
    /// unavailable monotonic clock.
    pub fn with_rack_banks(
        mappings: Vec<(
            usize,
            [SharedMemoryRegion; 2],
            usize,
            Arc<RackRecoverySignal>,
        )>,
    ) -> io::Result<Self> {
        let banks = mappings
            .into_iter()
            .map(|(rack_index, regions, active_index, recovery)| {
                if recovery.state() != RackRecoveryState::Idle {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "finish the rack bank handoff before preparing a new renderer",
                    ));
                }
                // SAFETY: the control plane supplies the fixed index owned by its live worker;
                // the opposite bank has no worker and has completed any prior retirement.
                unsafe { MappedRackBanks::from_indexed_regions(regions, active_index) }
                    .map(|banks| (rack_index, banks, recovery))
            })
            .collect::<io::Result<Vec<_>>>()?;
        Self::new_indexed_recoverable(banks)
    }

    fn with_indexed_mapped_banks(banks: Vec<(usize, MappedRackBanks)>) -> io::Result<Self> {
        Self::with_recovery_mapped_banks(
            banks
                .into_iter()
                .map(|(index, banks)| (index, banks, None))
                .collect(),
        )
    }

    fn with_recovery_mapped_banks(
        banks: Vec<(usize, MappedRackBanks, Option<Arc<RackRecoverySignal>>)>,
    ) -> io::Result<Self> {
        if banks.len() > MAX_RACKS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "too many rack shared-memory bank pairs",
            ));
        }
        let mut seen = [false; MAX_RACKS];
        for (rack_index, _, _) in &banks {
            let Some(slot) = seen.get_mut(*rack_index) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "rack shared-memory index is outside product capacity",
                ));
            };
            if std::mem::replace(slot, true) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "duplicate rack shared-memory index",
                ));
            }
        }
        let clock = MonotonicClock::new()?;
        // Full capacity up front: live topology changes push lanes on the callback.
        let mut racks = Vec::with_capacity(MAX_RACKS);
        for (rack_index, banks, recovery) in banks {
            racks.push(Box::new(RackDispatchState::new(
                rack_index, banks, recovery,
            )));
        }
        Ok(Self {
            clock,
            racks,
            block_index: 0,
        })
    }

    /// Moves lanes to their new positions, installs `new_lanes`, and moves every other lane
    /// into `retired`, at a callback block boundary.
    ///
    /// This never allocates when `retired` has capacity for [`MAX_RACKS`] lanes. Retired lanes
    /// release their live requests here; the caller returns them to the control thread.
    pub fn apply_topology(
        &mut self,
        sources: &[LaneSource; MAX_RACKS],
        new_lanes: &mut Vec<PreparedRackLane>,
        retired: &mut Vec<PreparedRackLane>,
    ) {
        for rack in &mut self.racks {
            let previous = rack.rack_index;
            rack.rack_index = sources
                .iter()
                .position(|source| *source == LaneSource::Keep(previous))
                .unwrap_or(usize::MAX);
        }
        while let Some(index) = self
            .racks
            .iter()
            .position(|rack| rack.rack_index == usize::MAX)
        {
            let mut rack = self.racks.swap_remove(index);
            abandon_live_request(&mut rack);
            retired.push(PreparedRackLane(rack));
        }
        // New lanes were queued in position order; install them from the back.
        for (position, source) in sources.iter().enumerate().rev() {
            if *source == LaneSource::New
                && let Some(PreparedRackLane(mut rack)) = new_lanes.pop()
            {
                rack.rack_index = position;
                self.racks.push(rack);
            }
        }
    }

    /// Returns the number of configured rack banks.
    #[must_use]
    pub fn rack_count(&self) -> usize {
        self.racks.len()
    }

    /// Advances only bank handshakes while the caller exclusively owns the stopped renderer.
    pub(crate) fn service_stopped_recoveries(
        &mut self,
    ) -> [Option<Arc<RackRecoverySignal>>; MAX_RACKS] {
        let mut attached = std::array::from_fn(|_| None);
        for rack in &mut self.racks {
            attached[rack.rack_index].clone_from(&rack.recovery);
            service_recovery(rack, self.block_index);
        }
        attached
    }

    /// Returns a lock-free telemetry snapshot for one rack.
    #[must_use]
    pub fn telemetry(&self, rack_index: usize) -> Option<RackDispatchTelemetry> {
        self.racks
            .iter()
            .find(|rack| rack.rack_index == rack_index)
            .map(|rack| rack.telemetry.snapshot())
    }

    /// Returns stable address, lifecycle, and generation metadata for a rack's two mappings.
    #[must_use]
    pub fn bank_metadata(&self, rack_index: usize) -> Option<[MappedBankMetadata; 2]> {
        self.racks
            .iter()
            .find(|rack| rack.rack_index == rack_index)
            .map(|rack| rack.banks.metadata())
    }

    /// Returns the non-active mapping name for a replacement worker launch.
    #[must_use]
    pub fn inactive_bank_name(&self, rack_index: usize) -> Option<&str> {
        self.racks
            .iter()
            .find(|rack| rack.rack_index == rack_index)
            .map(|rack| rack.banks.inactive_bank_name())
    }

    /// Reinitializes the inactive stable mapping for a replacement worker before callback use.
    ///
    /// The operation is a control-plane setup action. It cannot recycle abandoned or in-flight
    /// slots because the inactive mapping must be fully quiescent.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown rack or a bank that has not been safely retired.
    pub fn prepare_replacement_bank(
        &mut self,
        rack_index: usize,
        generation: u64,
    ) -> io::Result<MappedBankMetadata> {
        let rack = self
            .racks
            .iter_mut()
            .find(|rack| rack.rack_index == rack_index)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "rack index is outside dispatcher",
                )
            })?;
        rack.banks.prepare_inactive(generation)
    }

    /// Switches a faulted rack to its prepared replacement mapping at a callback boundary.
    ///
    /// The previous active mapping is retained as `Retiring`; callers must later use
    /// [`Self::retire_replaced_bank_after_worker_exit`] only after the old worker is reaped.
    ///
    /// # Errors
    ///
    /// Returns an error when a request is still live, the rack is not closed for replacement,
    /// or the inactive bank is not prepared.
    pub fn activate_prepared_replacement(
        &mut self,
        rack_index: usize,
    ) -> io::Result<MappedBankMetadata> {
        let rack = self
            .racks
            .iter_mut()
            .find(|rack| rack.rack_index == rack_index)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "rack index is outside dispatcher",
                )
            })?;
        if rack.live.is_some() || !matches!(rack.gate.state(), RackGateState::Closed { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "rack must close and retire its live request before bank replacement",
            ));
        }
        let retiring = rack.banks.activate_prepared()?;
        rack.gate.reset_after_replacement();
        rack.outcome = GateOutcome::DispatchAllowed;
        Ok(retiring)
    }

    /// Makes a previously replaced mapping inactive after its old worker exits and is reaped.
    ///
    /// # Safety
    ///
    /// The old worker must be reaped and all handles to its mapping closed before this call.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown rack or if the mapping is not the safely retired bank.
    pub unsafe fn retire_replaced_bank_after_worker_exit(
        &mut self,
        rack_index: usize,
        bank_index: usize,
        next_generation: u64,
    ) -> io::Result<MappedBankMetadata> {
        let rack = self
            .racks
            .iter_mut()
            .find(|rack| rack.rack_index == rack_index)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "rack index is outside dispatcher",
                )
            })?;
        // SAFETY: upheld by this method's safety contract after resolving the rack.
        unsafe {
            rack.banks
                .retire_after_worker_exit(bank_index, next_generation)
        }
    }

    /// Dispatches one stereo input block and observes completions until `deadline`.
    ///
    /// This callback operation never allocates, locks, logs, sleeps, launches a process, or
    /// performs control IPC. `deadline` is a bounded per-block budget, not a worker wait.
    pub fn process_block(
        &mut self,
        input: &[f32],
        midi: &[MidiEvent],
        frames: usize,
        deadline: Duration,
    ) {
        self.process_block_with_events(input, None, midi, &EMPTY_RACK_AUTOMATION, frames, deadline);
    }

    /// Dispatches audio, sidechains, MIDI, and rack-targeted automation for one callback block.
    pub fn process_block_with_events(
        &mut self,
        input: &[f32],
        sidechains: Option<&SidechainSources<'_>>,
        midi: &[MidiEvent],
        automation: &[RackAutomationEvents; MAX_RACKS],
        frames: usize,
        deadline: Duration,
    ) {
        self.process_block_inputs(
            RackInputs::Shared(input),
            sidechains,
            midi,
            automation,
            frames,
            deadline,
        );
    }

    /// Dispatches a distinct stereo input to each rack in one callback batch. `sidechains`
    /// fills the aux region of every sidechained plug-in slot; `None` sends no sidechain.
    pub fn process_block_with_rack_inputs(
        &mut self,
        inputs: &[[f32; MAX_SAMPLES]; MAX_RACKS],
        sidechains: Option<&SidechainSources<'_>>,
        midi: &[MidiEvent],
        automation: &[RackAutomationEvents; MAX_RACKS],
        frames: usize,
        deadline: Duration,
    ) {
        self.process_block_inputs(
            RackInputs::PerRack(inputs),
            sidechains,
            midi,
            automation,
            frames,
            deadline,
        );
    }

    fn process_block_inputs(
        &mut self,
        inputs: RackInputs<'_>,
        sidechains: Option<&SidechainSources<'_>>,
        midi: &[MidiEvent],
        automation: &[RackAutomationEvents; MAX_RACKS],
        frames: usize,
        deadline: Duration,
    ) {
        if frames == 0
            || frames > sp_engine::MAX_MIX_FRAMES
            || matches!(inputs, RackInputs::Shared(input) if input.len() < frames * 2)
            || midi.len() > MAX_MIDI_EVENTS
        {
            self.close_all_invalid();
            self.record_closed_gates();
            return;
        }

        for rack in &mut self.racks {
            rack.wet_valid = false;
            service_recovery(rack, self.block_index);
        }
        let deadline_tick = self
            .clock
            .now_ticks()
            .saturating_add(self.clock.duration_to_ticks(deadline));
        self.dispatch_open_inputs(inputs, sidechains, midi, automation, frames);
        self.observe_until(deadline_tick);
        self.record_closed_gates();
        self.block_index = self.block_index.wrapping_add(1);
    }

    /// Applies gate outcomes and the selected worker's published latency before mixing.
    pub fn apply_to_mixer(&self, mixer: &mut RealtimeRackMixer) {
        for rack in &self.racks {
            mixer.set_gate_outcome(rack.rack_index, rack.outcome);
            if rack.recovery.as_ref().is_some_and(|recovery| {
                matches!(
                    recovery.state(),
                    RackRecoveryState::QuiesceRequested
                        | RackRecoveryState::Quiescent
                        | RackRecoveryState::ReplacementReady { .. }
                )
            }) {
                // The control plane may replace/reset banks after the quiescence acknowledgement.
                continue;
            }
            let latency = rack
                .banks
                .active_bank()
                .bank()
                .header
                .worker_latency_samples
                .load(Ordering::Acquire);
            mixer.set_dry_delay_frames(
                rack.rack_index,
                usize::try_from(latency).unwrap_or(usize::MAX),
            );
        }
    }

    /// Returns fixed wet buffers for the current callback block.
    #[must_use]
    pub fn sources(&self) -> [RackAudioSource<'_>; MAX_RACKS] {
        let mut sources = [RackAudioSource::None; MAX_RACKS];
        for rack in &self.racks {
            if rack.wet_valid {
                sources[rack.rack_index] = RackAudioSource::Wet(&rack.wet);
            }
        }
        sources
    }

    fn observe_until(&mut self, deadline_tick: u64) {
        loop {
            let mut awaiting = false;
            for rack in &mut self.racks {
                if rack.live.is_none() {
                    continue;
                }
                awaiting = true;
                rack.telemetry
                    .completion_observation_passes
                    .fetch_add(1, Ordering::Relaxed);
                observe_rack(rack, self.block_index);
            }
            if !awaiting || self.racks.iter().all(|rack| rack.live.is_none()) {
                break;
            }
            if self.clock.now_ticks() >= deadline_tick {
                for rack in &self.racks {
                    if rack.live.is_some() {
                        rack.telemetry
                            .completion_observation_deadline_stops
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                break;
            }
            std::hint::spin_loop();
        }

        for rack in &mut self.racks {
            if matches!(rack.gate.state(), RackGateState::Awaiting { .. }) {
                record_deadline_diagnostics(rack, self.block_index, self.clock.now_ticks());
                abandon_live_request(rack);
                let prior = rack.gate.state();
                // The abandoned slot can never be reused, so the miss closes the rack
                // immediately; recovery requires a worker and bank replacement.
                rack.outcome = rack.gate.deadline_expired_hard(self.block_index);
                record_outcome(rack, prior, rack.outcome);
            }
        }
    }

    #[cfg(test)]
    fn dispatch_open(
        &mut self,
        input: &[f32],
        midi: &[MidiEvent],
        automation: &[RackAutomationEvents; MAX_RACKS],
        frames: usize,
    ) {
        self.dispatch_open_inputs(RackInputs::Shared(input), None, midi, automation, frames);
    }

    fn dispatch_open_inputs(
        &mut self,
        inputs: RackInputs<'_>,
        sidechains: Option<&SidechainSources<'_>>,
        midi: &[MidiEvent],
        automation: &[RackAutomationEvents; MAX_RACKS],
        frames: usize,
    ) {
        for rack in &mut self.racks {
            if rack.recovery.as_ref().is_some_and(|recovery| {
                matches!(
                    recovery.state(),
                    RackRecoveryState::QuiesceRequested
                        | RackRecoveryState::Quiescent
                        | RackRecoveryState::ReplacementReady { .. }
                )
            }) {
                continue;
            }
            if !matches!(rack.gate.state(), RackGateState::Open) {
                continue;
            }
            let bank_index = rack.banks.active_index();
            let Ok(Some(slot_index)) = free_slot_index(rack.banks.active_bank()) else {
                close_dispatch(rack, self.block_index);
                continue;
            };
            let Some(region) = rack.banks.bank_mut(bank_index) else {
                close_dispatch(rack, self.block_index);
                continue;
            };
            let Some(slot) = region.bank_mut().slot_mut(slot_index) else {
                close_dispatch(rack, self.block_index);
                continue;
            };
            let events = automation[rack.rack_index].as_slice();
            let input = match inputs {
                RackInputs::Shared(input) => input,
                RackInputs::PerRack(inputs) => &inputs[rack.rack_index],
            };
            copy_interleaved_stereo_to_planar(input, frames, slot);
            let sidechain_slots = sidechains.map_or(0, |sidechains| {
                copy_sidechains(sidechains, rack.rack_index, frames, slot)
            });
            let request = BlockRequest {
                frame_count: u32::try_from(frames).unwrap_or(u32::MAX),
                input_channel_count: STEREO_CHANNELS,
                output_channel_count: STEREO_CHANNELS,
                midi_event_count: u32::try_from(midi.len()).unwrap_or(u32::MAX),
                event_count: u32::try_from(events.len()).unwrap_or(u32::MAX),
                flags: 0,
                sidechain_slots,
            };
            slot.midi_events[..midi.len()].copy_from_slice(midi);
            slot.events[..events.len()].copy_from_slice(events);
            match rack
                .banks
                .bank_mut(bank_index)
                .expect("active mapped bank index is valid")
                .bank_mut()
                .request_block_at(slot_index, request, self.clock.now_ticks())
            {
                Ok(ticket) => {
                    let prior = rack.gate.state();
                    rack.outcome = rack.gate.dispatch(ticket, self.block_index);
                    record_outcome(rack, prior, rack.outcome);
                    if rack.outcome == GateOutcome::DispatchAllowed {
                        rack.live = Some(LiveRequest {
                            bank_index,
                            ticket,
                            slot_index,
                            request,
                        });
                        rack.telemetry.published.fetch_add(1, Ordering::Relaxed);
                        let started = self.clock.now_ticks();
                        let notified = rack.banks.active_bank().notify_request();
                        let wake_ticks = self.clock.now_ticks().saturating_sub(started);
                        rack.telemetry
                            .max_wake_ticks
                            .fetch_max(wake_ticks, Ordering::Relaxed);
                        if !notified {
                            rack.telemetry.wake_failures.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Err(_) => close_dispatch(rack, self.block_index),
            }
        }
    }

    fn close_all_invalid(&mut self) {
        for rack in &mut self.racks {
            abandon_live_request(rack);
            close_dispatch(rack, self.block_index);
        }
    }

    fn record_closed_gates(&self) {
        for rack in &self.racks {
            if matches!(rack.gate.state(), RackGateState::Closed { .. }) {
                rack.telemetry
                    .gate_closed_blocks
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn record_deadline_diagnostics(rack: &RackDispatchState, block_index: u64, observed_tick: u64) {
    let live = rack.live.expect("awaiting rack has a live request");
    let bank = rack
        .banks
        .bank(live.bank_index)
        .expect("live request refers to one stable bank")
        .bank();
    rack.telemetry.last_miss_worker_phase.store(
        u64::from(bank.header.worker_phase.load(Ordering::Acquire)),
        Ordering::Relaxed,
    );
    rack.telemetry.last_miss_worker_wait_sequence.store(
        u64::from(bank.header.worker_wait_sequence.load(Ordering::Acquire)),
        Ordering::Relaxed,
    );
    rack.telemetry.last_miss_wake_sequence.store(
        u64::from(bank.header.request_wake_sequence.load(Ordering::Acquire)),
        Ordering::Relaxed,
    );
    rack.telemetry.last_miss_worker_loop_tick.store(
        bank.header.worker_loop_tick.load(Ordering::Acquire),
        Ordering::Relaxed,
    );
    let slot = bank
        .slot(live.slot_index)
        .expect("fixed slot index is in range");
    let metadata = &slot.metadata;
    let owner = metadata.owner.load(Ordering::Acquire);
    let state = metadata.state();
    let counter = match state {
        Ok(SlotState::Requested) if owner == 0 => &rack.telemetry.missed_unclaimed,
        Ok(SlotState::Complete) => &rack.telemetry.missed_completed_late,
        _ => &rack.telemetry.missed_in_progress,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    rack.telemetry
        .last_miss_block_index
        .store(block_index, Ordering::Relaxed);
    rack.telemetry
        .last_miss_sequence
        .store(live.ticket.sequence, Ordering::Relaxed);
    rack.telemetry.last_miss_request_tick.store(
        metadata.request_published_tick.load(Ordering::Acquire),
        Ordering::Relaxed,
    );
    rack.telemetry.last_miss_claimed_tick.store(
        metadata.worker_claimed_tick.load(Ordering::Acquire),
        Ordering::Relaxed,
    );
    rack.telemetry
        .last_miss_observed_tick
        .store(observed_tick, Ordering::Relaxed);
}

fn service_recovery(rack: &mut RackDispatchState, block_index: u64) {
    let Some(recovery) = rack.recovery.clone() else {
        return;
    };
    match recovery.state() {
        RackRecoveryState::Idle
        | RackRecoveryState::ReplacementActive { .. }
        | RackRecoveryState::Quiescent => {}
        RackRecoveryState::QuiesceRequested => {
            abandon_live_request(rack);
            if !matches!(rack.gate.state(), RackGateState::Closed { .. }) {
                close_dispatch(rack, block_index);
            }
            let _ = recovery.mark_quiescent();
        }
        RackRecoveryState::ReplacementReady {
            bank_index,
            generation,
        } => {
            if rack.live.is_some() || !matches!(rack.gate.state(), RackGateState::Closed { .. }) {
                return;
            }
            if rack
                .banks
                .acknowledge_external_prepare(bank_index, generation)
                .is_ok()
                && let Ok(retiring) = rack.banks.activate_prepared()
            {
                rack.gate.reset_after_replacement();
                rack.outcome = GateOutcome::DispatchAllowed;
                let _ = recovery.mark_replacement_active(retiring.identity.index);
            }
        }
        RackRecoveryState::RetiredReset {
            bank_index,
            generation,
        } => {
            if rack
                .banks
                .acknowledge_external_retirement(bank_index, generation)
                .is_ok()
                && recovery.complete_retirement()
                && matches!(rack.gate.state(), RackGateState::Closed { .. })
            {
                // A replacement can itself miss its first deadline while the old
                // bank is still retiring. Requeue recovery once this handshake is idle.
                let _ = recovery.request_quiesce();
            }
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

/// Fills the aux region of each sidechained plug-in slot and returns their bitmask.
fn copy_sidechains(
    sidechains: &SidechainSources<'_>,
    rack_index: usize,
    frames: usize,
    slot: &mut sp_shared_memory::BlockSlot,
) -> u32 {
    let mut sidechain_slots = 0;
    for (plugin, audio) in slot.sidechain_audio.iter_mut().enumerate() {
        if sidechains.fill(rack_index, plugin, frames, audio) {
            sidechain_slots |= 1 << plugin;
        }
    }
    sidechain_slots
}

fn free_slot_index(region: &SharedMemoryRegion) -> Result<Option<usize>, ProtocolError> {
    for (slot_index, slot) in region.bank().slots.iter().enumerate() {
        match slot.metadata.state()? {
            SlotState::Free => return Ok(Some(slot_index)),
            SlotState::Requested
            | SlotState::Processing
            | SlotState::Complete
            | SlotState::Abandoned => {}
        }
    }
    Ok(None)
}

/// Abandons only a still-unclaimed request and forgets the callback's live ticket.
///
/// A processing, complete, malformed, or already abandoned slot intentionally remains intact.
/// Its bank stays retired until the worker has exited, so no late result can be reused.
fn abandon_live_request(rack: &mut RackDispatchState) {
    let Some(live) = rack.live.take() else {
        return;
    };
    let Some(slot) = rack
        .banks
        .bank(live.bank_index)
        .and_then(|region| region.bank().slot(live.slot_index))
    else {
        return;
    };
    if slot.metadata.state() == Ok(SlotState::Requested)
        && slot.abandon_request(live.ticket).is_ok()
    {
        rack.telemetry
            .abandoned_requests
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Observes one nonblocking completion sweep for a live rack request.
fn observe_rack(rack: &mut RackDispatchState, block_index: u64) {
    let live = rack.live.expect("live request checked before observation");
    let slot = rack
        .banks
        .bank(live.bank_index)
        .expect("live request refers to one stable bank")
        .bank()
        .slot(live.slot_index)
        .expect("fixed slot index is in range");
    let observation = match slot.completion_snapshot() {
        Ok(Some(snapshot)) if snapshot.ticket != live.ticket => {
            WorkerObservation::Completed(snapshot.ticket)
        }
        Ok(Some(snapshot)) if snapshot.request != live.request => {
            WorkerObservation::ProtocolFault(ProtocolError::MalformedCompletion)
        }
        Ok(Some(_))
            if !copy_finite_output(slot, &mut rack.wet, live.request.frame_count as usize) =>
        {
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
            abandon_live_request(rack);
            record_outcome(rack, prior, rack.outcome);
        }
        GateOutcome::DispatchAllowed => unreachable!("observation cannot open a dispatch gate"),
    }
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
        // A hard deadline miss abandons the live slot and closes this gate. The stable
        // bank must be retired before any further request can reach this rack.
        if let Some(recovery) = &rack.recovery {
            let _ = recovery.request_quiesce();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };

    use sp_shared_memory::{BLOCK_EVENT_PARAMETER, BlockEvent, MAX_RACKS, SlotState};
    use sp_shared_memory_macos::{
        MappedBankLifecycle, MappedRackBanks, RackRecoverySignal, RackRecoveryState,
        SharedMemoryRegion,
    };

    use super::{MidiEvent, RackAudioSource, RackAutomationEvents, RackSharedMemoryDispatcher};

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
            .banks
            .bank_mut(live.bank_index)
            .expect("live bank")
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

    fn publish(dispatcher: &mut RackSharedMemoryDispatcher, input: &[f32], midi: &[MidiEvent]) {
        dispatcher.dispatch_open(input, midi, &super::EMPTY_RACK_AUTOMATION, 2);
    }

    #[test]
    fn observation_uses_deadline_before_abandoning_unfinished_request() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let input = [0.0; 4];
        publish(&mut dispatcher, &input, &[]);
        let live = dispatcher.racks[0].live.expect("request");

        dispatcher.process_block(&input, &[], 2, Duration::from_millis(2));
        let telemetry = dispatcher.telemetry(0).expect("telemetry");
        assert!(telemetry.completion_observation_passes > 2);
        assert_eq!(telemetry.completion_observation_deadline_stops, 1);
        assert_eq!(telemetry.abandoned_requests, 1);
        assert_eq!(telemetry.deadline_misses, 1);
        assert_eq!(telemetry.missed_unclaimed, 1);
        assert_eq!(telemetry.missed_in_progress, 0);
        assert_eq!(telemetry.missed_completed_late, 0);
        assert_eq!(telemetry.last_miss_sequence, live.ticket.sequence);
        assert!(telemetry.last_miss_request_tick > 0);
        assert_eq!(telemetry.last_miss_claimed_tick, 0);
        assert!(telemetry.last_miss_observed_tick >= telemetry.last_miss_request_tick);
        assert!(dispatcher.racks[0].live.is_none());
        assert_eq!(
            dispatcher.racks[0]
                .banks
                .bank(live.bank_index)
                .expect("live bank")
                .bank()
                .slot(live.slot_index)
                .expect("slot")
                .metadata
                .state(),
            Ok(SlotState::Abandoned)
        );
    }

    #[test]
    fn deadline_diagnostics_distinguish_claimed_request() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let input = [0.0; 4];
        publish(&mut dispatcher, &input, &[]);
        let live = dispatcher.racks[0].live.expect("request");
        let slot = dispatcher.racks[0]
            .banks
            .bank(live.bank_index)
            .expect("live bank")
            .bank()
            .slot(live.slot_index)
            .expect("slot");
        slot.claim_for_processing_at(1, dispatcher.clock.now_ticks())
            .expect("claim");

        dispatcher.process_block(&input, &[], 2, Duration::ZERO);

        let telemetry = dispatcher.telemetry(0).expect("telemetry");
        assert_eq!(telemetry.deadline_misses, 1);
        assert_eq!(telemetry.missed_unclaimed, 0);
        assert_eq!(telemetry.missed_in_progress, 1);
        assert_eq!(telemetry.missed_completed_late, 0);
        assert_eq!(telemetry.abandoned_requests, 0);
        assert_eq!(telemetry.last_miss_sequence, live.ticket.sequence);
        assert!(telemetry.last_miss_request_tick > 0);
        assert!(telemetry.last_miss_claimed_tick >= telemetry.last_miss_request_tick);
        assert!(telemetry.last_miss_observed_tick >= telemetry.last_miss_claimed_tick);
    }

    #[test]
    fn sparse_product_rack_indices_are_preserved() {
        let region = match SharedMemoryRegion::create(1) {
            Ok(region) => region,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("region: {error}"),
        };
        let mut dispatcher =
            RackSharedMemoryDispatcher::new_indexed(vec![(3, region)]).expect("indexed dispatcher");
        let input = [0.0; 4];
        publish(&mut dispatcher, &input, &[]);
        complete(&mut dispatcher, 0, [1.0, 2.0, 3.0, 4.0]);
        dispatcher.process_block(&input, &[], 2, Duration::ZERO);

        let sources = dispatcher.sources();
        assert!(matches!(sources[0], RackAudioSource::None));
        assert!(matches!(sources[3], RackAudioSource::Wet(_)));
        assert!(dispatcher.telemetry(0).is_none());
        assert_eq!(
            dispatcher.telemetry(3).expect("rack telemetry").completed,
            1
        );
    }

    #[test]
    fn product_dispatch_copies_bounded_midi_into_each_worker_request() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let event = MidiEvent {
            frame_offset: 1,
            port: 0,
            data_length: 3,
            data: [0x90, 60, 100],
            flags: 0,
        };
        publish(&mut dispatcher, &[0.0; 4], &[event]);

        let live = dispatcher.racks[0].live.expect("live request");
        let slot = dispatcher.racks[0]
            .banks
            .bank(live.bank_index)
            .expect("bank")
            .bank()
            .slot(live.slot_index)
            .expect("slot");
        assert_eq!(slot.metadata.midi_event_count, 1);
        assert_eq!(slot.midi_events[0], event);
    }

    #[test]
    fn replacement_switches_generation_at_stable_addresses_and_retires_old_bank() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let initial = dispatcher.bank_metadata(0).expect("bank metadata");
        assert_eq!(initial[0].lifecycle, MappedBankLifecycle::Active);
        assert_eq!(initial[1].lifecycle, MappedBankLifecycle::Inactive);
        let prepared = dispatcher.prepare_replacement_bank(0, 11).expect("prepare");
        assert_eq!(prepared.address, initial[1].address);
        assert_eq!(prepared.identity.generation, 11);

        let input = [0.0; 4];
        publish(&mut dispatcher, &input, &[]);
        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
        let retiring = dispatcher
            .activate_prepared_replacement(0)
            .expect("activate replacement");
        assert_eq!(retiring.identity.index, 0);
        assert_eq!(retiring.address, initial[0].address);
        let switched = dispatcher.bank_metadata(0).expect("switched metadata");
        assert_eq!(switched[1].lifecycle, MappedBankLifecycle::Active);
        assert_eq!(switched[1].address, initial[1].address);
        assert_eq!(switched[1].identity.generation, 11);
        assert_eq!(switched[0].lifecycle, MappedBankLifecycle::Retiring);

        // SAFETY: this test never launches a worker for the replaced mapping.
        let retired = unsafe {
            dispatcher
                .retire_replaced_bank_after_worker_exit(0, retiring.identity.index, 13)
                .expect("retire old mapping")
        };
        assert_eq!(retired.address, initial[0].address);
        assert_eq!(retired.identity.generation, 13);
        assert_eq!(retired.lifecycle, MappedBankLifecycle::Inactive);
    }

    #[test]
    fn callback_acknowledges_external_replacement_at_block_boundaries() {
        let mut owners = match [SharedMemoryRegion::create(7), SharedMemoryRegion::create(8)] {
            [Ok(active), Ok(inactive)] => [active, inactive],
            [Err(error), _] | [_, Err(error)]
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return;
            }
            [Err(error), _] | [_, Err(error)] => panic!("regions: {error}"),
        };
        let callback_active = SharedMemoryRegion::open(owners[0].name()).expect("active mapping");
        let callback_inactive =
            SharedMemoryRegion::open(owners[1].name()).expect("inactive mapping");
        // SAFETY: no worker is launched in this test and the second mapping begins inactive.
        let banks = unsafe { MappedRackBanks::from_regions(callback_active, callback_inactive) }
            .expect("mapped pair");
        let recovery = Arc::new(RackRecoverySignal::new());
        let mut dispatcher = RackSharedMemoryDispatcher::new_indexed_recoverable(vec![(
            0,
            banks,
            Arc::clone(&recovery),
        )])
        .expect("dispatcher");

        assert!(recovery.request_quiesce());
        dispatcher.process_block(&[0.0; 4], &[], 2, Duration::ZERO);
        assert_eq!(recovery.state(), RackRecoveryState::Quiescent);

        // SAFETY: the callback acknowledged quiescence and no worker owns the inactive mapping.
        unsafe { owners[1].reset_after_worker_exit(11) }.expect("prepare replacement");
        assert!(recovery.publish_replacement(1, 11));
        dispatcher.process_block(&[0.0; 4], &[], 2, Duration::ZERO);
        assert_eq!(
            recovery.state(),
            RackRecoveryState::ReplacementActive {
                retiring_bank_index: 0,
            }
        );
        assert_eq!(dispatcher.racks[0].banks.active_index(), 1);

        // SAFETY: no worker exists and the callback switched away from the retired mapping.
        unsafe { owners[0].reset_after_worker_exit(13) }.expect("reset retired bank");
        assert!(recovery.publish_retired_reset(0, 13));
        dispatcher.process_block(&[0.0; 4], &[], 2, Duration::ZERO);
        assert_eq!(recovery.state(), RackRecoveryState::QuiesceRequested);
        assert_eq!(
            dispatcher.racks[0].banks.metadata()[0].lifecycle,
            MappedBankLifecycle::Inactive
        );
    }

    #[test]
    fn hard_deadline_miss_requests_bank_replacement() {
        let [active, inactive] = match [
            SharedMemoryRegion::create(21),
            SharedMemoryRegion::create(22),
        ] {
            [Ok(active), Ok(inactive)] => [active, inactive],
            [Err(error), _] | [_, Err(error)]
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return;
            }
            [Err(error), _] | [_, Err(error)] => panic!("regions: {error}"),
        };
        // SAFETY: both mappings are fresh and no worker has been launched.
        let banks =
            unsafe { MappedRackBanks::from_regions(active, inactive) }.expect("mapped pair");
        let recovery = Arc::new(RackRecoverySignal::new());
        let mut dispatcher = RackSharedMemoryDispatcher::new_indexed_recoverable(vec![(
            0,
            banks,
            Arc::clone(&recovery),
        )])
        .expect("dispatcher");
        dispatcher.process_block(&[0.0; 4], &[], 2, Duration::ZERO);

        assert_eq!(recovery.state(), RackRecoveryState::QuiesceRequested);
        assert_eq!(
            dispatcher.telemetry(0).expect("telemetry").deadline_misses,
            1
        );
        assert_eq!(
            dispatcher
                .telemetry(0)
                .expect("telemetry")
                .gate_closed_blocks,
            1
        );
        dispatcher.process_block(&[0.0; 4], &[], 2, Duration::ZERO);
        assert_eq!(recovery.state(), RackRecoveryState::Quiescent);
        assert_eq!(
            dispatcher
                .telemetry(0)
                .expect("telemetry")
                .gate_closed_blocks,
            2
        );
    }

    #[test]
    fn stopped_dispatcher_updates_bank_selector_before_same_renderer_resume() {
        let Some(mut dispatcher) = dispatcher(2) else {
            return;
        };
        let recovery = Arc::new(RackRecoverySignal::new());
        dispatcher.racks[0].recovery = Some(Arc::clone(&recovery));
        let mut active_owner =
            SharedMemoryRegion::open(dispatcher.racks[0].banks.bank(0).unwrap().name())
                .expect("active owner mapping");
        let mut inactive_owner =
            SharedMemoryRegion::open(dispatcher.racks[0].banks.bank(1).unwrap().name())
                .expect("inactive owner mapping");
        let other_metadata = dispatcher.bank_metadata(1).unwrap();

        assert!(recovery.request_quiesce());
        let attached = dispatcher.service_stopped_recoveries();
        assert!(
            attached[0]
                .as_ref()
                .is_some_and(|signal| Arc::ptr_eq(signal, &recovery))
        );
        assert!(attached[1..].iter().all(Option::is_none));
        assert_eq!(recovery.state(), RackRecoveryState::Quiescent);

        // SAFETY: this test has no native callback or worker, and the dispatcher is quiescent.
        unsafe { inactive_owner.reset_after_worker_exit(11) }.expect("replacement mapping");
        assert!(recovery.publish_replacement(1, 11));
        dispatcher.service_stopped_recoveries();
        assert_eq!(dispatcher.racks[0].banks.active_index(), 1);
        assert_eq!(
            recovery.state(),
            RackRecoveryState::ReplacementActive {
                retiring_bank_index: 0
            }
        );
        // SAFETY: no worker exists and the stopped dispatcher no longer selects this bank.
        unsafe { active_owner.reset_after_worker_exit(13) }.expect("retired mapping reset");
        assert!(recovery.publish_retired_reset(0, 13));
        dispatcher.service_stopped_recoveries();
        assert_eq!(recovery.state(), RackRecoveryState::Idle);
        assert_eq!(dispatcher.bank_metadata(1).unwrap(), other_metadata);
        assert_eq!(dispatcher.block_index, 0);
        assert_eq!(dispatcher.telemetry(0).unwrap().deadline_misses, 0);
        assert_eq!(dispatcher.telemetry(1).unwrap().fallback_activations, 0);

        inactive_owner
            .bank()
            .header
            .worker_latency_samples
            .store(37, Ordering::Release);
        let mut mixer = sp_engine::RealtimeRackMixer::new(sp_engine::PreparedGraph::empty());
        dispatcher.apply_to_mixer(&mut mixer);
        assert_eq!(mixer.rack_settings(0).unwrap().latency_frames, 37);
        publish(&mut dispatcher, &[0.0; 4], &[]);
        assert_eq!(dispatcher.racks[0].live.unwrap().ticket.generation, 11);
        assert_eq!(dispatcher.racks[1].live.unwrap().bank_index, 0);
        assert!(recovery.request_quiesce());
        dispatcher.service_stopped_recoveries();
        inactive_owner
            .bank()
            .header
            .worker_latency_samples
            .store(99, Ordering::Release);
        dispatcher.apply_to_mixer(&mut mixer);
        assert_eq!(mixer.rack_settings(0).unwrap().latency_frames, 37);
    }

    #[test]
    fn publishes_interleaved_input_as_planar_and_consumes_valid_stereo_wet() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let input = [0.25, -0.25, 0.5, -0.5];
        publish(&mut dispatcher, &input, &[]);
        let live = dispatcher.racks[0].live.expect("request");
        let slot = dispatcher.racks[0]
            .banks
            .bank(live.bank_index)
            .expect("live bank")
            .bank()
            .slot(live.slot_index)
            .expect("slot");
        assert_eq!(&slot.input_audio[0][..2], &[0.25, 0.5]);
        assert_eq!(&slot.input_audio[1][..2], &[-0.25, -0.5]);

        complete(&mut dispatcher, 0, [1.0, -1.0, 0.5, -0.5]);
        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
        let sources = dispatcher.sources();
        let RackAudioSource::Wet(wet) = sources[0] else {
            panic!("valid completion must provide wet audio");
        };
        assert_eq!(&wet[..4], &[1.0, -1.0, 0.5, -0.5]);
        assert_eq!(dispatcher.telemetry(0).expect("telemetry").completed, 1);
    }

    #[test]
    fn small_blocks_publish_and_consume_full_wet_output() {
        for frames in [32, 64] {
            let Some(mut dispatcher) = dispatcher(1) else {
                return;
            };
            let input: Vec<f32> = (0..frames * 2)
                .map(|sample| f32::from(u8::try_from(sample).expect("small block sample index")))
                .collect();
            dispatcher.dispatch_open(&input, &[], &super::EMPTY_RACK_AUTOMATION, frames);
            let live = dispatcher.racks[0].live.expect("request");
            assert_eq!(live.request.frame_count as usize, frames);
            let slot = dispatcher.racks[0]
                .banks
                .bank_mut(live.bank_index)
                .expect("bank")
                .bank_mut()
                .slot_mut(live.slot_index)
                .expect("slot");
            assert_eq!(
                slot.input_audio[0][frames - 1].to_bits(),
                input[(frames - 1) * 2].to_bits()
            );
            assert_eq!(
                slot.input_audio[1][frames - 1].to_bits(),
                input[(frames - 1) * 2 + 1].to_bits()
            );
            assert_eq!(
                slot.claim_for_processing_at(1, dispatcher.clock.now_ticks())
                    .unwrap(),
                live.ticket
            );
            slot.output_audio[0][frames - 1] = 0.25;
            slot.output_audio[1][frames - 1] = -0.25;
            slot.publish_completion_at(1, live.ticket, dispatcher.clock.now_ticks())
                .unwrap();

            super::observe_rack(&mut dispatcher.racks[0], dispatcher.block_index);
            let sources = dispatcher.sources();
            let RackAudioSource::Wet(wet) = sources[0] else {
                panic!("small completion must provide wet audio");
            };
            assert_eq!(&wet[(frames - 1) * 2..frames * 2], &[0.25, -0.25]);
            assert_eq!(dispatcher.telemetry(0).unwrap().completed, 1);
        }
    }

    #[test]
    #[allow(
        clippy::large_stack_arrays,
        reason = "the callback API uses fixed per-rack input arrays"
    )]
    fn per_rack_inputs_publish_distinct_audio_in_one_batch() {
        let Some(mut dispatcher) = dispatcher(2) else {
            return;
        };
        let mut inputs = [[0.0; super::MAX_SAMPLES]; MAX_RACKS];
        inputs[0][0] = 0.25;
        inputs[0][1] = -0.25;
        inputs[1][0] = 0.75;
        inputs[1][1] = -0.75;
        dispatcher.process_block_with_rack_inputs(
            &inputs,
            None,
            &[],
            &super::EMPTY_RACK_AUTOMATION,
            32,
            Duration::ZERO,
        );
        for (rack_index, input) in inputs.iter().enumerate().take(2) {
            let slot = dispatcher.racks[rack_index]
                .banks
                .active_bank()
                .bank()
                .slot(0)
                .expect("first slot");
            assert_eq!(slot.metadata.frame_count, 32);
            assert_eq!(slot.input_audio[0][0].to_bits(), input[0].to_bits());
            assert_eq!(slot.input_audio[1][0].to_bits(), input[1].to_bits());
            assert_eq!(dispatcher.telemetry(rack_index).unwrap().published, 1);
        }
    }

    #[test]
    #[allow(
        clippy::large_stack_arrays,
        reason = "test uses the same fixed rack automation array as the callback"
    )]
    fn accepts_completion_with_rack_automation() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let mut automation = [RackAutomationEvents::new(); MAX_RACKS];
        let event = BlockEvent {
            frame_offset: 0,
            event_type: BLOCK_EVENT_PARAMETER,
            key: 42,
            value: 0.75,
            flags: 1,
        };
        assert!(automation[0].push(event));
        dispatcher.dispatch_open(&[0.0; 4], &[], &automation, 2);
        let live = dispatcher.racks[0].live.expect("request");
        let slot = dispatcher.racks[0]
            .banks
            .bank(live.bank_index)
            .expect("bank")
            .bank()
            .slot(live.slot_index)
            .expect("slot");
        assert_eq!(slot.events[0], event);

        complete(&mut dispatcher, 0, [0.25; 4]);
        dispatcher.process_block(&[0.0; 4], &[], 2, Duration::ZERO);
        assert!(matches!(dispatcher.sources()[0], RackAudioSource::Wet(_)));
        let telemetry = dispatcher.telemetry(0).expect("telemetry");
        assert_eq!(telemetry.completed, 1);
        assert_eq!(telemetry.protocol_rejections, 0);
    }

    #[test]
    fn rejects_nonfinite_worker_output_without_poisoning_another_rack() {
        let Some(mut dispatcher) = dispatcher(2) else {
            return;
        };
        let input = [0.0; 4];
        publish(&mut dispatcher, &input, &[]);
        complete(&mut dispatcher, 0, [0.25, -0.25, 0.5, -0.5]);

        let live = dispatcher.racks[1].live.expect("live request");
        let slot = dispatcher.racks[1]
            .banks
            .bank_mut(live.bank_index)
            .expect("live bank")
            .bank_mut()
            .slot_mut(live.slot_index)
            .expect("slot");
        let ticket = slot
            .claim_for_processing_at(2, dispatcher.clock.now_ticks())
            .expect("claim");
        slot.output_audio[0][0] = f32::NAN;
        slot.publish_completion_at(2, ticket, dispatcher.clock.now_ticks())
            .expect("complete");

        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
        let sources = dispatcher.sources();
        assert!(matches!(sources[0], RackAudioSource::Wet(_)));
        assert!(matches!(sources[1], RackAudioSource::None));
        assert_eq!(dispatcher.telemetry(0).expect("rack 0").completed, 1);
        let rack_one = dispatcher.telemetry(1).expect("rack 1");
        assert_eq!(rack_one.protocol_rejections, 1);
        assert_eq!(rack_one.fallback_activations, 1);
    }

    #[test]
    fn malformed_target_completion_cannot_close_an_unaffected_rack() {
        let Some(mut dispatcher) = dispatcher(2) else {
            return;
        };
        let input = [0.0; 4];
        publish(&mut dispatcher, &input, &[]);
        complete(&mut dispatcher, 0, [0.25, -0.25, 0.5, -0.5]);
        complete(&mut dispatcher, 1, [0.0; 4]);
        let target = dispatcher.racks[1].live.expect("target live request");
        dispatcher.racks[1]
            .banks
            .bank_mut(target.bank_index)
            .expect("target bank")
            .bank_mut()
            .slot_mut(target.slot_index)
            .expect("target slot")
            .metadata
            .completion_sequence
            .store(target.ticket.sequence.saturating_add(1), Ordering::Release);

        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
        assert!(matches!(dispatcher.sources()[0], RackAudioSource::Wet(_)));
        assert!(matches!(dispatcher.sources()[1], RackAudioSource::None));
        assert_eq!(dispatcher.telemetry(0).expect("unaffected").completed, 1);
        assert_eq!(
            dispatcher.telemetry(1).expect("target").stale_completions,
            1
        );
    }

    #[test]
    fn rejects_a_stale_completion_ticket() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let input = [0.0; 4];
        publish(&mut dispatcher, &input, &[]);
        let live = dispatcher.racks[0].live.expect("live request");
        complete(&mut dispatcher, 0, [0.0; 4]);
        dispatcher.racks[0]
            .banks
            .bank_mut(live.bank_index)
            .expect("live bank")
            .bank_mut()
            .slot_mut(live.slot_index)
            .expect("slot")
            .metadata
            .completion_generation
            .store(live.ticket.generation.saturating_add(1), Ordering::Relaxed);

        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
        let telemetry = dispatcher.telemetry(0).expect("telemetry");
        assert_eq!(telemetry.stale_completions, 1);
        assert_eq!(telemetry.fallback_activations, 1);
    }
}
