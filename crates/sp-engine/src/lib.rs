//! Capacity-bounded, platform-neutral prepared graph contracts.
//!
//! Graph preparation happens before an audio block. `AudioThread::activate_block`
//! only swaps fixed-size values and acknowledges the swap; it performs neither
//! allocation nor locking. The product mix path lives in [`realtime`].

mod realtime;

pub use realtime::{
    MAX_MIX_FRAMES, MAX_TRANSITION_FRAMES, MIN_TRANSITION_FRAMES, MIX_CHANNELS, MixError,
    RackAudioSource, RackMeterSnapshot, RackSettings, RealtimeRackMixer, WET_RECOVERY_BLOCKS,
};

use sp_model::{
    ChannelLayout, EntityKind, MAX_RACKS, MAX_SLOTS_PER_RACK, PluginSlot, RackTopology, Session,
    ValidationError,
};
use sp_protocol::{BlockTicket, ProtocolError};
use thiserror::Error;

/// A fixed-capacity graph prepared from a valid session model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedGraph {
    racks: [Option<PreparedRack>; MAX_RACKS],
    rack_count: u8,
}

impl PreparedGraph {
    /// Compiles a model into a graph that needs no dynamic storage at activation.
    ///
    /// # Errors
    ///
    /// Returns [`PrepareError`] when the model violates a capacity or validation
    /// contract, or requests a topology or channel layout Phase 0 does not support.
    pub fn compile(session: &Session) -> Result<Self, PrepareError> {
        if session.racks.len() > MAX_RACKS {
            return Err(PrepareError::TooManyRacks {
                found: session.racks.len(),
                capacity: MAX_RACKS,
            });
        }
        for (rack_index, rack) in session.racks.iter().enumerate() {
            if rack.slots.len() > MAX_SLOTS_PER_RACK {
                return Err(PrepareError::TooManySlots {
                    rack_index,
                    found: rack.slots.len(),
                    capacity: MAX_SLOTS_PER_RACK,
                });
            }
        }
        session
            .validate_for_alpha()
            .map_err(PrepareError::InvalidSession)?;

        let mut graph = Self::empty();
        for (rack_index, rack) in session.racks.iter().enumerate() {
            if rack.topology != RackTopology::Serial {
                return Err(PrepareError::UnsupportedTopology {
                    rack_index,
                    topology: rack.topology,
                });
            }

            let source = session
                .sources
                .iter()
                .position(|source| source.id == rack.source_id)
                .ok_or_else(|| {
                    PrepareError::InvalidSession(ValidationError::UnknownReference {
                        owner: EntityKind::Rack,
                        reference: EntityKind::Source,
                        id: rack.source_id.0.clone(),
                    })
                })?;
            let endpoint = session
                .endpoints
                .iter()
                .position(|endpoint| endpoint.id == rack.endpoint_id)
                .ok_or_else(|| {
                    PrepareError::InvalidSession(ValidationError::UnknownReference {
                        owner: EntityKind::Rack,
                        reference: EntityKind::Endpoint,
                        id: rack.endpoint_id.0.clone(),
                    })
                })?;
            let source_layout = PreparedChannelLayout::try_from(&session.sources[source].layout)
                .map_err(|layout| PrepareError::UnsupportedChannelLayout { rack_index, layout })?;
            let endpoint_layout = PreparedChannelLayout::try_from(
                &session.endpoints[endpoint].layout,
            )
            .map_err(|layout| PrepareError::UnsupportedChannelLayout { rack_index, layout })?;
            if source_layout != endpoint_layout {
                return Err(PrepareError::MismatchedChannelLayouts {
                    rack_index,
                    source_layout,
                    endpoint_layout,
                });
            }
            let conversion = PreparedChannelConversion::between(source_layout, endpoint_layout);

            graph.racks[rack_index] = Some(PreparedRack::new(
                source,
                endpoint,
                source_layout,
                endpoint_layout,
                conversion,
                &rack.slots,
            ));
            graph.rack_count += 1;
        }
        Ok(graph)
    }

