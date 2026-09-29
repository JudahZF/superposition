#![forbid(unsafe_code)]
//! Fixed-layout, versioned data structures for the Superposition host/worker IPC contract.
//!
//! Every type intended to reside in a shared bank is `#[repr(C)]` and contains only
//! primitive values, fixed-size primitive arrays, or atomics. In particular, shared
//! structures never contain Rust references, `Vec`, `String`, or Rust enum values, so the
//! same layout works in process-owned memory and in an OS-backed shared-memory region.

/// Bounded request/response control-plane transport contract.
pub mod control;
/// Bounded typed payload codecs for control-plane operations.
pub mod payload;

use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Four-byte marker at the beginning of every shared bank.
pub const PROTOCOL_MAGIC: u32 = u32::from_le_bytes(*b"SP00");
/// Version of the shared-bank binary layout.
///
/// Version 8 adds a stereo sidechain region per plug-in slot to every block slot. Older mappings
/// are rejected.
pub const PROTOCOL_VERSION: u32 = 9;
/// Worker is scanning shared request slots.
pub const WORKER_PHASE_SCANNING: u32 = 0;
/// Worker is servicing control requests.
pub const WORKER_PHASE_CONTROL: u32 = 1;
/// Worker is polling native editor state.
pub const WORKER_PHASE_EDITOR_POLL: u32 = 2;
/// Worker is waiting for a request or periodic poll deadline.
pub const WORKER_PHASE_WAITING: u32 = 3;
/// Worker is stopping.
pub const WORKER_PHASE_STOPPING: u32 = 4;
/// Number of racks available in an Alpha topology.
pub const MAX_RACKS: usize = 64;
/// Number of plugins available in each Alpha rack.
pub const MAX_PLUGINS_PER_RACK: usize = 8;
/// Maximum audio channels in either direction for one block.
pub const MAX_CHANNELS: usize = 2;
/// Maximum number of audio frames in one block.
pub const MAX_FRAMES: usize = 256;
/// Number of independently owned block slots in a bank.
pub const BLOCK_SLOT_COUNT: usize = 4;
/// Maximum MIDI packets carried by one block.
pub const MAX_MIDI_EVENTS: usize = 256;
/// Maximum automation or transport events carried by one block.
pub const MAX_EVENTS: usize = 256;
/// Block-event discriminator for a normalized plug-in parameter change.
pub const BLOCK_EVENT_PARAMETER: u32 = 1;
/// Block-event discriminator for a plug-in slot bypass change.
pub const BLOCK_EVENT_SLOT_BYPASS: u32 = 2;
/// Length of the fixed, UTF-8-by-convention plugin identifier buffer.
pub const PLUGIN_IDENTIFIER_BYTES: usize = 64;

const MAX_RACKS_U32: u32 = 64;
const MAX_PLUGINS_PER_RACK_U32: u32 = 8;
const MAX_CHANNELS_U32: u32 = 2;
const MAX_FRAMES_U32: u32 = 256;
const BLOCK_SLOT_COUNT_U32: u32 = 4;
const MAX_MIDI_EVENTS_U32: u32 = 256;
const MAX_EVENTS_U32: u32 = 256;

/// Unclaimed ownership value.
pub const OWNER_NONE: u32 = 0;
const OWNER_REQUESTING: u32 = u32::MAX;
const OWNER_COMPLETING: u32 = u32::MAX - 1;
const OWNER_RECLAIMING: u32 = u32::MAX - 2;

/// A safely interpreted slot state. The raw shared representation is an `AtomicU32`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotState {
    /// The slot has no active request and may be prepared by a producer.
    Free = 0,
    /// A producer published a request which a worker may claim.
    Requested = 1,
    /// A worker owns the request and may write its output payload.
    Processing = 2,
    /// A worker published a validated completion for the producer to consume.
    Complete = 3,
    /// A request was cancelled and must be reclaimed before reuse.
    Abandoned = 4,
}

impl SlotState {
    /// Decodes a raw slot state without ever constructing an invalid enum value.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Free),
            1 => Some(Self::Requested),
            2 => Some(Self::Processing),
            3 => Some(Self::Complete),
            4 => Some(Self::Abandoned),
            _ => None,
        }
    }

    /// Returns the stable integer used in shared memory.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self as u32
    }
}

/// Identifies one request across slot reuse and session generations.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockTicket {
    /// Session generation chosen by the bank owner.
    pub generation: u64,
    /// Monotonically allocated request sequence within the bank.
    pub sequence: u64,
}

impl BlockTicket {
    /// Returns whether this ticket has valid nonzero identifiers.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.generation != 0 && self.sequence != 0
    }
}

/// Cross-process monotonic timestamps for one block lifecycle.
///
/// Values are raw ticks from a platform clock shared by the host and worker. An all-zero
/// record denotes an untimed compatibility operation; otherwise every value is nonzero
/// and ordered from request publication through completion publication.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockTiming {
    /// Tick captured immediately before the host publishes the request.
    pub request_published_tick: u64,
    /// Tick captured when the worker claims the request.
    pub worker_claimed_tick: u64,
    /// Tick captured immediately before the worker publishes completion.
    pub completion_published_tick: u64,
}

impl BlockTiming {
    /// Returns whether this is a coherent timed record or the all-zero untimed record.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        let untimed = self.request_published_tick == 0
            && self.worker_claimed_tick == 0
            && self.completion_published_tick == 0;
        let timed = self.request_published_tick != 0
            && self.worker_claimed_tick >= self.request_published_tick
            && self.completion_published_tick >= self.worker_claimed_tick;
        untimed || timed
    }
}

/// Immutable metadata observed from a published completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletionSnapshot {
    /// Ticket attached to the completion.
    pub ticket: BlockTicket,
    /// Bounded request dimensions retained by the slot.
    pub request: BlockRequest,
    /// Cross-process lifecycle timing for the request.
    pub timing: BlockTiming,
}

/// Fixed request metadata supplied before a producer publishes a slot.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockRequest {
    /// Number of valid frames in the fixed audio arrays.
    pub frame_count: u32,
    /// Number of valid input channels.
    pub input_channel_count: u32,
    /// Number of valid output channels.
    pub output_channel_count: u32,
    /// Number of valid entries in `BlockSlot::midi_events`.
    pub midi_event_count: u32,
    /// Number of valid entries in `BlockSlot::events`.
    pub event_count: u32,
    /// Protocol-defined request flags.
    pub flags: u32,
    /// Bit `n` marks `BlockSlot::sidechain_audio[n]` as holding this block's sidechain for
    /// plug-in slot `n`.
    pub sidechain_slots: u32,
}

impl BlockRequest {
    /// Checks that every bounded count fits the fixed audio layout.
    ///
    /// An Alpha request always carries at least one frame and one output channel. Input
    /// remains allowed to be zero so an instrument rack can produce audio without an
    /// upstream audio bus.
    pub const fn is_valid(self) -> bool {
        self.frame_count != 0
            && self.frame_count <= MAX_FRAMES_U32
            && self.input_channel_count <= MAX_CHANNELS_U32
            && self.output_channel_count != 0
            && self.output_channel_count <= MAX_CHANNELS_U32
            && self.midi_event_count <= MAX_MIDI_EVENTS_U32
            && self.event_count <= MAX_EVENTS_U32
            && self.flags == 0
            && self.sidechain_slots >> MAX_PLUGINS_PER_RACK_U32 == 0
    }
}

/// Stable, primitive-only plugin description held in the topology section.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginDescriptor {
    /// UTF-8-by-convention plugin identifier bytes, NUL padded.
    pub identifier: [u8; PLUGIN_IDENTIFIER_BYTES],
    /// Zero disables the plugin; nonzero enables it.
    pub enabled: u32,
    /// Declared plugin input-channel count.
    pub input_channel_count: u32,
    /// Declared plugin output-channel count.
    pub output_channel_count: u32,
    /// Reserved for future compatible layout expansion.
    pub reserved: u32,
}

impl PluginDescriptor {
    /// An empty disabled plugin descriptor.
    pub const EMPTY: Self = Self {
        identifier: [0; PLUGIN_IDENTIFIER_BYTES],
        enabled: 0,
        input_channel_count: 0,
        output_channel_count: 0,
        reserved: 0,
    };

    /// Returns whether this descriptor is the all-zero unused entry.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        if self.enabled != 0
            || self.input_channel_count != 0
            || self.output_channel_count != 0
            || self.reserved != 0
        {
            return false;
        }
        let mut index = 0;
        while index < PLUGIN_IDENTIFIER_BYTES {
            if self.identifier[index] != 0 {
                return false;
            }
            index += 1;
        }
        true
    }

    /// Returns whether this raw descriptor uses an Alpha-supported main-bus layout.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        let is_disabled = self.enabled == 0
            && self.input_channel_count == 0
            && self.output_channel_count == 0
            && self.reserved == 0;
        let is_enabled = self.enabled == 1
            && self.input_channel_count <= MAX_CHANNELS_U32
            && self.output_channel_count >= 1
            && self.output_channel_count <= MAX_CHANNELS_U32
            && self.reserved == 0;
        is_disabled || is_enabled
    }
}

/// Fixed plugin list for a single Alpha rack.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RackDescriptor {
    /// Number of valid leading entries in `plugins`.
    pub plugin_count: u32,
    /// Zero processes the rack normally; nonzero bypasses it.
    pub bypassed: u32,
    /// Fixed-capacity plugin entries.
    pub plugins: [PluginDescriptor; MAX_PLUGINS_PER_RACK],
}

