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
    WorkerObservation,
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
const MAX_IDLE_OBSERVATION_PASSES: u8 = 2;

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

    fn as_slice(&self) -> &[BlockEvent] {
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

/// Result of one bounded completion-observation sweep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservationStop {
    /// At least one request remains and another nonblocking sweep may be useful.
    Continue,
    /// No request remains observable.
    NoLiveRequests,
    /// The absolute callback deadline has been reached.
    DeadlineReached,
    /// Consecutive no-progress sweeps make another poll useless for this callback.
    NoUsefulProgress,
}

/// Fixed adaptive policy for callback completion observation.
///
/// The policy never sleeps or blocks. Progress resets its short idle allowance; otherwise it
/// exits after a bounded number of full sweeps even if the absolute deadline is further away.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CompletionObservationBudget {
    idle_passes: u8,
}

impl CompletionObservationBudget {
    fn after_pass(
        &mut self,
        awaiting: bool,
        made_progress: bool,
        deadline_reached: bool,
    ) -> ObservationStop {
        if !awaiting {
            return ObservationStop::NoLiveRequests;
        }
        if deadline_reached {
            return ObservationStop::DeadlineReached;
        }
        if made_progress {
            self.idle_passes = 0;
            return ObservationStop::Continue;
        }
        self.idle_passes = self.idle_passes.saturating_add(1);
        if self.idle_passes >= MAX_IDLE_OBSERVATION_PASSES {
            ObservationStop::NoUsefulProgress
        } else {
            ObservationStop::Continue
        }
    }
}

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
    /// Completion-observation sweeps that inspected this rack.
    pub completion_observation_passes: u64,
    /// Observations that stopped because the absolute deadline was reached.
    pub completion_observation_deadline_stops: u64,
    /// Observations that stopped because bounded polling made no useful progress.
    pub completion_observation_no_progress_stops: u64,
    /// Timed-out requested slots explicitly marked abandoned and retained for retirement.
    pub abandoned_requests: u64,
}