    /// Returns an empty graph with all fixed-capacity storage initialized.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            racks: [None; MAX_RACKS],
            rack_count: 0,
        }
    }

    /// Returns the number of active racks.
    #[must_use]
    pub const fn rack_count(&self) -> usize {
        self.rack_count as usize
    }

    /// Returns a prepared rack by its model ordering index.
    #[must_use]
    pub fn rack(&self, index: usize) -> Option<&PreparedRack> {
        self.racks.get(index).and_then(Option::as_ref)
    }
}

impl Default for PreparedGraph {
    fn default() -> Self {
        Self::empty()
    }
}

/// A fixed-capacity prepared rack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedRack {
    source_index: usize,
    endpoint_index: usize,
    source_layout: PreparedChannelLayout,
    endpoint_layout: PreparedChannelLayout,
    conversion: PreparedChannelConversion,
    fallback: PreparedFallbackRoute,
    slots: [Option<PreparedSlot>; MAX_SLOTS_PER_RACK],
    slot_count: usize,
}

impl PreparedRack {
    fn new(
        source_index: usize,
        endpoint_index: usize,
        source_layout: PreparedChannelLayout,
        endpoint_layout: PreparedChannelLayout,
        conversion: PreparedChannelConversion,
        model_slots: &[PluginSlot],
    ) -> Self {
        let mut slots = [None; MAX_SLOTS_PER_RACK];
        let mut index = 0;
        while index < model_slots.len() {
            slots[index] = Some(PreparedSlot {
                parameter_count: model_slots[index].parameters.values.len(),
            });
            index += 1;
        }
        Self {
            source_index,
            endpoint_index,
            source_layout,
            endpoint_layout,
            conversion,
            fallback: PreparedFallbackRoute::DelayedDry,
            slots,
            slot_count: model_slots.len(),
        }
    }

    /// Returns this rack's source position in `Session::sources`.
    #[must_use]
    pub const fn source_index(&self) -> usize {
        self.source_index
    }

    /// Returns this rack's endpoint position in `Session::endpoints`.
    #[must_use]
    pub const fn endpoint_index(&self) -> usize {
        self.endpoint_index
    }

    /// Returns this rack's output layout. This is retained as the legacy route layout accessor.
    #[must_use]
    pub const fn layout(&self) -> PreparedChannelLayout {
        self.endpoint_layout
    }

    /// Returns the source layout accepted by this route.
    #[must_use]
    pub const fn source_layout(&self) -> PreparedChannelLayout {
        self.source_layout
    }

    /// Returns the endpoint layout emitted by this route.
    #[must_use]
    pub const fn endpoint_layout(&self) -> PreparedChannelLayout {
        self.endpoint_layout
    }

    /// Returns the explicit source-to-endpoint conversion selected at preparation time.
    #[must_use]
    pub const fn conversion(&self) -> PreparedChannelConversion {
        self.conversion
    }

    /// Returns the deterministic fallback route for this rack.
    #[must_use]
    pub const fn fallback(&self) -> PreparedFallbackRoute {
        self.fallback
    }

    /// Returns the number of prepared slots.
    #[must_use]
    pub const fn slot_count(&self) -> usize {
        self.slot_count
    }

    /// Returns a prepared slot by its model ordering index.
    #[must_use]
    pub fn slot(&self, index: usize) -> Option<&PreparedSlot> {
        self.slots.get(index).and_then(Option::as_ref)
    }
}

/// Fixed metadata retained for a prepared plug-in slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedSlot {
    parameter_count: usize,
}

impl PreparedSlot {
    /// Returns the number of persisted normalized parameters for this slot.
    #[must_use]
    pub const fn parameter_count(&self) -> usize {
        self.parameter_count
    }
}

/// Channel layouts that the Phase 0 graph executor can activate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparedChannelLayout {
    /// One discrete channel.
    Mono,
    /// Two discrete channels.
    Stereo,
}

