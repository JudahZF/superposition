//! One rack lane's dispatch: its bank pair, gate, live ticket, wet buffer, and telemetry.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use sp_shared_memory::{
    BlockRequest, BlockSlot, BlockTicket, BlockTiming, ProtocolError, SlotState,
};
use sp_shared_memory_os::{
    MappedBankMetadata, MappedRackBanks, MonotonicClock, RackRecoverySignal, RackRecoveryState,
    SharedMemoryRegion,
};

use crate::{
    FallbackReason, GateOutcome, LANE_SAMPLES, MAX_LANE_FRAMES, RackGate, RackGateState,
    WorkerObservation,
};

/// Lock-free counters for one lane, suitable for control-plane polling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LaneTelemetry {
    /// Requests published to this lane's bank.
    pub published: u64,
    /// Failed kernel wake calls after publishing a request.
    pub wake_failures: u64,
    /// Longest observed kernel wake call in shared monotonic clock ticks.
    pub max_wake_ticks: u64,
    /// Valid completions consumed from this lane's bank.
    pub completed: u64,
    /// Requests that did not complete before the read deadline.
    pub deadline_misses: u64,
    /// Deadline misses where the worker had not claimed the request.
    pub missed_unclaimed: u64,
    /// Deadline misses where the worker owned or was processing the request.
    pub missed_in_progress: u64,
    /// Deadline misses where completion became visible after the final sweep.
    pub missed_completed_late: u64,
    /// Host block index of the most recent deadline miss.
    pub last_miss_block_index: u64,
    /// Request ticket sequence of the most recent deadline miss.
    pub last_miss_sequence: u64,
    /// Shared monotonic tick when the most recent missed request was published.
    pub last_miss_request_tick: u64,
    /// Shared monotonic tick when the worker claimed the most recent missed request, or zero.
    pub last_miss_claimed_tick: u64,
    /// Shared monotonic tick when the host observed the most recent deadline miss.
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
    /// Transitions to lane-local fallback.
    pub fallback_activations: u64,
    /// Host blocks that ended with this lane's gate closed.
    pub gate_closed_blocks: u64,
    /// Non-blocking completion checks of a live request.
    pub completion_observation_passes: u64,
    /// Live requests still awaiting at their read deadline.
    pub completion_observation_deadline_stops: u64,
    /// Timed-out requested slots explicitly marked abandoned and retained for retirement.
    pub abandoned_requests: u64,
}