impl RackDescriptor {
    /// An empty rack descriptor.
    pub const EMPTY: Self = Self {
        plugin_count: 0,
        bypassed: 0,
        plugins: [PluginDescriptor::EMPTY; MAX_PLUGINS_PER_RACK],
    };

    /// Returns whether the descriptor uses only fixed Alpha capacities and layouts.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        if self.plugin_count > MAX_PLUGINS_PER_RACK_U32 {
            return false;
        }

        let mut index = 0;
        while index < MAX_PLUGINS_PER_RACK {
            let plugin = self.plugins[index];
            let is_declared = index < self.plugin_count as usize;
            if !plugin.is_valid() || (!is_declared && !plugin.is_empty()) {
                return false;
            }
            index += 1;
        }
        true
    }
}

/// One bounded MIDI message in a block payload.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MidiEvent {
    /// Frame offset from the beginning of the block.
    pub frame_offset: u32,
    /// MIDI port selected by the sender.
    pub port: u32,
    /// Number of valid bytes in `data`, from zero through three.
    pub data_length: u32,
    /// Packed MIDI 1.0 message bytes, zero padded.
    pub data: [u8; 3],
    /// Protocol-defined MIDI flags.
    pub flags: u8,
}

impl MidiEvent {
    /// Checks the MIDI message's fixed bounds against a block length.
    #[must_use]
    pub const fn is_valid(self, frame_count: u32) -> bool {
        self.frame_offset < frame_count && self.data_length <= 3
    }
}

/// One bounded non-MIDI event in a block payload.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BlockEvent {
    /// Frame offset from the beginning of the block.
    pub frame_offset: u32,
    /// Protocol-defined event discriminator.
    pub event_type: u32,
    /// Protocol-defined target or parameter identifier.
    pub key: u32,
    /// Protocol-defined numeric payload.
    pub value: f32,
    /// Protocol-defined event flags.
    pub flags: u32,
}

impl BlockEvent {
    /// Checks the event's frame offset against a block length.
    #[must_use]
    pub const fn is_valid(self, frame_count: u32) -> bool {
        self.frame_offset < frame_count && self.value.is_finite()
    }
}

/// Atomic and scalar metadata for a block slot.
///
/// The state field stores a `SlotState::raw()` value, rather than an enum, so the
/// memory image remains an explicit C-compatible primitive contract.
#[repr(C, align(64))]
pub struct BlockMetadata {
    /// Atomic raw `SlotState` value.
    pub state: AtomicU32,
    /// Atomic holder ID for temporary publication and worker ownership.
    pub owner: AtomicU32,
    /// Generation of the currently published request.
    pub request_generation: AtomicU64,
    /// Sequence of the currently published request.
    pub request_sequence: AtomicU64,
    /// Generation attached to the most recently published completion.
    pub completion_generation: AtomicU64,
    /// Sequence attached to the most recently published completion.
    pub completion_sequence: AtomicU64,
    /// Host monotonic tick captured for request publication.
    pub request_published_tick: AtomicU64,
    /// Worker monotonic tick captured when the request was claimed.
    pub worker_claimed_tick: AtomicU64,
    /// Worker monotonic tick captured for completion publication.
    pub completion_published_tick: AtomicU64,
    /// Number of valid frames in the slot's audio arrays.
    pub frame_count: u32,
    /// Number of valid input channels in the slot's audio arrays.
    pub input_channel_count: u32,
    /// Number of valid output channels in the slot's audio arrays.
    pub output_channel_count: u32,
    /// Number of valid MIDI entries in the slot's MIDI array.
    pub midi_event_count: u32,
    /// Number of valid non-MIDI entries in the slot's event array.
    pub event_count: u32,
    /// Protocol-defined block flags.
    pub flags: u32,
    /// Plug-in slots whose sidechain region holds this block's audio, as a bitmask.
    pub sidechain_slots: u32,
    /// Reserved for compatible layout expansion.
    pub reserved: u32,
}

impl BlockMetadata {
    /// Creates an empty free metadata record.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(SlotState::Free.raw()),
            owner: AtomicU32::new(OWNER_NONE),
            request_generation: AtomicU64::new(0),
            request_sequence: AtomicU64::new(0),
            completion_generation: AtomicU64::new(0),
            completion_sequence: AtomicU64::new(0),
            request_published_tick: AtomicU64::new(0),
            worker_claimed_tick: AtomicU64::new(0),
            completion_published_tick: AtomicU64::new(0),
            frame_count: 0,
            input_channel_count: 0,
            output_channel_count: 0,
            midi_event_count: 0,
            event_count: 0,
            flags: 0,
            sidechain_slots: 0,
            reserved: 0,
        }
    }

    /// Loads the current state, reporting an invalid raw value without UB.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidState`] when memory contains an unknown value.
    pub fn state(&self) -> Result<SlotState, ProtocolError> {
        SlotState::from_raw(self.state.load(Ordering::Acquire)).ok_or(ProtocolError::InvalidState)
    }

    fn ticket(&self) -> BlockTicket {
        BlockTicket {
            generation: self.request_generation.load(Ordering::Acquire),
            sequence: self.request_sequence.load(Ordering::Acquire),
        }
    }

    fn completion_ticket(&self) -> BlockTicket {
        BlockTicket {
            generation: self.completion_generation.load(Ordering::Acquire),
            sequence: self.completion_sequence.load(Ordering::Acquire),
        }
    }

    fn request(&self) -> BlockRequest {
        BlockRequest {
            frame_count: self.frame_count,
            input_channel_count: self.input_channel_count,
            output_channel_count: self.output_channel_count,
            midi_event_count: self.midi_event_count,
            event_count: self.event_count,
            flags: self.flags,
            sidechain_slots: self.sidechain_slots,
        }
    }

    fn timing(&self) -> BlockTiming {
        BlockTiming {
            request_published_tick: self.request_published_tick.load(Ordering::Acquire),
            worker_claimed_tick: self.worker_claimed_tick.load(Ordering::Acquire),
            completion_published_tick: self.completion_published_tick.load(Ordering::Acquire),
        }
    }
}

impl Default for BlockMetadata {
    fn default() -> Self {
        Self::new()
    }
}

/// Fixed audio, MIDI, event, and metadata storage for one block slot.
#[repr(C, align(64))]
pub struct BlockSlot {
    /// Atomic control plane and bounded payload counts.
    pub metadata: BlockMetadata,
    /// Producer-to-worker audio input, indexed by channel then frame.
    pub input_audio: [[f32; MAX_FRAMES]; MAX_CHANNELS],
    /// Producer-to-worker stereo sidechain, indexed by plug-in slot, channel, then frame. Only
    /// slots marked in `BlockRequest::sidechain_slots` hold audio for the current request.
    pub sidechain_audio: [[[f32; MAX_FRAMES]; MAX_CHANNELS]; MAX_PLUGINS_PER_RACK],
    /// Worker-to-producer audio output, indexed by channel then frame.
    pub output_audio: [[f32; MAX_FRAMES]; MAX_CHANNELS],
    /// Bounded producer-to-worker MIDI payload.
    pub midi_events: [MidiEvent; MAX_MIDI_EVENTS],
    /// Bounded producer-to-worker non-MIDI payload.
    pub events: [BlockEvent; MAX_EVENTS],
}