/// Explicit conversion applied by a prepared route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparedChannelConversion {
    /// Source and endpoint have the same channel count.
    Direct,
    /// Duplicate the mono source to both stereo destination channels.
    MonoToStereo,
    /// Mix stereo source channels equally into the mono destination channel.
    StereoToMono,
}

impl PreparedChannelConversion {
    const fn between(source: PreparedChannelLayout, endpoint: PreparedChannelLayout) -> Self {
        match (source, endpoint) {
            (PreparedChannelLayout::Mono, PreparedChannelLayout::Mono)
            | (PreparedChannelLayout::Stereo, PreparedChannelLayout::Stereo) => Self::Direct,
            (PreparedChannelLayout::Mono, PreparedChannelLayout::Stereo) => Self::MonoToStereo,
            (PreparedChannelLayout::Stereo, PreparedChannelLayout::Mono) => Self::StereoToMono,
        }
    }
}

/// Deterministic fallback route available without worker audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparedFallbackRoute {
    /// Use latency-matched source input through the prepared channel conversion.
    DelayedDry,
    /// Use silence when an effect-style dry path is unavailable.
    Silence,
}

impl TryFrom<&ChannelLayout> for PreparedChannelLayout {
    type Error = ChannelLayout;

    fn try_from(layout: &ChannelLayout) -> Result<Self, Self::Error> {
        match layout {
            ChannelLayout::Mono => Ok(Self::Mono),
            ChannelLayout::Stereo => Ok(Self::Stereo),
            unsupported @ ChannelLayout::Discrete { .. } => Err(unsupported.clone()),
        }
    }
}

/// A two-buffer graph arena.
///
/// Preparation and staging are control-thread work. An `AudioThread` has an
/// exclusive mutable borrow of this arena while it activates a block, so its
/// activation operation cannot lock or allocate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphArena {
    active: StagedGraph,
    staged: Option<StagedGraph>,
    next_generation: u64,
}

impl GraphArena {
    /// Creates an arena whose first graph is already active.
    #[must_use]
    pub const fn new(initial: PreparedGraph) -> Self {
        Self {
            active: StagedGraph {
                graph: initial,
                generation: GraphGeneration(0),
            },
            staged: None,
            next_generation: 1,
        }
    }

    /// Moves a prepared graph into the preallocated pending position.
    ///
    /// Replacing an unacknowledged pending graph is intentional: only the most
    /// recent complete graph is relevant at the next block boundary.
    pub fn stage(&mut self, graph: PreparedGraph) -> GraphSwapRequest {
        let generation = GraphGeneration(self.next_generation);
        self.next_generation = self.next_generation.wrapping_add(1);
        self.staged = Some(StagedGraph { graph, generation });
        GraphSwapRequest { generation }
    }

    /// Borrows the arena as the contract used by the audio thread for a block.
    pub fn activate_audio_thread(&mut self) -> AudioThread<'_> {
        AudioThread { arena: self }
    }

    /// Returns the current active graph outside an audio activation.
    #[must_use]
    pub const fn active_graph(&self) -> &PreparedGraph {
        &self.active.graph
    }

    /// Returns the current active graph generation.
    #[must_use]
    pub const fn active_generation(&self) -> GraphGeneration {
        self.active.generation
    }
}

/// A request to activate a staged graph at the next block boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphSwapRequest {
    /// Generation that will be acknowledged when the swap occurs.
    pub generation: GraphGeneration,
}

/// Monotonic identifier assigned to a staged graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphGeneration(pub u64);

/// An exclusive Phase 0 audio-thread view of a graph arena.
///
/// This type does not execute DSP. It provides the allocation-free,
/// lock-free block-boundary graph activation contract that a later audio layer
/// will use before processing a block.
pub struct AudioThread<'arena> {
    arena: &'arena mut GraphArena,
}