#[derive(Default)]
struct AtomicLaneTelemetry {
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

impl AtomicLaneTelemetry {
    fn snapshot(&self) -> LaneTelemetry {
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
        LaneTelemetry {
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

/// The lane's one live request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiveRequest {
    /// The bank (0 or 1) that holds the request.
    pub bank_index: usize,
    /// The ticket the gate accepts.
    pub ticket: BlockTicket,
    /// The slot in that bank.
    pub slot_index: usize,
    /// The request as published.
    pub request: BlockRequest,
}

/// One rack lane: a worker's two stable banks, the gate, and the live ticket.
///
/// The lane runs one request at a time. Each host block it may publish one request
/// ([`Self::publish`], then [`Self::wake`] the worker) and consume one completion
/// ([`Self::poll`] until a result or the read deadline, then [`Self::expire`]). A miss, a crash,
/// or a malformed completion closes the gate: the lane plays its fallback until the control
/// plane replaces the worker on the other bank ([`RackRecoverySignal`]).
///
/// The real-time methods (`begin_block`, `publish`, `wake`, `poll`, `expire`, `consume`,
/// `end_block`) do not allocate, lock, or block; `wake` makes one non-blocking kernel wake call.
/// The lane holds no engine or session type, so any host loop can drive it.
pub struct RackLane {
    clock: MonotonicClock,
    banks: MappedRackBanks,
    recovery: Option<Arc<RackRecoverySignal>>,
    gate: RackGate,
    live: Option<LiveRequest>,
    outcome: GateOutcome,
    /// Interleaved stereo output of the last accepted completion; a mono request fills both.
    wet: [f32; LANE_SAMPLES],
    wet_valid: bool,
    completion_timing: BlockTiming,
    telemetry: AtomicLaneTelemetry,
}

impl RackLane {
    /// Creates a lane over a stable bank pair. Build it off the real-time thread.
    #[must_use]
    pub fn new(
        clock: MonotonicClock,
        banks: MappedRackBanks,
        recovery: Option<Arc<RackRecoverySignal>>,
    ) -> Self {
        Self {
            clock,
            banks,
            recovery,
            gate: RackGate::new(),
            live: None,
            outcome: GateOutcome::DispatchAllowed,
            wet: [0.0; LANE_SAMPLES],
            wet_valid: false,
            completion_timing: BlockTiming::default(),
            telemetry: AtomicLaneTelemetry::default(),
        }
    }

    /// Maps a live worker's two banks: `active_index` is the bank its worker serves.
    ///
    /// # Errors
    /// Returns an error for an unfinished bank handoff or an invalid bank pair.
    pub fn from_live_regions(
        clock: MonotonicClock,
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
        // bank has no worker and has completed any prior retirement.
        #[allow(unsafe_code, reason = "documented bank-pair lifecycle contract")]
        let banks = unsafe { MappedRackBanks::from_indexed_regions(regions, active_index) }?;
        Ok(Self::new(clock, banks, Some(recovery)))
    }

    /// Writes fresh banks into a VM port's region and makes a lane over them: bank 0 active
    /// with `generation`, bank 1 inactive. Requests wake the guest through `doorbell`, the
    /// eventfd of the guest's vector on the port ([`sp_shared_memory_os::create_port_banks`]).
    ///
    /// The control plane calls it only after the guest agent has confirmed that no worker
    /// runs on the port (the one-owner rule of the VM plan), and holds one lane per port.
    ///
    /// # Errors
    /// Returns an error for a port region that is too small, a zero or largest generation,
    /// or a failed mapping.
    #[cfg(target_os = "linux")]
    pub fn over_vm_port(
        clock: MonotonicClock,
        memory: std::os::fd::OwnedFd,
        port: u32,
        generation: u64,
        doorbell: std::os::fd::BorrowedFd<'_>,
        recovery: Arc<RackRecoverySignal>,
    ) -> io::Result<Self> {
        if recovery.state() != RackRecoveryState::Idle {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "finish the rack bank handoff before adding it to live audio",
            ));
        }
        // SAFETY: the control plane calls this only when no guest worker maps the port and no
        // other lane holds it (documented above).
        #[allow(unsafe_code, reason = "documented VM port ownership contract")]
        let banks =
            unsafe { sp_shared_memory_os::create_port_banks(memory, port, generation, doorbell) }?;
        Ok(Self::new(clock, banks, Some(recovery)))
    }

    /// The worker recovery signal, when the lane has one.
    #[must_use]
    pub fn recovery(&self) -> Option<&Arc<RackRecoverySignal>> {
        self.recovery.as_ref()
    }

    /// The bank pair, for reading names, metadata, and header counters.
    #[must_use]
    pub fn banks(&self) -> &MappedRackBanks {
        &self.banks
    }

    /// The gate's current state.
    #[must_use]
    pub fn gate_state(&self) -> RackGateState {
        self.gate.state()
    }

    /// The gate outcome of the latest operation in this block.
    #[must_use]
    pub fn outcome(&self) -> GateOutcome {
        self.outcome
    }

    /// The live request, if one is awaiting its completion.
    #[must_use]
    pub fn live(&self) -> Option<LiveRequest> {
        self.live
    }

    /// The accepted completion of this block (interleaved stereo), if there is one.
    #[must_use]
    pub fn wet(&self) -> Option<&[f32; LANE_SAMPLES]> {
        self.wet_valid.then_some(&self.wet)
    }

    /// The timing of the last accepted completion, from the bank: publication, claim, and
    /// completion ticks.
    #[must_use]
    pub fn completion_timing(&self) -> BlockTiming {
        self.completion_timing
    }

    /// A lock-free telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> LaneTelemetry {
        self.telemetry.snapshot()
    }