#[derive(Default)]
struct AtomicRackDispatchTelemetry {
    published: AtomicU64,
    completed: AtomicU64,
    deadline_misses: AtomicU64,
    protocol_rejections: AtomicU64,
    stale_completions: AtomicU64,
    fallback_activations: AtomicU64,
    completion_observation_passes: AtomicU64,
    completion_observation_deadline_stops: AtomicU64,
    completion_observation_no_progress_stops: AtomicU64,
    abandoned_requests: AtomicU64,
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
            completion_observation_passes: self
                .completion_observation_passes
                .load(Ordering::Relaxed),
            completion_observation_deadline_stops: self
                .completion_observation_deadline_stops
                .load(Ordering::Relaxed),
            completion_observation_no_progress_stops: self
                .completion_observation_no_progress_stops
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

/// Preallocated host-side dispatcher for two stable shared-memory banks per rack.
///
/// It follows the same protocol sequence proven by `xtask phase1`: publish a timed request,
/// observe a completion through `completion_snapshot`, then consume only the exact live ticket.
pub struct RackSharedMemoryDispatcher {
    clock: MonotonicClock,
    racks: Vec<RackDispatchState>,
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
        let mut racks = Vec::with_capacity(banks.len());
        for (rack_index, banks, recovery) in banks {
            racks.push(RackDispatchState {
                rack_index,
                banks,
                recovery,
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
        self.process_block_with_events(input, midi, &EMPTY_RACK_AUTOMATION, frames, deadline);
    }

    /// Dispatches audio, MIDI, and rack-targeted automation for one callback block.
    pub fn process_block_with_events(
        &mut self,
        input: &[f32],
        midi: &[MidiEvent],
        automation: &[RackAutomationEvents; MAX_RACKS],
        frames: usize,
        deadline: Duration,
    ) {
        if frames == 0
            || frames > sp_engine::MAX_MIX_FRAMES
            || input.len() < frames * 2
            || midi.len() > MAX_MIDI_EVENTS
        {
            self.close_all_invalid();
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
        self.observe_until(frames, deadline_tick);
        self.dispatch_open(input, midi, automation, frames);
        self.block_index = self.block_index.wrapping_add(1);
    }

    /// Applies the fixed gate outcomes to the existing realtime mixer.
    pub fn apply_to_mixer(&self, mixer: &mut RealtimeRackMixer) {
        for rack in &self.racks {
            mixer.set_gate_outcome(rack.rack_index, rack.outcome);
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

    fn observe_until(&mut self, frames: usize, deadline_tick: u64) {
        let mut budget = CompletionObservationBudget::default();
        let stop = loop {
            let mut awaiting = false;
            let mut made_progress = false;
            for rack in &mut self.racks {
                if rack.live.is_none() {
                    continue;
                }
                awaiting = true;
                rack.telemetry
                    .completion_observation_passes
                    .fetch_add(1, Ordering::Relaxed);
                made_progress |= observe_rack(rack, self.block_index, frames);
            }
            let stop = budget.after_pass(
                awaiting,
                made_progress,
                self.clock.now_ticks() >= deadline_tick,
            );
            if stop != ObservationStop::Continue {
                break stop;
            }
        };

        match stop {
            ObservationStop::DeadlineReached => {
                for rack in &self.racks {
                    if rack.live.is_some() {
                        rack.telemetry
                            .completion_observation_deadline_stops
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            ObservationStop::NoUsefulProgress => {
                for rack in &self.racks {
                    if rack.live.is_some() {
                        rack.telemetry
                            .completion_observation_no_progress_stops
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            ObservationStop::Continue | ObservationStop::NoLiveRequests => {}
        }

        for rack in &mut self.racks {
            if matches!(rack.gate.state(), RackGateState::Awaiting { .. }) {
                abandon_live_request(rack);
                let prior = rack.gate.state();
                // The abandoned slot can never be reused, so the miss closes the rack
                // immediately; recovery requires a worker and bank replacement.
                rack.outcome = rack.gate.deadline_expired_hard(self.block_index);
                record_outcome(rack, prior, rack.outcome);
            }
        }
    }

    fn dispatch_open(
        &mut self,
        input: &[f32],
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
            let request = BlockRequest {
                frame_count: u32::try_from(frames).unwrap_or(u32::MAX),
                input_channel_count: STEREO_CHANNELS,
                output_channel_count: STEREO_CHANNELS,
                midi_event_count: u32::try_from(midi.len()).unwrap_or(u32::MAX),
                event_count: u32::try_from(events.len()).unwrap_or(u32::MAX),
                flags: 0,
            };
            copy_interleaved_stereo_to_planar(input, frames, slot);
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
                        });
                        rack.telemetry.published.fetch_add(1, Ordering::Relaxed);
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
            {
                let _ = recovery.complete_retirement();
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

/// Observes one nonblocking completion sweep and returns whether it made useful progress.
fn observe_rack(rack: &mut RackDispatchState, block_index: u64, frames: usize) -> bool {
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
            true
        }
        GateOutcome::Awaiting => false,
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
            true
        }
        GateOutcome::DispatchAllowed => unreachable!("observation cannot open a dispatch gate"),
    }
}

fn is_expected_stereo(request: BlockRequest, frames: usize) -> bool {
    request.frame_count == u32::try_from(frames).unwrap_or(u32::MAX)
        && request.input_channel_count == STEREO_CHANNELS
        && request.output_channel_count == STEREO_CHANNELS
        && u32::try_from(sp_shared_memory::MAX_MIDI_EVENTS)
            .is_ok_and(|maximum| request.midi_event_count <= maximum)
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
        // Deadline misses close the rack but never quiesce from the callback: replacing a
        // slow-but-alive worker is a control-plane policy decision made off this thread.
        if let Some(recovery) = &rack.recovery
            && reason != FallbackReason::DeadlineMiss
        {
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

    use sp_shared_memory::SlotState;
    use sp_shared_memory_macos::{
        MappedBankLifecycle, MappedRackBanks, RackRecoverySignal, RackRecoveryState,
        SharedMemoryRegion,
    };

    use super::{
        CompletionObservationBudget, MidiEvent, ObservationStop, RackAudioSource,
        RackSharedMemoryDispatcher,
    };

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

    #[test]
    fn observation_policy_has_deterministic_deadline_and_idle_bounds() {
        let mut budget = CompletionObservationBudget::default();
        assert_eq!(
            budget.after_pass(true, false, false),
            ObservationStop::Continue
        );
        assert_eq!(
            budget.after_pass(true, false, false),
            ObservationStop::NoUsefulProgress
        );

        let mut progress_budget = CompletionObservationBudget::default();
        assert_eq!(
            progress_budget.after_pass(true, true, false),
            ObservationStop::Continue
        );
        assert_eq!(
            progress_budget.after_pass(true, false, false),
            ObservationStop::Continue
        );
        assert_eq!(
            progress_budget.after_pass(true, false, true),
            ObservationStop::DeadlineReached
        );
        assert_eq!(
            progress_budget.after_pass(false, true, false),
            ObservationStop::NoLiveRequests
        );
    }

    #[test]
    fn idle_observation_is_bounded_and_abandons_without_reusing_the_slot() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let input = [0.0; 4];
        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
        let live = dispatcher.racks[0].live.expect("request");

        // A deliberately distant absolute deadline must not induce a busy wait after two
        // complete no-progress sweeps.
        dispatcher.process_block(&input, &[], 2, Duration::from_secs(1));
        let telemetry = dispatcher.telemetry(0).expect("telemetry");
        assert_eq!(telemetry.completion_observation_passes, 2);
        assert_eq!(telemetry.completion_observation_no_progress_stops, 1);
        assert_eq!(telemetry.completion_observation_deadline_stops, 0);
        assert_eq!(telemetry.abandoned_requests, 1);
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
    fn sparse_product_rack_indices_are_preserved() {
        let region = match SharedMemoryRegion::create(1) {
            Ok(region) => region,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("region: {error}"),
        };
        let mut dispatcher =
            RackSharedMemoryDispatcher::new_indexed(vec![(3, region)]).expect("indexed dispatcher");
        let input = [0.0; 4];
        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
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
        dispatcher.process_block(&[0.0; 4], &[event], 2, Duration::ZERO);

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
        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
        dispatcher.process_block(&input, &[], 2, Duration::from_secs(1));
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
        assert_eq!(recovery.state(), RackRecoveryState::Idle);
        assert_eq!(
            dispatcher.racks[0].banks.metadata()[0].lifecycle,
            MappedBankLifecycle::Inactive
        );
    }

    #[test]
    fn publishes_interleaved_input_as_planar_and_consumes_valid_stereo_wet() {
        let Some(mut dispatcher) = dispatcher(1) else {
            return;
        };
        let input = [0.25, -0.25, 0.5, -0.5];
        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
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
    fn rejects_nonfinite_worker_output_without_poisoning_another_rack() {
        let Some(mut dispatcher) = dispatcher(2) else {
            return;
        };
        let input = [0.0; 4];
        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
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
        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
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
        dispatcher.process_block(&input, &[], 2, Duration::ZERO);
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