impl AudioThread<'_> {
    /// Activates the latest staged graph at a block boundary and reports it.
    ///
    /// This method only reads, writes, and moves `Copy` fixed-size graph data.
    /// It therefore performs no heap allocation and acquires no lock.
    #[must_use]
    pub fn activate_block(&mut self) -> BlockActivation {
        let acknowledgement = self.arena.staged.take().map(|staged| {
            self.arena.active = staged;
            GraphSwapAcknowledgement {
                generation: staged.generation,
            }
        });
        BlockActivation {
            graph: self.arena.active.graph,
            active_generation: self.arena.active.generation,
            acknowledgement,
        }
    }
}

/// Graph information available to the audio thread for one block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockActivation {
    graph: PreparedGraph,
    active_generation: GraphGeneration,
    acknowledgement: Option<GraphSwapAcknowledgement>,
}

impl BlockActivation {
    /// Returns the graph activated for this block.
    #[must_use]
    pub const fn graph(&self) -> &PreparedGraph {
        &self.graph
    }

    /// Returns the active generation for this block.
    #[must_use]
    pub const fn active_generation(&self) -> GraphGeneration {
        self.active_generation
    }

    /// Returns an acknowledgement only when this block swapped a graph.
    #[must_use]
    pub const fn acknowledgement(&self) -> Option<GraphSwapAcknowledgement> {
        self.acknowledgement
    }
}

/// Proof that a particular staged graph became active at a block boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphSwapAcknowledgement {
    /// Generation that was made active.
    pub generation: GraphGeneration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StagedGraph {
    graph: PreparedGraph,
    generation: GraphGeneration,
}

/// Allocation-free state for one rack's real-time worker gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RackGateState {
    /// The rack may publish its next request.
    Open,
    /// The rack is waiting for exactly one published ticket.
    Awaiting {
        /// Ticket whose completion may be accepted.
        ticket: BlockTicket,
        /// Host block index which published the request.
        dispatch_block: u64,
    },
    /// The rack is latched to deterministic fallback until its worker and bank are replaced.
    Closed {
        /// Failure which closed the gate.
        reason: FallbackReason,
        /// Host block index which observed the failure.
        closed_block: u64,
    },
}

/// One nonblocking observation supplied to a [`RackGate`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerObservation {
    /// No matching completion was visible during this bounded observation.
    Pending,
    /// A completion was validated and consumed for the supplied ticket.
    Completed(BlockTicket),
    /// Shared protocol validation rejected the worker result.
    ProtocolFault(ProtocolError),
    /// The control plane observed that the worker exited or became unavailable.
    WorkerExited,
}

/// Rack-local reasons for selecting deterministic fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackReason {
    /// The expected completion was absent at its deadline.
    DeadlineMiss,
    /// The worker exited or failed its liveness contract.
    WorkerExited,
    /// Completion metadata exceeded a fixed protocol capacity.
    MalformedCompletion,
    /// A completion generation or sequence did not match the live request.
    StaleCompletion,
    /// The protocol reported another invalid state or ownership transition.
    InvalidProtocolState,
    /// A previous request was still pending when a new dispatch was attempted.
    SlotUnavailable,
}

/// Result of one bounded gate operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateOutcome {
    /// A request was accepted as the rack's single live ticket.
    DispatchAllowed,
    /// No completion was visible; the caller may continue the block without waiting.
    Awaiting,
    /// The expected worker result was accepted exactly once.
    WorkerResultAccepted,
    /// The rack must render its configured fallback for this and later blocks.
    UseFallback(FallbackReason),
}

/// Deterministic, nonblocking state machine for one isolated rack worker.
///
/// The gate accepts at most one live ticket. A deadline, worker loss, or protocol fault
/// closes only this rack and permanently rejects late results. Reopening requires an
/// explicit control-thread reset after the old worker is reaped and a fresh bank is
/// installed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RackGate {
    state: RackGateState,
    consecutive_deadline_misses: u8,
    expired_ticket: Option<BlockTicket>,
}