    /// Whether the control plane is replacing this lane's worker: the lane neither publishes
    /// nor trusts its bank header until the handoff ends.
    #[must_use]
    pub fn is_quiescing(&self) -> bool {
        self.recovery.as_ref().is_some_and(|recovery| {
            matches!(
                recovery.state(),
                RackRecoveryState::QuiesceRequested
                    | RackRecoveryState::Quiescent
                    | RackRecoveryState::ReplacementReady { .. }
            )
        })
    }

    /// The latency the active worker publishes (its chain's total), or `None` while the
    /// control plane is replacing it and may reset its bank.
    #[must_use]
    pub fn worker_latency_samples(&self) -> Option<u32> {
        (!self.is_quiescing()).then(|| {
            self.banks
                .active_bank()
                .bank()
                .header
                .worker_latency_samples
                .load(Ordering::Acquire)
        })
    }

    /// Starts a host block: forgets the previous block's wet output and advances any bank
    /// handoff the control plane has requested. Real-time safe.
    pub fn begin_block(&mut self, block_index: u64) {
        self.wet_valid = false;
        self.service_recovery(block_index);
    }

    /// Ends a host block: counts it when the gate is closed. Real-time safe.
    pub fn end_block(&self) {
        if matches!(self.gate.state(), RackGateState::Closed { .. }) {
            self.telemetry
                .gate_closed_blocks
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Writes and publishes this block's request without waking the worker.
    ///
    /// `fill` writes the payload (input audio, sidechains, MIDI, events) into the free slot and
    /// returns the request that describes it. `read_deadline_tick` is when the host will read
    /// the completion, on the shared clock; `None` keeps the one-block rule. Returns whether a
    /// request is now live. A lane whose gate is not open, or whose worker is being replaced,
    /// publishes nothing. Real-time safe: no allocation, lock, or system call besides the clock.
    pub fn publish(
        &mut self,
        block_index: u64,
        read_deadline_tick: Option<u64>,
        fill: impl FnOnce(&mut BlockSlot) -> BlockRequest,
    ) -> bool {
        if self.is_quiescing() || !matches!(self.gate.state(), RackGateState::Open) {
            return false;
        }
        let bank_index = self.banks.active_index();
        let clock = self.clock;
        let published = free_slot_index(self.banks.active_bank())
            .ok()
            .flatten()
            .and_then(|slot_index| {
                let bank = self.banks.bank_mut(bank_index)?.bank_mut();
                let request = fill(bank.slot_mut(slot_index)?);
                let published = clock.now_ticks();
                // A deadline at or before publication is already due; zero would mean untimed.
                let deadline_offset = read_deadline_tick.map_or(0, |deadline| {
                    u32::try_from(deadline.saturating_sub(published))
                        .unwrap_or(u32::MAX)
                        .max(1)
                });
                bank.request_timed_block_at(slot_index, request, published, deadline_offset)
                    .ok()
                    .map(|ticket| (slot_index, ticket, request))
            });
        let Some((slot_index, ticket, request)) = published else {
            self.close_dispatch(block_index);
            return false;
        };
        let prior = self.gate.state();
        self.outcome = self.gate.dispatch(ticket, block_index);
        self.record_outcome(prior);
        if self.outcome != GateOutcome::DispatchAllowed {
            return false;
        }
        self.live = Some(LiveRequest {
            bank_index,
            ticket,
            slot_index,
            request,
        });
        self.telemetry.published.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Wakes the worker for the live request: one non-blocking kernel wake. Does nothing without
    /// a live request. Any thread may call it after the publishing thread is done.
    pub fn wake(&self) {
        if self.live.is_none() {
            return;
        }
        let started = self.clock.now_ticks();
        let notified = self.banks.active_bank().notify_request();
        let wake_ticks = self.clock.now_ticks().saturating_sub(started);
        self.telemetry
            .max_wake_ticks
            .fetch_max(wake_ticks, Ordering::Relaxed);
        if !notified {
            self.telemetry.wake_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Checks the live request's slot once, without waiting. Accepts a valid completion into
    /// [`Self::wet`]; a malformed or stale one closes the gate. Returns the gate outcome.
    ///
    /// # Panics
    /// Never: a live request names one of the lane's two banks and a fixed slot index.
    pub fn poll(&mut self, block_index: u64) -> GateOutcome {
        let Some(live) = self.live else {
            return self.outcome;
        };
        self.telemetry
            .completion_observation_passes
            .fetch_add(1, Ordering::Relaxed);
        let slot = self
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
            Ok(Some(_)) if !copy_finite_output(slot, &mut self.wet, live.request) => {
                WorkerObservation::ProtocolFault(ProtocolError::MalformedCompletion)
            }
            Ok(Some(_)) => match slot.consume_completion_timing(live.ticket) {
                Ok(timing) => {
                    self.completion_timing = timing;
                    WorkerObservation::Completed(live.ticket)
                }
                Err(ProtocolError::UnexpectedState | ProtocolError::Owned) => {
                    WorkerObservation::Pending
                }
                Err(error) => WorkerObservation::ProtocolFault(error),
            },
            Ok(None) | Err(ProtocolError::Owned) => WorkerObservation::Pending,
            Err(error) => WorkerObservation::ProtocolFault(error),
        };
        let prior = self.gate.state();
        self.outcome = self.gate.observe(observation, block_index);
        match self.outcome {
            GateOutcome::WorkerResultAccepted => {
                self.live = None;
                self.wet_valid = true;
                self.telemetry.completed.fetch_add(1, Ordering::Relaxed);
            }
            GateOutcome::Awaiting => {}
            GateOutcome::UseFallback(_) => {
                if matches!(observation, WorkerObservation::ProtocolFault(_)) {
                    self.telemetry
                        .protocol_rejections
                        .fetch_add(1, Ordering::Relaxed);
                }
                if matches!(observation, WorkerObservation::Completed(_)) {
                    self.telemetry
                        .stale_completions
                        .fetch_add(1, Ordering::Relaxed);
                }
                self.release_live_request();
                self.record_outcome(prior);
            }
            GateOutcome::DispatchAllowed => unreachable!("observation cannot open a dispatch gate"),
        }
        self.outcome
    }

    /// Ends the wait at the read deadline: a request still awaiting is a miss. The abandoned
    /// slot cannot be reused, so the gate closes at once and the control plane must replace
    /// the worker and bank. Does nothing when no request is awaiting.
    pub fn expire(&mut self, block_index: u64) {
        if !matches!(self.gate.state(), RackGateState::Awaiting { .. }) {
            return;
        }
        self.telemetry
            .completion_observation_deadline_stops
            .fetch_add(1, Ordering::Relaxed);
        self.record_deadline_diagnostics(block_index, self.clock.now_ticks());
        self.release_live_request();
        let prior = self.gate.state();
        self.outcome = self.gate.deadline_expired_hard(block_index);
        self.record_outcome(prior);
    }

    /// Reads the live request's result at the read deadline, without waiting: a completion is
    /// accepted, anything else is a miss ([`Self::poll`], then [`Self::expire`]). Returns the
    /// accepted wet output.
    pub fn consume(&mut self, block_index: u64) -> Option<&[f32; LANE_SAMPLES]> {
        self.poll(block_index);
        self.expire(block_index);
        self.wet()
    }

    /// Closes the gate for a block the host cannot dispatch (bad geometry), abandoning any
    /// unclaimed request.
    pub fn close_invalid(&mut self, block_index: u64) {
        self.release_live_request();
        self.close_dispatch(block_index);
    }

    /// Abandons a still-unclaimed request and forgets the live ticket.
    ///
    /// A processing, complete, malformed, or already abandoned slot stays intact. Its bank stays
    /// retired until the worker has exited, so no late result can be reused.
    pub fn release_live_request(&mut self) {
        let Some(live) = self.live.take() else {
            return;
        };
        let Some(slot) = self
            .banks
            .bank(live.bank_index)
            .and_then(|region| region.bank().slot(live.slot_index))
        else {
            return;
        };
        if slot.metadata.state() == Ok(SlotState::Requested)
            && slot.abandon_request(live.ticket).is_ok()
        {
            self.telemetry
                .abandoned_requests
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Advances the control plane's bank handoff by one step. Real-time safe. The host calls it
    /// once per block ([`Self::begin_block`]), or while stopped, as the handoff's only driver.
    pub fn service_recovery(&mut self, block_index: u64) {
        let Some(recovery) = self.recovery.clone() else {
            return;
        };
        match recovery.state() {
            RackRecoveryState::Idle
            | RackRecoveryState::ReplacementActive { .. }
            | RackRecoveryState::Quiescent => {}
            RackRecoveryState::QuiesceRequested => {
                self.release_live_request();
                if !matches!(self.gate.state(), RackGateState::Closed { .. }) {
                    self.close_dispatch(block_index);
                }
                let _ = recovery.mark_quiescent();
            }
            RackRecoveryState::ReplacementReady {
                bank_index,
                generation,
            } => {
                if self.live.is_some() || !matches!(self.gate.state(), RackGateState::Closed { .. })
                {
                    return;
                }
                if self
                    .banks
                    .acknowledge_external_prepare(bank_index, generation)
                    .is_ok()
                    && let Ok(retiring) = self.banks.activate_prepared()
                {
                    self.gate.reset_after_replacement();
                    self.outcome = GateOutcome::DispatchAllowed;
                    let _ = recovery.mark_replacement_active(retiring.identity.index);
                }
            }
            RackRecoveryState::RetiredReset {
                bank_index,
                generation,
            } => {
                if self
                    .banks
                    .acknowledge_external_retirement(bank_index, generation)
                    .is_ok()
                    && recovery.complete_retirement()
                    && matches!(self.gate.state(), RackGateState::Closed { .. })
                {
                    // A replacement can itself miss its first deadline while the old
                    // bank is still retiring. Requeue recovery once this handshake is idle.
                    let _ = recovery.request_quiesce();
                }
            }
        }
    }

    /// The non-active mapping's name, for a replacement worker launch.
    #[must_use]
    pub fn inactive_bank_name(&self) -> &str {
        self.banks.inactive_bank_name()
    }

    /// Reinitializes the inactive mapping for a replacement worker. A control-plane action; the
    /// inactive mapping must be quiescent.
    ///
    /// # Errors
    /// Returns an error for a bank that has not been safely retired.
    pub fn prepare_replacement_bank(&mut self, generation: u64) -> io::Result<MappedBankMetadata> {
        self.banks.prepare_inactive(generation)
    }

    /// Switches a closed lane to its prepared replacement bank, at a block boundary. The
    /// previous bank stays `Retiring` until [`Self::retire_replaced_bank_after_worker_exit`].
    ///
    /// # Errors
    /// Returns an error when a request is still live, the gate is not closed, or the inactive
    /// bank is not prepared.
    pub fn activate_prepared_replacement(&mut self) -> io::Result<MappedBankMetadata> {
        if self.live.is_some() || !matches!(self.gate.state(), RackGateState::Closed { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "rack must close and retire its live request before bank replacement",
            ));
        }
        let retiring = self.banks.activate_prepared()?;
        self.gate.reset_after_replacement();
        self.outcome = GateOutcome::DispatchAllowed;
        Ok(retiring)
    }

    /// Makes a replaced mapping inactive after its old worker exits and is reaped.
    ///
    /// # Safety
    /// The old worker must be reaped and all handles to its mapping closed before this call.
    ///
    /// # Errors
    /// Returns an error if the mapping is not the safely retired bank.
    #[allow(
        unsafe_code,
        reason = "forwards the bank pair's documented safety contract"
    )]
    pub unsafe fn retire_replaced_bank_after_worker_exit(
        &mut self,
        bank_index: usize,
        next_generation: u64,
    ) -> io::Result<MappedBankMetadata> {
        // SAFETY: upheld by this method's safety contract.
        unsafe {
            self.banks
                .retire_after_worker_exit(bank_index, next_generation)
        }
    }

    /// Mutable bank access, so tests can play the worker in-process.
    #[cfg(feature = "test-access")]
    pub fn banks_mut(&mut self) -> &mut MappedRackBanks {
        &mut self.banks
    }

    /// Attaches a recovery signal after construction, for tests.
    #[cfg(feature = "test-access")]
    pub fn set_recovery(&mut self, recovery: Arc<RackRecoverySignal>) {
        self.recovery = Some(recovery);
    }

    fn close_dispatch(&mut self, block_index: u64) {
        let prior = self.gate.state();
        self.outcome = self.gate.dispatch(
            BlockTicket {
                generation: 0,
                sequence: 0,
            },
            block_index,
        );
        self.record_outcome(prior);
    }

    fn record_outcome(&self, prior: RackGateState) {
        if !matches!(prior, RackGateState::Closed { .. })
            && let GateOutcome::UseFallback(reason) = self.outcome
        {
            self.telemetry
                .fallback_activations
                .fetch_add(1, Ordering::Relaxed);
            if reason == FallbackReason::DeadlineMiss {
                self.telemetry
                    .deadline_misses
                    .fetch_add(1, Ordering::Relaxed);
            }
            // A hard deadline miss abandons the live slot and closes this gate. The stable
            // bank must be retired before any further request can reach this lane.
            if let Some(recovery) = &self.recovery {
                let _ = recovery.request_quiesce();
            }
        }
    }

    fn record_deadline_diagnostics(&self, block_index: u64, observed_tick: u64) {
        let live = self.live.expect("awaiting lane has a live request");
        let bank = self
            .banks
            .bank(live.bank_index)
            .expect("live request refers to one stable bank")
            .bank();
        let telemetry = &self.telemetry;
        telemetry.last_miss_worker_phase.store(
            u64::from(bank.header.worker_phase.load(Ordering::Acquire)),
            Ordering::Relaxed,
        );
        telemetry.last_miss_worker_wait_sequence.store(
            u64::from(bank.header.worker_wait_sequence.load(Ordering::Acquire)),
            Ordering::Relaxed,
        );
        telemetry.last_miss_wake_sequence.store(
            u64::from(bank.header.request_wake_sequence.load(Ordering::Acquire)),
            Ordering::Relaxed,
        );
        telemetry.last_miss_worker_loop_tick.store(
            bank.header.worker_loop_tick.load(Ordering::Acquire),
            Ordering::Relaxed,
        );
        let metadata = &bank
            .slot(live.slot_index)
            .expect("fixed slot index is in range")
            .metadata;
        let owner = metadata.owner.load(Ordering::Acquire);
        let counter = match metadata.state() {
            Ok(SlotState::Requested) if owner == 0 => &telemetry.missed_unclaimed,
            Ok(SlotState::Complete) => &telemetry.missed_completed_late,
            _ => &telemetry.missed_in_progress,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        telemetry
            .last_miss_block_index
            .store(block_index, Ordering::Relaxed);
        telemetry
            .last_miss_sequence
            .store(live.ticket.sequence, Ordering::Relaxed);
        telemetry.last_miss_request_tick.store(
            metadata.request_published_tick.load(Ordering::Acquire),
            Ordering::Relaxed,
        );
        telemetry.last_miss_claimed_tick.store(
            metadata.worker_claimed_tick.load(Ordering::Acquire),
            Ordering::Relaxed,
        );
        telemetry
            .last_miss_observed_tick
            .store(observed_tick, Ordering::Relaxed);
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

/// Copies a completion's output to interleaved stereo, duplicating a mono output. Returns
/// `false` for a non-finite sample.
fn copy_finite_output(
    slot: &BlockSlot,
    wet: &mut [f32; LANE_SAMPLES],
    request: BlockRequest,
) -> bool {
    let frames = (request.frame_count as usize).min(MAX_LANE_FRAMES);
    let right = usize::from(request.output_channel_count >= 2);
    for frame in 0..frames {
        let left = slot.output_audio[0][frame];
        let right = slot.output_audio[right][frame];
        if !left.is_finite() || !right.is_finite() {
            return false;
        }
        wet[frame * 2] = left;
        wet[frame * 2 + 1] = right;
    }
    true
}