impl BlockSlot {
    /// Creates a zeroed, free block slot using no unsafe initialization.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            metadata: BlockMetadata::new(),
            input_audio: [[0.0; MAX_FRAMES]; MAX_CHANNELS],
            sidechain_audio: [[[0.0; MAX_FRAMES]; MAX_CHANNELS]; MAX_PLUGINS_PER_RACK],
            output_audio: [[0.0; MAX_FRAMES]; MAX_CHANNELS],
            midi_events: [MidiEvent {
                frame_offset: 0,
                port: 0,
                data_length: 0,
                data: [0; 3],
                flags: 0,
            }; MAX_MIDI_EVENTS],
            events: [BlockEvent {
                frame_offset: 0,
                event_type: 0,
                key: 0,
                value: 0.0,
                flags: 0,
            }; MAX_EVENTS],
        }
    }

    fn validate_request_payload(&self, request: BlockRequest) -> Result<(), ProtocolError> {
        if !request.is_valid() {
            return Err(ProtocolError::InvalidRequest);
        }

        let midi_event_count =
            usize::try_from(request.midi_event_count).map_err(|_| ProtocolError::InvalidRequest)?;
        let event_count =
            usize::try_from(request.event_count).map_err(|_| ProtocolError::InvalidRequest)?;
        if self.midi_events[..midi_event_count]
            .iter()
            .any(|event| !event.is_valid(request.frame_count))
            || self.events[..event_count]
                .iter()
                .any(|event| !event.is_valid(request.frame_count))
        {
            return Err(ProtocolError::InvalidRequest);
        }
        Ok(())
    }

    /// Validates a mapped slot's state-appropriate metadata without changing ownership.
    ///
    /// A free slot may retain payload bytes and tickets from its previous use. Every other
    /// state must have a live request with bounded payload metadata. Request, processing,
    /// completion, and abandoned states additionally require the tickets, owner, and partial
    /// timestamp sequence that their published state permits.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when memory contains an unknown state, malformed active
    /// request, invalid ticket or owner, inconsistent completion, or invalid timing record.
    pub fn validate_mapped_contents(&self) -> Result<(), ProtocolError> {
        let state = self.metadata.state()?;
        let owner = self.metadata.owner.load(Ordering::Acquire);
        if state == SlotState::Free {
            return (owner == OWNER_NONE)
                .then_some(())
                .ok_or(ProtocolError::InvalidOwner);
        }

        let request = self.metadata.request();
        let ticket = self.metadata.ticket();
        if !ticket.is_valid() {
            return Err(ProtocolError::InvalidTicket);
        }
        self.validate_request_payload(request)?;
        if self.metadata.reserved != 0 {
            return Err(ProtocolError::InvalidRequest);
        }

        let completion = self.metadata.completion_ticket();
        let timing = self.metadata.timing();
        match state {
            SlotState::Requested => {
                require_no_slot_owner(owner)?;
                require_empty_completion(completion)?;
                validate_requested_timing(timing)
            }
            SlotState::Processing => {
                require_worker_slot_owner(owner)?;
                require_empty_completion(completion)?;
                validate_processing_timing(timing)
            }
            SlotState::Complete => {
                require_no_slot_owner(owner)?;
                if completion != ticket || !completion.is_valid() {
                    return Err(ProtocolError::MalformedCompletion);
                }
                timing
                    .is_valid()
                    .then_some(())
                    .ok_or(ProtocolError::InvalidTimestamp)
            }
            SlotState::Abandoned => {
                require_no_slot_owner(owner)?;
                require_empty_completion(completion)?;
                validate_abandoned_timing(timing)
            }
            SlotState::Free => unreachable!("free slots return before active validation"),
        }
    }

    /// Publishes a producer request from a free slot.
    ///
    /// The caller must have exclusively written the input payload before calling this
    /// method. The release publication makes that payload visible to a successful
    /// worker claim.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid ticket or request, a non-free slot, or a slot
    /// temporarily owned by another transition.
    pub fn publish_request(
        &mut self,
        ticket: BlockTicket,
        request: BlockRequest,
    ) -> Result<(), ProtocolError> {
        self.publish_request_inner(ticket, request, 0)
    }

    /// Publishes a producer request with a nonzero shared monotonic timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidTimestamp`] for a zero timestamp, in addition
    /// to the errors documented by [`Self::publish_request`].
    pub fn publish_request_at(
        &mut self,
        ticket: BlockTicket,
        request: BlockRequest,
        published_tick: u64,
    ) -> Result<(), ProtocolError> {
        if published_tick == 0 {
            return Err(ProtocolError::InvalidTimestamp);
        }
        self.publish_request_inner(ticket, request, published_tick)
    }

    fn publish_request_inner(
        &mut self,
        ticket: BlockTicket,
        request: BlockRequest,
        published_tick: u64,
    ) -> Result<(), ProtocolError> {
        if !ticket.is_valid() {
            return Err(ProtocolError::InvalidTicket);
        }
        self.validate_request_payload(request)?;
        if self.metadata.reserved != 0 {
            return Err(ProtocolError::InvalidRequest);
        }
        self.acquire_owner(OWNER_REQUESTING)?;
        let state = match self.metadata.state() {
            Ok(state) => state,
            Err(error) => {
                self.release_owner(OWNER_REQUESTING);
                return Err(error);
            }
        };
        if state != SlotState::Free {
            self.release_owner(OWNER_REQUESTING);
            return Err(ProtocolError::UnexpectedState);
        }

        self.metadata
            .request_generation
            .store(ticket.generation, Ordering::Relaxed);
        self.metadata
            .request_sequence
            .store(ticket.sequence, Ordering::Relaxed);
        self.metadata
            .completion_generation
            .store(0, Ordering::Relaxed);
        self.metadata
            .completion_sequence
            .store(0, Ordering::Relaxed);
        self.metadata
            .request_published_tick
            .store(published_tick, Ordering::Relaxed);
        self.metadata
            .worker_claimed_tick
            .store(0, Ordering::Relaxed);
        self.metadata
            .completion_published_tick
            .store(0, Ordering::Relaxed);
        self.metadata.frame_count = request.frame_count;
        self.metadata.input_channel_count = request.input_channel_count;
        self.metadata.output_channel_count = request.output_channel_count;
        self.metadata.midi_event_count = request.midi_event_count;
        self.metadata.event_count = request.event_count;
        self.metadata.flags = request.flags;
        self.metadata.sidechain_slots = request.sidechain_slots;
        self.metadata
            .state
            .store(SlotState::Requested.raw(), Ordering::Release);
        self.release_owner(OWNER_REQUESTING);
        Ok(())
    }

    /// Claims a published request for a worker with a nonzero owner ID.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid worker ID, non-requested slot, a slot owned
    /// by another transition, or an untimed claim of a timed request.
    pub fn claim_for_processing(&self, worker_id: u32) -> Result<BlockTicket, ProtocolError> {
        self.claim_for_processing_inner(worker_id, 0, None)
    }

    /// Claims a published request with a nonzero shared monotonic timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidTimestamp`] for a zero, mixed-mode, or
    /// pre-publication timestamp, in addition to the errors documented by
    /// [`Self::claim_for_processing`].
    pub fn claim_for_processing_at(
        &self,
        worker_id: u32,
        claimed_tick: u64,
    ) -> Result<BlockTicket, ProtocolError> {
        if claimed_tick == 0 {
            return Err(ProtocolError::InvalidTimestamp);
        }
        self.claim_for_processing_inner(worker_id, claimed_tick, None)
    }

    /// Claims a specific published ticket with a nonzero shared monotonic timestamp.
    ///
    /// This conditional claim is useful when work performed before claiming, such as a
    /// deterministic delay, must apply to exactly the ticket first observed by the
    /// worker. Slot reuse cannot cause the operation to claim a different request.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::StaleTicket`] if the requested slot now contains a
    /// different ticket. Other errors match [`Self::claim_for_processing_at`].
    pub fn claim_ticket_for_processing_at(
        &self,
        worker_id: u32,
        expected_ticket: BlockTicket,
        claimed_tick: u64,
    ) -> Result<BlockTicket, ProtocolError> {
        if !expected_ticket.is_valid() {
            return Err(ProtocolError::InvalidTicket);
        }
        if claimed_tick == 0 {
            return Err(ProtocolError::InvalidTimestamp);
        }
        self.claim_for_processing_inner(worker_id, claimed_tick, Some(expected_ticket))
    }

    fn claim_for_processing_inner(
        &self,
        worker_id: u32,
        claimed_tick: u64,
        expected_ticket: Option<BlockTicket>,
    ) -> Result<BlockTicket, ProtocolError> {
        if worker_id == OWNER_NONE || is_reserved_owner(worker_id) {
            return Err(ProtocolError::InvalidOwner);
        }
        if self.metadata.state()? != SlotState::Requested {
            return Err(ProtocolError::UnexpectedState);
        }
        self.acquire_owner(worker_id)?;
        let result = (|| {
            if self.metadata.state()? != SlotState::Requested {
                return Err(ProtocolError::UnexpectedState);
            }
            let ticket = self.metadata.ticket();
            if !ticket.is_valid() {
                return Err(ProtocolError::InvalidTicket);
            }
            self.validate_request_payload(self.metadata.request())?;
            if expected_ticket.is_some_and(|expected| expected != ticket) {
                return Err(ProtocolError::StaleTicket);
            }
            let request_published_tick =
                self.metadata.request_published_tick.load(Ordering::Acquire);
            if (request_published_tick == 0) != (claimed_tick == 0)
                || (claimed_tick != 0 && claimed_tick < request_published_tick)
            {
                return Err(ProtocolError::InvalidTimestamp);
            }

            self.metadata
                .worker_claimed_tick
                .store(claimed_tick, Ordering::Relaxed);
            self.metadata
                .state
                .compare_exchange(
                    SlotState::Requested.raw(),
                    SlotState::Processing.raw(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .map(|_| ticket)
                .map_err(|_| ProtocolError::UnexpectedState)
        })();
        if result.is_err() {
            self.metadata
                .worker_claimed_tick
                .store(0, Ordering::Relaxed);
            self.release_owner(worker_id);
        }
        result
    }

    /// Publishes a worker completion after validating its generation and sequence.
    ///
    /// The caller must have written the output payload before this method. A stale
    /// worker cannot complete a slot that has been abandoned and reused because both
    /// ticket components are checked while the worker owns the slot.
    ///
    /// # Errors
    ///
    /// Returns an error unless `worker_id` owns a processing slot whose live ticket
    /// matches `ticket` and the untimed request and claim both used zero timestamps.
    pub fn publish_completion(
        &self,
        worker_id: u32,
        ticket: BlockTicket,
    ) -> Result<(), ProtocolError> {
        self.publish_completion_inner(worker_id, ticket, 0)
    }

    /// Publishes a worker completion with a nonzero shared monotonic timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidTimestamp`] for a zero or out-of-order
    /// timestamp, in addition to the errors documented by [`Self::publish_completion`].
    pub fn publish_completion_at(
        &self,
        worker_id: u32,
        ticket: BlockTicket,
        completed_tick: u64,
    ) -> Result<(), ProtocolError> {
        if completed_tick == 0 {
            return Err(ProtocolError::InvalidTimestamp);
        }
        self.publish_completion_inner(worker_id, ticket, completed_tick)
    }

    fn publish_completion_inner(
        &self,
        worker_id: u32,
        ticket: BlockTicket,
        completed_tick: u64,
    ) -> Result<(), ProtocolError> {
        self.require_worker(worker_id, ticket)?;
        self.validate_completion_timing(completed_tick)?;
        self.publish_completion_metadata(worker_id, ticket, completed_tick);
        Ok(())
    }

    fn publish_completion_metadata(
        &self,
        worker_id: u32,
        completion_ticket: BlockTicket,
        completed_tick: u64,
    ) {
        self.metadata
            .completion_generation
            .store(completion_ticket.generation, Ordering::Relaxed);
        self.metadata
            .completion_sequence
            .store(completion_ticket.sequence, Ordering::Relaxed);
        self.metadata
            .completion_published_tick
            .store(completed_tick, Ordering::Relaxed);
        self.metadata
            .state
            .store(SlotState::Complete.raw(), Ordering::Release);
        self.release_owner(worker_id);
    }

    /// Deliberately publishes completion metadata with an invalid frame count.
    ///
    /// This test-only hook lets protocol tests prove that consumers reject the fault.
    /// Production integrations must use [`Self::publish_completion`] or
    /// [`Self::publish_completion_at`].
    ///
    /// # Errors
    ///
    /// Returns an error unless `worker_id` owns the matching timed request and
    /// `completed_tick` is coherent with its earlier timestamps.
    #[cfg(test)]
    pub fn test_publish_malformed_completion_at(
        &mut self,
        worker_id: u32,
        ticket: BlockTicket,
        completed_tick: u64,
    ) -> Result<(), ProtocolError> {
        if completed_tick == 0 {
            return Err(ProtocolError::InvalidTimestamp);
        }
        self.require_worker(worker_id, ticket)?;
        self.validate_completion_timing(completed_tick)?;
        self.metadata.frame_count = MAX_FRAMES_U32 + 1;
        self.publish_completion_metadata(worker_id, ticket, completed_tick);
        Ok(())
    }

    /// Deliberately publishes a valid completion under a mismatched ticket.
    ///
    /// This test-only hook lets protocol tests prove that consumers reject the fault.
    /// Production integrations must use [`Self::publish_completion`] or
    /// [`Self::publish_completion_at`].
    ///
    /// # Errors
    ///
    /// Returns an error unless `worker_id` owns `live_ticket`, both tickets are valid and
    /// different, and `completed_tick` is coherent with the live request timestamps.
    #[cfg(test)]
    pub fn test_publish_stale_completion_at(
        &self,
        worker_id: u32,
        live_ticket: BlockTicket,
        stale_ticket: BlockTicket,
        completed_tick: u64,
    ) -> Result<(), ProtocolError> {
        if completed_tick == 0 {
            return Err(ProtocolError::InvalidTimestamp);
        }
        if !stale_ticket.is_valid() || stale_ticket == live_ticket {
            return Err(ProtocolError::InvalidTicket);
        }
        self.require_worker(worker_id, live_ticket)?;
        self.validate_completion_timing(completed_tick)?;
        self.publish_completion_metadata(worker_id, stale_ticket, completed_tick);
        Ok(())
    }

    /// Consumes a completion only when its generation and sequence match `ticket`.
    ///
    /// # Errors
    ///
    /// Returns an error if the slot is unavailable or not complete, or if the
    /// completion generation and sequence do not exactly match `ticket`.
    pub fn consume_completion(&self, ticket: BlockTicket) -> Result<(), ProtocolError> {
        self.consume_completion_timing(ticket).map(|_| ())
    }

    /// Consumes a matching completion and returns its validated cross-process timing.
    ///
    /// # Errors
    ///
    /// Returns an error for an unavailable, stale, malformed, or out-of-order
    /// completion. A rejected completion remains published for diagnosis.
    pub fn consume_completion_timing(
        &self,
        ticket: BlockTicket,
    ) -> Result<BlockTiming, ProtocolError> {
        if !ticket.is_valid() {
            return Err(ProtocolError::InvalidTicket);
        }
        if self.metadata.state()? != SlotState::Complete {
            return Err(ProtocolError::UnexpectedState);
        }
        self.acquire_owner(OWNER_COMPLETING)?;
        let result = (|| {
            let snapshot = self
                .completion_snapshot_inner()?
                .ok_or(ProtocolError::UnexpectedState)?;
            if snapshot.ticket != ticket {
                return Err(ProtocolError::StaleCompletion);
            }
            self.metadata
                .state
                .compare_exchange(
                    SlotState::Complete.raw(),
                    SlotState::Free.raw(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .map_err(|_| ProtocolError::UnexpectedState)?;
            Ok(snapshot.timing)
        })();
        self.release_owner(OWNER_COMPLETING);
        result
    }

    /// Observes a validated completion without freeing its slot.
    ///
    /// # Errors
    ///
    /// Returns an error when the raw slot state, completion ticket, retained bounds,
    /// or timing record is malformed, or when another transition temporarily owns the
    /// slot.
    pub fn completion_snapshot(&self) -> Result<Option<CompletionSnapshot>, ProtocolError> {
        // Polling an unfinished slot must not compete with the worker's claim. Recheck
        // under ownership below before reading any non-atomic completion payload.
        if self.metadata.state()? != SlotState::Complete {
            return Ok(None);
        }
        self.acquire_owner(OWNER_COMPLETING)?;
        let result = self.completion_snapshot_inner();
        self.release_owner(OWNER_COMPLETING);
        result
    }

    fn completion_snapshot_inner(&self) -> Result<Option<CompletionSnapshot>, ProtocolError> {
        if self.metadata.state()? != SlotState::Complete {
            return Ok(None);
        }
        let ticket = self.metadata.completion_ticket();
        if !ticket.is_valid() {
            return Err(ProtocolError::MalformedCompletion);
        }
        let request = self.metadata.request();
        if self.metadata.reserved != 0 || self.validate_request_payload(request).is_err() {
            return Err(ProtocolError::MalformedCompletion);
        }
        let timing = self.metadata.timing();
        if !timing.is_valid() {
            return Err(ProtocolError::InvalidTimestamp);
        }
        Ok(Some(CompletionSnapshot {
            ticket,
            request,
            timing,
        }))
    }

    /// Abandons an unclaimed request when its ticket still matches.
    ///
    /// # Errors
    ///
    /// Returns an error if a worker or another transition owns the slot, or if the
    /// slot is no longer the requested ticket.
    pub fn abandon_request(&self, ticket: BlockTicket) -> Result<(), ProtocolError> {
        if !ticket.is_valid() {
            return Err(ProtocolError::InvalidTicket);
        }
        self.acquire_owner(OWNER_RECLAIMING)?;
        let result = (|| {
            if self.metadata.state()? != SlotState::Requested || self.metadata.ticket() != ticket {
                return Err(ProtocolError::UnexpectedState);
            }
            self.metadata
                .state
                .compare_exchange(
                    SlotState::Requested.raw(),
                    SlotState::Abandoned.raw(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .map_err(|_| ProtocolError::UnexpectedState)?;
            Ok(())
        })();
        self.release_owner(OWNER_RECLAIMING);
        result
    }

    /// Abandons a processing request only when the owning worker presents its ticket.
    ///
    /// # Errors
    ///
    /// Returns an error unless `worker_id` owns a processing slot whose request
    /// generation and sequence match `ticket`.
    pub fn abandon_processing(
        &self,
        worker_id: u32,
        ticket: BlockTicket,
    ) -> Result<(), ProtocolError> {
        self.require_worker(worker_id, ticket)?;
        self.metadata
            .state
            .store(SlotState::Abandoned.raw(), Ordering::Release);
        self.release_owner(worker_id);
        Ok(())
    }

    /// Returns an abandoned slot to the free pool.
    ///
    /// # Errors
    ///
    /// Returns an error if another transition owns the slot or it is not abandoned.
    pub fn reclaim_abandoned(&self) -> Result<(), ProtocolError> {
        self.acquire_owner(OWNER_RECLAIMING)?;
        let result = (|| {
            if self.metadata.state()? != SlotState::Abandoned {
                return Err(ProtocolError::UnexpectedState);
            }
            self.metadata
                .state
                .compare_exchange(
                    SlotState::Abandoned.raw(),
                    SlotState::Free.raw(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .map(|_| ())
                .map_err(|_| ProtocolError::UnexpectedState)
        })();
        self.release_owner(OWNER_RECLAIMING);
        result
    }

    fn acquire_owner(&self, owner: u32) -> Result<(), ProtocolError> {
        self.metadata
            .owner
            .compare_exchange(OWNER_NONE, owner, Ordering::Acquire, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| ProtocolError::Owned)
    }

    fn release_owner(&self, owner: u32) {
        let _ = self.metadata.owner.compare_exchange(
            owner,
            OWNER_NONE,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    fn require_worker(&self, worker_id: u32, ticket: BlockTicket) -> Result<(), ProtocolError> {
        if !ticket.is_valid() {
            return Err(ProtocolError::InvalidTicket);
        }
        if worker_id == OWNER_NONE || is_reserved_owner(worker_id) {
            return Err(ProtocolError::InvalidOwner);
        }
        if self.metadata.owner.load(Ordering::Acquire) != worker_id {
            return Err(ProtocolError::NotOwner);
        }
        if self.metadata.state()? != SlotState::Processing {
            return Err(ProtocolError::UnexpectedState);
        }
        if self.metadata.ticket() != ticket {
            return Err(ProtocolError::StaleTicket);
        }
        Ok(())
    }

    fn validate_completion_timing(&self, completed_tick: u64) -> Result<(), ProtocolError> {
        let request_published_tick = self.metadata.request_published_tick.load(Ordering::Acquire);
        let worker_claimed_tick = self.metadata.worker_claimed_tick.load(Ordering::Acquire);
        let untimed =
            request_published_tick == 0 && worker_claimed_tick == 0 && completed_tick == 0;
        let timed = request_published_tick != 0
            && worker_claimed_tick >= request_published_tick
            && completed_tick >= worker_claimed_tick;
        if untimed || timed {
            Ok(())
        } else {
            Err(ProtocolError::InvalidTimestamp)
        }
    }
}

impl Default for BlockSlot {
    fn default() -> Self {
        Self::new()
    }
}

fn require_no_slot_owner(owner: u32) -> Result<(), ProtocolError> {
    (owner == OWNER_NONE)
        .then_some(())
        .ok_or(ProtocolError::InvalidOwner)
}

fn require_worker_slot_owner(owner: u32) -> Result<(), ProtocolError> {
    (owner != OWNER_NONE && !is_reserved_owner(owner))
        .then_some(())
        .ok_or(ProtocolError::InvalidOwner)
}

fn require_empty_completion(completion: BlockTicket) -> Result<(), ProtocolError> {
    (completion
        == BlockTicket {
            generation: 0,
            sequence: 0,
        })
    .then_some(())
    .ok_or(ProtocolError::MalformedCompletion)
}

fn validate_requested_timing(timing: BlockTiming) -> Result<(), ProtocolError> {
    let untimed = timing == BlockTiming::default();
    let timed = timing.request_published_tick != 0
        && timing.worker_claimed_tick == 0
        && timing.completion_published_tick == 0;
    (untimed || timed)
        .then_some(())
        .ok_or(ProtocolError::InvalidTimestamp)
}

fn validate_processing_timing(timing: BlockTiming) -> Result<(), ProtocolError> {
    let untimed = timing == BlockTiming::default();
    let timed = timing.request_published_tick != 0
        && timing.worker_claimed_tick >= timing.request_published_tick
        && timing.completion_published_tick == 0;
    (untimed || timed)
        .then_some(())
        .ok_or(ProtocolError::InvalidTimestamp)
}

fn validate_abandoned_timing(timing: BlockTiming) -> Result<(), ProtocolError> {
    let unclaimed = validate_requested_timing(timing).is_ok();
    let claimed = validate_processing_timing(timing).is_ok();
    (unclaimed || claimed)
        .then_some(())
        .ok_or(ProtocolError::InvalidTimestamp)
}

/// Header at the beginning of each independently usable shared bank.
#[repr(C, align(64))]
pub struct ProtocolHeader {
    /// `PROTOCOL_MAGIC` identifies this binary protocol.
    pub magic: u32,
    /// `PROTOCOL_VERSION` identifies this binary layout revision.
    pub version: u32,
    /// Size in bytes of `ProtocolHeader`.
    pub header_bytes: u32,
    /// Size in bytes of its enclosing `SharedBank`.
    pub bank_bytes: u32,
    /// Fixed Alpha rack capacity.
    pub max_racks: u32,
    /// Fixed Alpha plugin-per-rack capacity.
    pub max_plugins_per_rack: u32,
    /// Fixed Alpha channel capacity.
    pub max_channels: u32,
    /// Fixed Alpha frame capacity.
    pub max_frames: u32,
    /// Fixed block-slot capacity.
    pub block_slot_count: u32,
    /// Fixed MIDI-event capacity per slot.
    pub max_midi_events: u32,
    /// Fixed general-event capacity per slot.
    pub max_events: u32,
    /// Protocol-defined bank flags.
    pub flags: u32,
    /// Active session generation chosen by the host.
    pub generation: AtomicU64,
    /// Atomic sequence allocator for producer requests.
    pub next_request_sequence: AtomicU64,
    /// Atomic producer endpoint ownership claim.
    pub request_owner: AtomicU32,
    /// Atomic consumer endpoint ownership claim.
    pub completion_owner: AtomicU32,
    /// Latest worker liveness tick published in the shared monotonic clock domain.
    pub worker_heartbeat_tick: AtomicU64,
    /// Sum of calibrated busy-spin targets in the shared monotonic clock domain.
    pub worker_busy_requested_ticks: AtomicU64,
    /// Sum of observed calibrated busy-spin durations in the shared monotonic clock domain.
    pub worker_busy_observed_ticks: AtomicU64,
    /// Number of calibrated busy-spin operations included in the busy-time counters.
    pub worker_busy_operations: AtomicU64,
    /// Sequence used to wake a worker after a request is published.
    pub request_wake_sequence: AtomicU32,
    /// Reserved for compatible layout expansion.
    pub reserved: u32,
    /// Current processing-thread phase, using `WORKER_PHASE_*` values.
    pub worker_phase: AtomicU32,
    /// Request wake sequence observed at the beginning of the processing loop.
    pub worker_wait_sequence: AtomicU32,
    /// Monotonic tick recorded at the beginning of the processing loop.
    pub worker_loop_tick: AtomicU64,
    /// Latest rack latency in samples, published by the worker.
    pub worker_latency_samples: AtomicU32,
    /// Latched VST3 change-notification bitmask; the host clears it after observing it.
    pub worker_restart_requested: AtomicU32,
}

impl ProtocolHeader {
    /// Creates an initialized header with a caller-provided generation.
    #[must_use]
    pub fn new(bank_bytes: u32, generation: u64) -> Self {
        Self {
            magic: PROTOCOL_MAGIC,
            version: PROTOCOL_VERSION,
            header_bytes: header_size_u32(),
            bank_bytes,
            max_racks: MAX_RACKS_U32,
            max_plugins_per_rack: MAX_PLUGINS_PER_RACK_U32,
            max_channels: MAX_CHANNELS_U32,
            max_frames: MAX_FRAMES_U32,
            block_slot_count: BLOCK_SLOT_COUNT_U32,
            max_midi_events: MAX_MIDI_EVENTS_U32,
            max_events: MAX_EVENTS_U32,
            flags: 0,
            generation: AtomicU64::new(generation),
            next_request_sequence: AtomicU64::new(1),
            request_owner: AtomicU32::new(OWNER_NONE),
            completion_owner: AtomicU32::new(OWNER_NONE),
            worker_heartbeat_tick: AtomicU64::new(0),
            worker_busy_requested_ticks: AtomicU64::new(0),
            worker_busy_observed_ticks: AtomicU64::new(0),
            worker_busy_operations: AtomicU64::new(0),
            request_wake_sequence: AtomicU32::new(0),
            reserved: 0,
            worker_phase: AtomicU32::new(WORKER_PHASE_SCANNING),
            worker_wait_sequence: AtomicU32::new(0),
            worker_loop_tick: AtomicU64::new(0),
            worker_latency_samples: AtomicU32::new(0),
            worker_restart_requested: AtomicU32::new(0),
        }
    }

    /// Checks fixed header markers and capacities before accepting a bank.
    #[must_use]
    pub fn is_compatible(&self, bank_bytes: u32) -> bool {
        self.magic == PROTOCOL_MAGIC
            && self.version == PROTOCOL_VERSION
            && self.header_bytes == header_size_u32()
            && self.bank_bytes == bank_bytes
            && self.max_racks == MAX_RACKS_U32
            && self.max_plugins_per_rack == MAX_PLUGINS_PER_RACK_U32
            && self.max_channels == MAX_CHANNELS_U32
            && self.max_frames == MAX_FRAMES_U32
            && self.block_slot_count == BLOCK_SLOT_COUNT_U32
            && self.max_midi_events == MAX_MIDI_EVENTS_U32
            && self.max_events == MAX_EVENTS_U32
            && self.flags == 0
            && self.generation.load(Ordering::Acquire) != 0
            && self.next_request_sequence.load(Ordering::Acquire) != 0
            && self.reserved == 0
    }

    /// Allocates the next nonzero ticket for the current generation.
    ///
    /// Sequence exhaustion is a hard boundary: the allocator never wraps into zero,
    /// because zero is reserved as an invalid raw ticket component.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidTicket`] for a zero active generation, a
    /// corrupted zero sequence, or an allocation that would roll over to zero.
    pub fn allocate_ticket(&self) -> Result<BlockTicket, ProtocolError> {
        let generation = self.generation.load(Ordering::Acquire);
        if generation == 0 {
            return Err(ProtocolError::InvalidTicket);
        }

        let mut sequence = self.next_request_sequence.load(Ordering::Acquire);
        loop {
            if sequence == 0 {
                return Err(ProtocolError::InvalidTicket);
            }
            let Some(next_sequence) = sequence.checked_add(1) else {
                return Err(ProtocolError::InvalidTicket);
            };
            match self.next_request_sequence.compare_exchange_weak(
                sequence,
                next_sequence,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(BlockTicket {
                        generation,
                        sequence,
                    });
                }
                Err(observed) => sequence = observed,
            }
        }
    }

    /// Claims one endpoint ownership atomically for `owner_id`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid owner ID or an already claimed endpoint.
    pub fn claim_request_owner(&self, owner_id: u32) -> Result<(), ProtocolError> {
        claim_endpoint(&self.request_owner, owner_id)
    }

    /// Releases a request endpoint claim held by `owner_id`.
    ///
    /// # Errors
    ///
    /// Returns an error unless `owner_id` currently owns the endpoint.
    pub fn release_request_owner(&self, owner_id: u32) -> Result<(), ProtocolError> {
        release_endpoint(&self.request_owner, owner_id)
    }

    /// Claims one completion endpoint ownership atomically for `owner_id`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid owner ID or an already claimed endpoint.
    pub fn claim_completion_owner(&self, owner_id: u32) -> Result<(), ProtocolError> {
        claim_endpoint(&self.completion_owner, owner_id)
    }

    /// Releases a completion endpoint claim held by `owner_id`.
    ///
    /// # Errors
    ///
    /// Returns an error unless `owner_id` currently owns the endpoint.
    pub fn release_completion_owner(&self, owner_id: u32) -> Result<(), ProtocolError> {
        release_endpoint(&self.completion_owner, owner_id)
    }

    /// Publishes the latest nonzero worker heartbeat tick.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidTimestamp`] when `tick` is zero.
    pub fn publish_worker_heartbeat(&self, tick: u64) -> Result<(), ProtocolError> {
        if tick == 0 {
            return Err(ProtocolError::InvalidTimestamp);
        }
        self.worker_heartbeat_tick
            .fetch_max(tick, Ordering::Release);
        Ok(())
    }

    /// Loads the latest worker heartbeat tick, or zero before the first heartbeat.
    #[must_use]
    pub fn worker_heartbeat(&self) -> u64 {
        self.worker_heartbeat_tick.load(Ordering::Acquire)
    }

    /// Adds one measured calibrated busy-spin interval to this worker's fixed shared counters.
    ///
    /// Both durations use the header's shared monotonic clock domain. Zero durations are rejected
    /// so a completed calibrated operation cannot be confused with an unavailable measurement.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::InvalidTimestamp`] when either duration is zero.
    pub fn record_worker_busy_ticks(
        &self,
        requested_ticks: u64,
        observed_ticks: u64,
    ) -> Result<(), ProtocolError> {
        if requested_ticks == 0 || observed_ticks == 0 {
            return Err(ProtocolError::InvalidTimestamp);
        }
        self.worker_busy_requested_ticks
            .fetch_add(requested_ticks, Ordering::Release);
        self.worker_busy_observed_ticks
            .fetch_add(observed_ticks, Ordering::Release);
        self.worker_busy_operations.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Returns cumulative requested, observed, and operation-count busy-spin evidence.
    #[must_use]
    pub fn worker_busy_ticks(&self) -> (u64, u64, u64) {
        (
            self.worker_busy_requested_ticks.load(Ordering::Acquire),
            self.worker_busy_observed_ticks.load(Ordering::Acquire),
            self.worker_busy_operations.load(Ordering::Acquire),
        )
    }
}

/// Errors from a validated state transition or ownership operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    /// A slot's raw state integer is not a defined `SlotState` value.
    InvalidState,
    /// A request's bounded counts, event offsets, or event payload do not fit the fixed contract.
    InvalidRequest,
    /// A ticket has a zero generation or sequence.
    InvalidTicket,
    /// A required shared monotonic timestamp is zero or out of order.
    InvalidTimestamp,
    /// Completion metadata or its ticket is malformed after worker processing.
    MalformedCompletion,
    /// An ownership ID is zero or reserved by the protocol.
    InvalidOwner,
    /// The requested operation is not valid for the slot's current state.
    UnexpectedState,
    /// Another endpoint or transition temporarily owns the slot.
    Owned,
    /// The supplied worker does not own the processing slot.
    NotOwner,
    /// The worker ticket does not match the live request ticket.
    StaleTicket,
    /// The consumer ticket does not match the published completion ticket.
    StaleCompletion,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidState => "invalid raw slot state",
            Self::InvalidRequest => "request violates fixed protocol bounds or event offsets",
            Self::InvalidTicket => "ticket generation and sequence must be nonzero",
            Self::InvalidTimestamp => "shared monotonic timestamp is zero or out of order",
            Self::MalformedCompletion => "completion metadata or ticket is malformed",
            Self::InvalidOwner => "owner ID is zero or protocol-reserved",
            Self::UnexpectedState => "operation is invalid for the current slot state",
            Self::Owned => "slot or endpoint is already owned",
            Self::NotOwner => "operation requires the current worker owner",
            Self::StaleTicket => "worker ticket does not match the live request",
            Self::StaleCompletion => "completion ticket does not match the requested ticket",
        })
    }
}

impl std::error::Error for ProtocolError {}

fn header_size_u32() -> u32 {
    u32::try_from(std::mem::size_of::<ProtocolHeader>())
        .expect("header layout exceeds u32 byte count")
}

fn is_reserved_owner(owner: u32) -> bool {
    owner == OWNER_REQUESTING || owner == OWNER_COMPLETING || owner == OWNER_RECLAIMING
}

fn claim_endpoint(endpoint: &AtomicU32, owner_id: u32) -> Result<(), ProtocolError> {
    if owner_id == OWNER_NONE || is_reserved_owner(owner_id) {
        return Err(ProtocolError::InvalidOwner);
    }
    endpoint
        .compare_exchange(OWNER_NONE, owner_id, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| ProtocolError::Owned)
}

fn release_endpoint(endpoint: &AtomicU32, owner_id: u32) -> Result<(), ProtocolError> {
    if owner_id == OWNER_NONE || is_reserved_owner(owner_id) {
        return Err(ProtocolError::InvalidOwner);
    }
    endpoint
        .compare_exchange(owner_id, OWNER_NONE, Ordering::Release, Ordering::Relaxed)
        .map(|_| ())
        .map_err(|_| ProtocolError::NotOwner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    const REQUEST: BlockRequest = BlockRequest {
        frame_count: 128,
        input_channel_count: 2,
        output_channel_count: 2,
        midi_event_count: 1,
        event_count: 1,
        flags: 0,
        sidechain_slots: 0,
    };

    #[test]
    fn completion_poll_does_not_compete_for_an_unfinished_request() {
        let mut slot = BlockSlot::new();
        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };
        slot.publish_request(ticket, REQUEST).unwrap();
        slot.acquire_owner(OWNER_REQUESTING).unwrap();
        assert_eq!(slot.completion_snapshot(), Ok(None));
        assert_eq!(
            slot.consume_completion_timing(ticket),
            Err(ProtocolError::UnexpectedState)
        );
        assert_eq!(
            slot.metadata.owner.load(Ordering::Acquire),
            OWNER_REQUESTING
        );
        slot.release_owner(OWNER_REQUESTING);
        slot.claim_for_processing(7).unwrap();
        assert_eq!(slot.completion_snapshot(), Ok(None));
        assert_eq!(slot.metadata.owner.load(Ordering::Acquire), 7);
    }

    #[test]
    fn fixed_capacities_are_part_of_the_contract() {
        assert_eq!(PROTOCOL_VERSION, 9);
        assert_eq!(MAX_RACKS, 64);
        assert_eq!(MAX_PLUGINS_PER_RACK, 8);
        assert_eq!(MAX_CHANNELS, 2);
        assert_eq!(MAX_FRAMES, 256);
        assert_eq!(BLOCK_SLOT_COUNT, 4);
        assert_eq!(MAX_MIDI_EVENTS, 256);
        assert_eq!(MAX_EVENTS, 256);
    }

    #[test]
    fn small_callback_requests_fit_the_fixed_slot_contract() {
        for frames in [32, 64] {
            assert!(
                BlockRequest {
                    frame_count: frames,
                    ..REQUEST
                }
                .is_valid()
            );
        }
    }

    #[test]
    fn shared_layouts_are_c_aligned_and_primitive_sized() {
        assert_eq!(size_of::<MidiEvent>(), 16);
        assert_eq!(size_of::<BlockEvent>(), 20);
        assert_eq!(align_of::<BlockMetadata>(), 64);
        assert_eq!(align_of::<BlockSlot>(), 64);
        assert_eq!(align_of::<ProtocolHeader>(), 64);
        assert_eq!(size_of::<ProtocolHeader>(), 192);
        assert_eq!(offset_of!(ProtocolHeader, worker_phase), 112);
        assert_eq!(offset_of!(ProtocolHeader, worker_wait_sequence), 116);
        assert_eq!(offset_of!(ProtocolHeader, worker_loop_tick), 120);
        assert_eq!(offset_of!(ProtocolHeader, worker_latency_samples), 128);
        assert_eq!(offset_of!(ProtocolHeader, worker_restart_requested), 132);
        assert_eq!(size_of::<BlockMetadata>() % 64, 0);
        assert_eq!(size_of::<BlockSlot>() % 64, 0);
        assert_eq!(size_of::<ProtocolHeader>() % 64, 0);
        assert_eq!(offset_of!(BlockSlot, metadata), 0);
        assert_eq!(size_of::<BlockMetadata>(), 128);
        assert_eq!(offset_of!(BlockMetadata, sidechain_slots), 88);
        assert_eq!(offset_of!(BlockSlot, input_audio) % align_of::<f32>(), 0);
        assert_eq!(
            offset_of!(BlockSlot, sidechain_audio) % align_of::<f32>(),
            0
        );
        assert_eq!(
            size_of::<[[[f32; MAX_FRAMES]; MAX_CHANNELS]; MAX_PLUGINS_PER_RACK]>(),
            MAX_PLUGINS_PER_RACK * MAX_CHANNELS * MAX_FRAMES * size_of::<f32>()
        );
        assert!(offset_of!(BlockSlot, sidechain_audio) > offset_of!(BlockSlot, input_audio));
        assert!(offset_of!(BlockSlot, output_audio) > offset_of!(BlockSlot, sidechain_audio));
        assert!(offset_of!(BlockSlot, midi_events) > offset_of!(BlockSlot, output_audio));
        assert!(offset_of!(BlockSlot, events) > offset_of!(BlockSlot, midi_events));
    }

    #[test]
    fn header_reports_its_own_contract() {
        let header = ProtocolHeader::new(65_536, 7);
        assert!(header.is_compatible(65_536));
        assert!(!header.is_compatible(0));
        assert_eq!(
            header.allocate_ticket().unwrap(),
            BlockTicket {
                generation: 7,
                sequence: 1
            }
        );
        assert_eq!(
            header.allocate_ticket().unwrap(),
            BlockTicket {
                generation: 7,
                sequence: 2
            }
        );
    }

    #[test]
    fn header_rejects_zero_generations_and_sequence_rollover() {
        let zero_generation = ProtocolHeader::new(1, 0);
        assert!(!zero_generation.is_compatible(1));
        assert_eq!(
            zero_generation.allocate_ticket(),
            Err(ProtocolError::InvalidTicket)
        );

        let header = ProtocolHeader::new(1, 1);
        header.next_request_sequence.store(0, Ordering::Relaxed);
        assert_eq!(header.allocate_ticket(), Err(ProtocolError::InvalidTicket));
        header
            .next_request_sequence
            .store(u64::MAX, Ordering::Relaxed);
        assert_eq!(header.allocate_ticket(), Err(ProtocolError::InvalidTicket));
        assert_eq!(
            header.next_request_sequence.load(Ordering::Acquire),
            u64::MAX,
            "a rejected allocation must not wrap the raw sequence to zero"
        );
    }

    #[test]
    fn rejects_capacity_overflow_in_raw_request_and_topology() {
        let mut slot = BlockSlot::new();
        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };
        let oversized = BlockRequest {
            frame_count: u32::try_from(MAX_FRAMES).unwrap() + 1,
            ..REQUEST
        };
        assert_eq!(
            slot.publish_request(ticket, oversized),
            Err(ProtocolError::InvalidRequest)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Free));

        let mut rack = RackDescriptor::EMPTY;
        rack.plugin_count = u32::try_from(MAX_PLUGINS_PER_RACK).unwrap() + 1;
        assert!(!rack.is_valid());
    }

    #[test]
    fn sidechain_mask_names_only_fixed_plug_in_slots_and_survives_completion() {
        let every_slot = (1_u32 << MAX_PLUGINS_PER_RACK_U32) - 1;
        assert!(
            BlockRequest {
                sidechain_slots: every_slot,
                ..REQUEST
            }
            .is_valid()
        );
        assert!(
            !BlockRequest {
                sidechain_slots: every_slot + 1,
                ..REQUEST
            }
            .is_valid()
        );

        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };
        let request = BlockRequest {
            sidechain_slots: 0b101,
            ..REQUEST
        };
        let mut slot = BlockSlot::new();
        slot.publish_request(ticket, request).unwrap();
        slot.claim_for_processing(7).unwrap();
        slot.publish_completion(7, ticket).unwrap();
        assert_eq!(
            slot.completion_snapshot().unwrap().unwrap().request,
            request
        );
    }

    #[test]
    fn raw_publish_and_claim_reject_zero_sized_audio_requests() {
        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };
        let zero_frames = BlockRequest {
            frame_count: 0,
            ..REQUEST
        };
        let zero_outputs = BlockRequest {
            output_channel_count: 0,
            ..REQUEST
        };

        for request in [zero_frames, zero_outputs] {
            let mut slot = BlockSlot::new();
            assert_eq!(
                slot.publish_request(ticket, request),
                Err(ProtocolError::InvalidRequest)
            );
            assert_eq!(slot.metadata.state(), Ok(SlotState::Free));

            slot.metadata.frame_count = request.frame_count;
            slot.metadata.input_channel_count = request.input_channel_count;
            slot.metadata.output_channel_count = request.output_channel_count;
            slot.metadata.midi_event_count = request.midi_event_count;
            slot.metadata.event_count = request.event_count;
            slot.metadata.flags = request.flags;
            slot.metadata
                .request_generation
                .store(ticket.generation, Ordering::Relaxed);
            slot.metadata
                .request_sequence
                .store(ticket.sequence, Ordering::Relaxed);
            slot.metadata
                .state
                .store(SlotState::Requested.raw(), Ordering::Release);

            assert_eq!(
                slot.claim_for_processing(7),
                Err(ProtocolError::InvalidRequest)
            );
            assert_eq!(slot.metadata.state(), Ok(SlotState::Requested));
        }
    }

    #[test]
    fn state_transition_model_covers_successful_lifecycle() {
        let mut slot = BlockSlot::new();
        let ticket = BlockTicket {
            generation: 3,
            sequence: 9,
        };

        assert_eq!(slot.metadata.state(), Ok(SlotState::Free));
        slot.publish_request(ticket, REQUEST).unwrap();
        assert_eq!(slot.metadata.state(), Ok(SlotState::Requested));
        assert_eq!(slot.claim_for_processing(42), Ok(ticket));
        assert_eq!(slot.metadata.state(), Ok(SlotState::Processing));
        slot.publish_completion(42, ticket).unwrap();
        assert_eq!(slot.metadata.state(), Ok(SlotState::Complete));
        slot.consume_completion(ticket).unwrap();
        assert_eq!(slot.metadata.state(), Ok(SlotState::Free));
    }

    #[test]
    fn state_transition_model_rejects_invalid_transitions() {
        let mut slot = BlockSlot::new();
        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };

        assert_eq!(
            slot.claim_for_processing(1),
            Err(ProtocolError::UnexpectedState)
        );
        assert_eq!(
            slot.consume_completion(ticket),
            Err(ProtocolError::UnexpectedState)
        );
        slot.publish_request(ticket, REQUEST).unwrap();
        assert_eq!(
            slot.publish_request(ticket, REQUEST),
            Err(ProtocolError::UnexpectedState)
        );
        assert_eq!(
            slot.publish_completion(1, ticket),
            Err(ProtocolError::NotOwner)
        );
        slot.abandon_request(ticket).unwrap();
        assert_eq!(slot.metadata.state(), Ok(SlotState::Abandoned));
        assert_eq!(
            slot.claim_for_processing(1),
            Err(ProtocolError::UnexpectedState)
        );
        slot.reclaim_abandoned().unwrap();
        assert_eq!(slot.metadata.state(), Ok(SlotState::Free));
    }

    #[test]
    fn rejects_malformed_raw_slot_states_without_transitioning_them() {
        let mut slot = BlockSlot::new();
        slot.metadata.state.store(99, Ordering::Relaxed);
        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };

        assert_eq!(slot.metadata.state(), Err(ProtocolError::InvalidState));
        assert_eq!(
            slot.publish_request(ticket, REQUEST),
            Err(ProtocolError::InvalidState)
        );
        assert_eq!(
            slot.claim_for_processing(1),
            Err(ProtocolError::InvalidState)
        );
        assert_eq!(slot.reclaim_abandoned(), Err(ProtocolError::InvalidState));
        assert_eq!(slot.metadata.state.load(Ordering::Acquire), 99);
    }

    #[test]
    fn mapped_slot_validation_requires_state_appropriate_metadata() {
        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };

        let mut requested = BlockSlot::new();
        requested.publish_request_at(ticket, REQUEST, 10).unwrap();
        assert_eq!(requested.validate_mapped_contents(), Ok(()));
        requested
            .metadata
            .request_sequence
            .store(0, Ordering::Release);
        assert_eq!(
            requested.validate_mapped_contents(),
            Err(ProtocolError::InvalidTicket)
        );

        let mut processing = BlockSlot::new();
        processing.publish_request_at(ticket, REQUEST, 10).unwrap();
        processing.claim_for_processing_at(7, 11).unwrap();
        assert_eq!(processing.validate_mapped_contents(), Ok(()));
        processing
            .metadata
            .worker_claimed_tick
            .store(9, Ordering::Release);
        assert_eq!(
            processing.validate_mapped_contents(),
            Err(ProtocolError::InvalidTimestamp)
        );

        let mut completed = BlockSlot::new();
        completed.publish_request(ticket, REQUEST).unwrap();
        completed.claim_for_processing(7).unwrap();
        completed.publish_completion(7, ticket).unwrap();
        assert_eq!(completed.validate_mapped_contents(), Ok(()));
        completed
            .metadata
            .completion_sequence
            .store(2, Ordering::Release);
        assert_eq!(
            completed.validate_mapped_contents(),
            Err(ProtocolError::MalformedCompletion)
        );
    }

    #[test]
    fn rejects_event_offsets_before_publish_and_after_raw_mutation() {
        let mut slot = BlockSlot::new();
        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };
        slot.midi_events[0] = MidiEvent {
            frame_offset: REQUEST.frame_count,
            port: 0,
            data_length: 3,
            data: [0x90, 60, 100],
            flags: 0,
        };
        assert_eq!(
            slot.publish_request(ticket, REQUEST),
            Err(ProtocolError::InvalidRequest)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Free));

        slot.midi_events[0].frame_offset = REQUEST.frame_count - 1;
        slot.publish_request(ticket, REQUEST).unwrap();
        slot.events[0] = BlockEvent {
            frame_offset: REQUEST.frame_count,
            event_type: 1,
            key: 2,
            value: 0.5,
            flags: 0,
        };
        assert_eq!(
            slot.claim_for_processing(7),
            Err(ProtocolError::InvalidRequest)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Requested));
    }

    #[test]
    fn rejects_nonfinite_events_in_a_raw_completion_snapshot() {
        let mut slot = BlockSlot::new();
        let ticket = BlockTicket {
            generation: 1,
            sequence: 1,
        };
        slot.publish_request(ticket, REQUEST).unwrap();
        slot.claim_for_processing(7).unwrap();
        slot.events[0].value = f32::NAN;
        slot.publish_completion(7, ticket).unwrap();

        assert_eq!(
            slot.completion_snapshot(),
            Err(ProtocolError::MalformedCompletion)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Complete));
    }

    #[test]
    fn all_slot_operations_reject_zero_ticket_components() {
        let mut slot = BlockSlot::new();
        let zero = BlockTicket {
            generation: 0,
            sequence: 1,
        };

        assert_eq!(
            slot.publish_request(zero, REQUEST),
            Err(ProtocolError::InvalidTicket)
        );
        assert_eq!(
            slot.consume_completion(zero),
            Err(ProtocolError::InvalidTicket)
        );
        assert_eq!(
            slot.abandon_request(zero),
            Err(ProtocolError::InvalidTicket)
        );
    }

    #[test]
    fn stale_completion_cannot_release_a_reused_slot() {
        let mut slot = BlockSlot::new();
        let old_ticket = BlockTicket {
            generation: 5,
            sequence: 1,
        };
        let new_ticket = BlockTicket {
            generation: 6,
            sequence: 2,
        };

        slot.publish_request(old_ticket, REQUEST).unwrap();
        slot.abandon_request(old_ticket).unwrap();
        slot.reclaim_abandoned().unwrap();
        slot.publish_request(new_ticket, REQUEST).unwrap();
        let claimed = slot.claim_for_processing(77).unwrap();
        assert_eq!(claimed, new_ticket);
        assert_eq!(
            slot.publish_completion(77, old_ticket),
            Err(ProtocolError::StaleTicket)
        );
        slot.publish_completion(77, new_ticket).unwrap();
        assert_eq!(
            slot.consume_completion(old_ticket),
            Err(ProtocolError::StaleCompletion)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Complete));
        slot.consume_completion(new_ticket).unwrap();
    }

    #[test]
    fn conditional_timed_claim_never_claims_a_different_ticket() {
        let mut slot = BlockSlot::new();
        let live_ticket = BlockTicket {
            generation: 4,
            sequence: 2,
        };
        let observed_ticket = BlockTicket {
            generation: 4,
            sequence: 1,
        };

        slot.publish_request_at(live_ticket, REQUEST, 10).unwrap();
        assert_eq!(
            slot.claim_ticket_for_processing_at(7, observed_ticket, 15),
            Err(ProtocolError::StaleTicket)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Requested));
        assert_eq!(
            slot.claim_ticket_for_processing_at(7, live_ticket, 16),
            Ok(live_ticket)
        );
    }

    #[test]
    fn timed_lifecycle_returns_ordered_cross_process_timing() {
        let mut slot = BlockSlot::new();
        let ticket = BlockTicket {
            generation: 4,
            sequence: 2,
        };

        slot.publish_request_at(ticket, REQUEST, 10).unwrap();
        assert_eq!(slot.claim_for_processing_at(7, 15), Ok(ticket));
        slot.publish_completion_at(7, ticket, 21).unwrap();
        let snapshot = slot.completion_snapshot().unwrap().unwrap();
        assert_eq!(snapshot.ticket, ticket);
        assert_eq!(snapshot.request, REQUEST);
        assert_eq!(
            snapshot.timing,
            BlockTiming {
                request_published_tick: 10,
                worker_claimed_tick: 15,
                completion_published_tick: 21,
            }
        );
        assert_eq!(slot.consume_completion_timing(ticket), Ok(snapshot.timing));
    }

    #[test]
    fn timed_lifecycle_rejects_zero_and_out_of_order_ticks() {
        let mut slot = BlockSlot::new();
        let ticket = BlockTicket {
            generation: 4,
            sequence: 3,
        };

        assert_eq!(
            slot.publish_request_at(ticket, REQUEST, 0),
            Err(ProtocolError::InvalidTimestamp)
        );
        slot.publish_request_at(ticket, REQUEST, 10).unwrap();
        assert_eq!(
            slot.claim_for_processing_at(7, 0),
            Err(ProtocolError::InvalidTimestamp)
        );
        assert_eq!(
            slot.claim_for_processing_at(7, 9),
            Err(ProtocolError::InvalidTimestamp)
        );
        assert_eq!(
            slot.claim_for_processing(7),
            Err(ProtocolError::InvalidTimestamp)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Requested));
        slot.claim_for_processing_at(7, 15).unwrap();
        assert_eq!(
            slot.publish_completion_at(7, ticket, 14),
            Err(ProtocolError::InvalidTimestamp)
        );
        slot.publish_completion_at(7, ticket, 20).unwrap();
    }

    #[test]
    fn untimed_lifecycle_rejects_mixed_timestamp_operations() {
        let mut slot = BlockSlot::new();
        let ticket = BlockTicket {
            generation: 4,
            sequence: 5,
        };

        slot.publish_request(ticket, REQUEST).unwrap();
        assert_eq!(
            slot.claim_for_processing_at(7, 10),
            Err(ProtocolError::InvalidTimestamp)
        );
        assert_eq!(slot.claim_for_processing(7), Ok(ticket));
        assert_eq!(
            slot.publish_completion_at(7, ticket, 20),
            Err(ProtocolError::InvalidTimestamp)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Processing));
        slot.publish_completion(7, ticket).unwrap();
        assert_eq!(
            slot.consume_completion_timing(ticket),
            Ok(BlockTiming::default())
        );
    }

    #[test]
    fn malformed_completion_remains_published_for_diagnosis() {
        let mut slot = BlockSlot::new();
        let ticket = BlockTicket {
            generation: 4,
            sequence: 4,
        };

        slot.publish_request_at(ticket, REQUEST, 10).unwrap();
        slot.claim_for_processing_at(7, 15).unwrap();
        slot.test_publish_malformed_completion_at(7, ticket, 20)
            .unwrap();

        assert_eq!(
            slot.consume_completion_timing(ticket),
            Err(ProtocolError::MalformedCompletion)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Complete));
    }

    #[test]
    fn stale_completion_remains_observable_but_cannot_release_the_live_ticket() {
        let mut slot = BlockSlot::new();
        let live_ticket = BlockTicket {
            generation: 4,
            sequence: 6,
        };
        let stale_ticket = BlockTicket {
            generation: 4,
            sequence: 5,
        };

        slot.publish_request_at(live_ticket, REQUEST, 10).unwrap();
        slot.claim_for_processing_at(7, 15).unwrap();
        slot.test_publish_stale_completion_at(7, live_ticket, stale_ticket, 20)
            .unwrap();

        assert_eq!(
            slot.completion_snapshot().unwrap().unwrap().ticket,
            stale_ticket
        );
        assert_eq!(
            slot.consume_completion_timing(live_ticket),
            Err(ProtocolError::StaleCompletion)
        );
        assert_eq!(slot.metadata.state(), Ok(SlotState::Complete));
    }

    #[test]
    fn worker_heartbeat_is_published_monotonically_by_policy() {
        let header = ProtocolHeader::new(1, 1);
        assert_eq!(header.worker_heartbeat(), 0);
        assert_eq!(
            header.publish_worker_heartbeat(0),
            Err(ProtocolError::InvalidTimestamp)
        );
        header.publish_worker_heartbeat(42).unwrap();
        header.publish_worker_heartbeat(7).unwrap();
        assert_eq!(header.worker_heartbeat(), 42);
    }

    #[test]
    fn worker_busy_ticks_retain_requested_and_observed_calibration() {
        let header = ProtocolHeader::new(1, 1);
        assert_eq!(header.worker_busy_ticks(), (0, 0, 0));
        assert_eq!(
            header.record_worker_busy_ticks(0, 1),
            Err(ProtocolError::InvalidTimestamp)
        );
        assert_eq!(
            header.record_worker_busy_ticks(1, 0),
            Err(ProtocolError::InvalidTimestamp)
        );
        header.record_worker_busy_ticks(10, 11).unwrap();
        header.record_worker_busy_ticks(20, 22).unwrap();
        assert_eq!(header.worker_busy_ticks(), (30, 33, 2));
    }

    #[test]
    fn endpoint_claims_are_exclusive() {
        let header = ProtocolHeader::new(1, 1);
        assert_eq!(
            header.release_request_owner(OWNER_NONE),
            Err(ProtocolError::InvalidOwner)
        );
        header.claim_request_owner(1).unwrap();
        assert_eq!(header.claim_request_owner(2), Err(ProtocolError::Owned));
        assert_eq!(
            header.release_request_owner(2),
            Err(ProtocolError::NotOwner)
        );
        header.release_request_owner(1).unwrap();
        header.claim_completion_owner(2).unwrap();
        header.release_completion_owner(2).unwrap();
    }
}