impl RackGate {
    /// Creates an open gate with no live request.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: RackGateState::Open,
            consecutive_deadline_misses: 0,
            expired_ticket: None,
        }
    }

    /// Returns the current fixed-size gate state.
    #[must_use]
    pub const fn state(self) -> RackGateState {
        self.state
    }

    /// Records one published ticket without waiting for its completion.
    #[must_use]
    pub fn dispatch(&mut self, ticket: BlockTicket, block_index: u64) -> GateOutcome {
        match self.state {
            RackGateState::Open if ticket.is_valid() => {
                self.state = RackGateState::Awaiting {
                    ticket,
                    dispatch_block: block_index,
                };
                GateOutcome::DispatchAllowed
            }
            RackGateState::Open => self.close(FallbackReason::InvalidProtocolState, block_index),
            RackGateState::Awaiting { .. } => {
                self.close(FallbackReason::SlotUnavailable, block_index)
            }
            RackGateState::Closed { reason, .. } => GateOutcome::UseFallback(reason),
        }
    }

    /// Applies one bounded worker observation to the live ticket.
    #[must_use]
    pub fn observe(&mut self, observation: WorkerObservation, block_index: u64) -> GateOutcome {
        match self.state {
            RackGateState::Closed { reason, .. } => GateOutcome::UseFallback(reason),
            RackGateState::Open => match observation {
                WorkerObservation::WorkerExited => {
                    self.close(FallbackReason::WorkerExited, block_index)
                }
                // A completion for a ticket that already expired is late, not a protocol
                // violation: the block was rendered with fallback and the gate stays usable.
                WorkerObservation::Completed(completed)
                    if self.expired_ticket == Some(completed) =>
                {
                    self.expired_ticket = None;
                    GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
                }
                _ => self.close(FallbackReason::InvalidProtocolState, block_index),
            },
            RackGateState::Awaiting { ticket, .. } => match observation {
                WorkerObservation::Pending => GateOutcome::Awaiting,
                WorkerObservation::Completed(completed) if completed == ticket => {
                    self.state = RackGateState::Open;
                    self.consecutive_deadline_misses = 0;
                    GateOutcome::WorkerResultAccepted
                }
                WorkerObservation::Completed(completed)
                    if self.expired_ticket == Some(completed) =>
                {
                    self.expired_ticket = None;
                    GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
                }
                WorkerObservation::Completed(_) => {
                    self.close(FallbackReason::StaleCompletion, block_index)
                }
                WorkerObservation::ProtocolFault(error) => {
                    self.close(fallback_reason(error), block_index)
                }
                WorkerObservation::WorkerExited => {
                    self.close(FallbackReason::WorkerExited, block_index)
                }
            },
        }
    }

    /// Falls back for an expired block and closes after three consecutive misses.
    #[must_use]
    pub fn deadline_expired(&mut self, block_index: u64) -> GateOutcome {
        match self.state {
            RackGateState::Awaiting { ticket, .. } => {
                self.consecutive_deadline_misses =
                    self.consecutive_deadline_misses.saturating_add(1);
                self.expired_ticket = Some(ticket);
                if self.consecutive_deadline_misses >= 3 {
                    self.close(FallbackReason::DeadlineMiss, block_index)
                } else {
                    self.state = RackGateState::Open;
                    GateOutcome::UseFallback(FallbackReason::DeadlineMiss)
                }
            }
            RackGateState::Closed { reason, .. } => GateOutcome::UseFallback(reason),
            RackGateState::Open => self.close(FallbackReason::InvalidProtocolState, block_index),
        }
    }

    /// Closes this rack immediately for a hard deadline failure, such as an active device
    /// callback exhausting its completion budget. Unlike [`RackGate::deadline_expired`], no
    /// consecutive-miss tolerance applies: the abandoned slot cannot be reused, so the rack
    /// stays latched to fallback until the control plane replaces its worker and bank.
    #[must_use]
    pub fn deadline_expired_hard(&mut self, block_index: u64) -> GateOutcome {
        match self.state {
            RackGateState::Closed { reason, .. } => GateOutcome::UseFallback(reason),
            RackGateState::Awaiting { ticket, .. } => {
                self.expired_ticket = Some(ticket);
                self.close(FallbackReason::DeadlineMiss, block_index)
            }
            RackGateState::Open => self.close(FallbackReason::InvalidProtocolState, block_index),
        }
    }

    /// Closes this rack after an off-thread liveness monitor observes worker loss.
    #[must_use]
    pub fn worker_lost(&mut self, block_index: u64) -> GateOutcome {
        match self.state {
            RackGateState::Closed { reason, .. } => GateOutcome::UseFallback(reason),
            RackGateState::Open | RackGateState::Awaiting { .. } => {
                self.close(FallbackReason::WorkerExited, block_index)
            }
        }
    }

    /// Reopens the gate after the control plane installs a fresh worker and bank.
    pub fn reset_after_replacement(&mut self) {
        self.state = RackGateState::Open;
        self.consecutive_deadline_misses = 0;
        self.expired_ticket = None;
    }

    fn close(&mut self, reason: FallbackReason, block_index: u64) -> GateOutcome {
        self.state = RackGateState::Closed {
            reason,
            closed_block: block_index,
        };
        GateOutcome::UseFallback(reason)
    }
}

impl Default for RackGate {
    fn default() -> Self {
        Self::new()
    }
}

const fn fallback_reason(error: ProtocolError) -> FallbackReason {
    match error {
        ProtocolError::MalformedCompletion => FallbackReason::MalformedCompletion,
        ProtocolError::StaleTicket | ProtocolError::StaleCompletion => {
            FallbackReason::StaleCompletion
        }
        ProtocolError::InvalidState
        | ProtocolError::InvalidRequest
        | ProtocolError::InvalidTicket
        | ProtocolError::InvalidTimestamp
        | ProtocolError::InvalidOwner
        | ProtocolError::UnexpectedState
        | ProtocolError::Owned
        | ProtocolError::NotOwner => FallbackReason::InvalidProtocolState,
    }
}

/// Reasons a model cannot become a Phase 0 prepared graph.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum PrepareError {
    /// The session exceeded the graph's rack bound.
    #[error("session has {found} racks; prepared graphs support at most {capacity}")]
    TooManyRacks {
        /// Racks present in the input.
        found: usize,
        /// Maximum number of racks supported by the graph.
        capacity: usize,
    },
    /// A rack exceeded the graph's slot bound.
    #[error("rack at index {rack_index} has {found} slots; maximum is {capacity}")]
    TooManySlots {
        /// Rack position in the session.
        rack_index: usize,
        /// Slots present in the input.
        found: usize,
        /// Maximum number of slots supported by the graph.
        capacity: usize,
    },
    /// The platform-neutral model itself is invalid.
    #[error("invalid session model: {0}")]
    InvalidSession(ValidationError),
    /// The graph executor does not implement the requested rack topology.
    #[error("rack at index {rack_index} requests unsupported topology {topology:?}")]
    UnsupportedTopology {
        /// Rack position in the session.
        rack_index: usize,
        /// Requested topology.
        topology: RackTopology,
    },
    /// The graph executor does not implement the requested channel layout.
    #[error("rack at index {rack_index} uses unsupported channel layout {layout:?}")]
    UnsupportedChannelLayout {
        /// Rack position in the session.
        rack_index: usize,
        /// Unsupported layout.
        layout: ChannelLayout,
    },
    /// Rack source and endpoint do not share a supported layout.
    #[error(
        "rack at index {rack_index} connects {source_layout:?} source channels to {endpoint_layout:?} endpoint channels"
    )]
    MismatchedChannelLayouts {
        /// Rack position in the session.
        rack_index: usize,
        /// Prepared source layout.
        source_layout: PreparedChannelLayout,
        /// Prepared endpoint layout.
        endpoint_layout: PreparedChannelLayout,
    },
}

/// Fixed-capacity dry delay used when a failed rack falls back to delayed input.
///
/// Storage is owned by the caller. This type only advances indexes and copies samples; it
/// never allocates.
#[derive(Debug)]
pub struct DryDelayLine<'a> {
    storage: &'a mut [f32],
    channels: usize,
    write_frame: usize,
    delay_frames: usize,
    capacity_frames: usize,
}

impl<'a> DryDelayLine<'a> {
    /// Borrows preallocated interleaved storage for a fixed delay.
    ///
    /// # Errors
    ///
    /// Returns [`DryDelayError`] when the geometry cannot represent the delay.
    pub fn new(
        storage: &'a mut [f32],
        channels: usize,
        delay_frames: usize,
    ) -> Result<Self, DryDelayError> {
        if channels == 0 {
            return Err(DryDelayError::ZeroChannels);
        }
        let capacity_frames = storage.len() / channels;
        if capacity_frames == 0 {
            return Err(DryDelayError::EmptyStorage);
        }
        if delay_frames >= capacity_frames {
            return Err(DryDelayError::DelayExceedsCapacity {
                delay_frames,
                capacity_frames,
            });
        }
        storage.fill(0.0);
        Ok(Self {
            storage,
            channels,
            write_frame: 0,
            delay_frames,
            capacity_frames,
        })
    }

    /// Writes `input` and reads the delayed frames into `output`.
    ///
    /// `input` and `output` must contain `frames * channels` interleaved samples.
    ///
    /// # Errors
    ///
    /// Returns [`DryDelayError::BufferLength`] when either slice length is wrong.
    pub fn process_interleaved(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        frames: usize,
    ) -> Result<(), DryDelayError> {
        let samples = frames
            .checked_mul(self.channels)
            .ok_or(DryDelayError::BufferLength)?;
        if input.len() != samples || output.len() != samples {
            return Err(DryDelayError::BufferLength);
        }
        for frame in 0..frames {
            let read_frame = self
                .write_frame
                .wrapping_add(self.capacity_frames - self.delay_frames)
                % self.capacity_frames;
            for channel in 0..self.channels {
                let input_index = frame * self.channels + channel;
                let write_index = self.write_frame * self.channels + channel;
                let read_index = read_frame * self.channels + channel;
                output[input_index] = self.storage[read_index];
                self.storage[write_index] = input[input_index];
            }
            self.write_frame += 1;
            if self.write_frame == self.capacity_frames {
                self.write_frame = 0;
            }
        }
        Ok(())
    }
}

/// Errors constructing or running a [`DryDelayLine`].
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DryDelayError {
    /// Channel count must be nonzero.
    #[error("dry delay requires a nonzero channel count")]
    ZeroChannels,
    /// Storage must hold at least one frame.
    #[error("dry delay storage is empty")]
    EmptyStorage,
    /// Delay must be strictly less than storage capacity.
    #[error("delay of {delay_frames} frames exceeds storage capacity {capacity_frames}")]
    DelayExceedsCapacity {
        /// Requested delay.
        delay_frames: usize,
        /// Available frames in storage.
        capacity_frames: usize,
    },
    /// Input/output slice length does not match frames × channels.
    #[error("dry delay buffer length does not match frames and channels")]
    BufferLength,
}

/// Per-rack decision produced while driving gates for one audio block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RackBlockAction {
    /// The rack may publish a new shared-memory request.
    Dispatch,
    /// The rack is still waiting for a previously published ticket.
    AwaitCompletion,
    /// The worker result for the live ticket was accepted.
    AcceptWorkerResult,
    /// The rack must render deterministic fallback for this block.
    UseFallback(FallbackAudio),
}

/// Allocation-free planner that maps gate outcomes onto block actions.
#[derive(Debug, Default)]
pub struct LiveBlockPlanner {
    dry_delay_available: [bool; MAX_RACKS],
}

impl LiveBlockPlanner {
    /// Creates a planner with no dry-delay paths enabled.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            dry_delay_available: [false; MAX_RACKS],
        }
    }

    /// Records whether rack `index` may fall back to delayed dry audio.
    pub fn set_dry_delay_available(&mut self, index: usize, available: bool) {
        if let Some(slot) = self.dry_delay_available.get_mut(index) {
            *slot = available;
        }
    }

    /// Returns whether rack `index` may fall back to delayed dry audio.
    #[must_use]
    pub fn dry_delay_available(&self, index: usize) -> bool {
        self.dry_delay_available
            .get(index)
            .copied()
            .unwrap_or(false)
    }

    /// Interprets one gate outcome for rack `index`.
    #[must_use]
    pub fn action_for(&self, index: usize, outcome: GateOutcome) -> RackBlockAction {
        match outcome {
            GateOutcome::DispatchAllowed => RackBlockAction::Dispatch,
            GateOutcome::Awaiting => RackBlockAction::AwaitCompletion,
            GateOutcome::WorkerResultAccepted => RackBlockAction::AcceptWorkerResult,
            GateOutcome::UseFallback(reason) => {
                let dry_delay = self
                    .dry_delay_available
                    .get(index)
                    .copied()
                    .unwrap_or(false);
                RackBlockAction::UseFallback(FallbackAudio::for_reason(reason, dry_delay))
            }
        }
    }
}

/// Selects the deterministic fallback buffer for a closed rack gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackAudio {
    /// Silence (instruments / incompatible topologies).
    Silence,
    /// Delayed dry input when the rack topology matches an effect path.
    DelayedDry,
}

impl FallbackAudio {
    /// Chooses fallback audio from the closed-gate reason and whether dry delay is available.
    #[must_use]
    pub const fn for_reason(reason: FallbackReason, dry_delay_available: bool) -> Self {
        match reason {
            FallbackReason::DeadlineMiss
            | FallbackReason::WorkerExited
            | FallbackReason::MalformedCompletion
            | FallbackReason::StaleCompletion
            | FallbackReason::InvalidProtocolState
                if dry_delay_available =>
            {
                Self::DelayedDry
            }
            _ => Self::Silence,
        }
    }
}

#[cfg(test)]
mod dry_delay_tests {
    use super::{
        DryDelayError, DryDelayLine, FallbackAudio, FallbackReason, GateOutcome, LiveBlockPlanner,
        RackBlockAction,
    };

    #[test]
    fn delayed_dry_reads_prior_input() {
        let mut storage = [0.0_f32; 8];
        let mut delay = DryDelayLine::new(&mut storage, 2, 1).unwrap();
        let input = [0.5, -0.5, 1.0, -1.0];
        let mut output = [0.0; 4];
        delay
            .process_interleaved(&input[..2], &mut output[..2], 1)
            .unwrap();
        assert_eq!(&output[..2], &[0.0, 0.0]);
        delay
            .process_interleaved(&input[2..], &mut output[2..], 1)
            .unwrap();
        assert_eq!(&output[2..], &[0.5, -0.5]);
    }

    #[test]
    fn rejects_delay_equal_to_capacity() {
        let mut storage = [0.0_f32; 4];
        assert_eq!(
            DryDelayLine::new(&mut storage, 2, 2).unwrap_err(),
            DryDelayError::DelayExceedsCapacity {
                delay_frames: 2,
                capacity_frames: 2
            }
        );
    }

    #[test]
    fn fallback_prefers_delayed_dry_when_available() {
        assert_eq!(
            FallbackAudio::for_reason(FallbackReason::DeadlineMiss, true),
            FallbackAudio::DelayedDry
        );
        assert_eq!(
            FallbackAudio::for_reason(FallbackReason::DeadlineMiss, false),
            FallbackAudio::Silence
        );
    }

    #[test]
    fn live_block_planner_maps_gate_outcomes() {
        let mut planner = LiveBlockPlanner::new();
        planner.set_dry_delay_available(0, true);
        assert_eq!(
            planner.action_for(0, GateOutcome::DispatchAllowed),
            RackBlockAction::Dispatch
        );
        assert_eq!(
            planner.action_for(0, GateOutcome::UseFallback(FallbackReason::DeadlineMiss)),
            RackBlockAction::UseFallback(FallbackAudio::DelayedDry)
        );
        assert_eq!(
            planner.action_for(1, GateOutcome::UseFallback(FallbackReason::DeadlineMiss)),
            RackBlockAction::UseFallback(FallbackAudio::Silence)
        );
    }
}
