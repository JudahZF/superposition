#![deny(unsafe_code)]
//! In-process, aligned dual-bank storage for the shared-memory contract.
//!
//! This crate owns `SharedBank` values in aligned `Box` allocations. The allocations
//! provide stable bank addresses and exercise the exact protocol layout without OS
//! mapping. The macOS mapping lives in `sp-shared-memory-macos`, preserving
//! this crate's platform-neutral, safe state-transition contract. The one unsafe module
//! initializes the enlarged fixed layout directly on the heap to avoid stack overflow.

use std::fmt;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};

mod bank_init;
mod parameter_feedback;
pub use parameter_feedback::{
    PARAMETER_FEEDBACK_CAPACITY_PER_SLOT, ParameterFeedbackBank, ParameterFeedbackError,
    ParameterFeedbackSnapshot, RestartCursor, RestartSnapshot,
};

pub use sp_protocol::{
    BLOCK_EVENT_PARAMETER, BLOCK_EVENT_SLOT_BYPASS, BLOCK_SLOT_COUNT, BlockEvent, BlockMetadata,
    BlockRequest, BlockSlot, BlockTicket, BlockTiming, CompletionSnapshot, MAX_CHANNELS,
    MAX_EVENTS, MAX_FRAMES, MAX_MIDI_EVENTS, MAX_PLUGINS_PER_RACK, MAX_RACKS, MidiEvent,
    PLUGIN_IDENTIFIER_BYTES, PROTOCOL_MAGIC, PROTOCOL_VERSION, PluginDescriptor, ProtocolError,
    ProtocolHeader, RackDescriptor, SlotState,
};

/// Number of stable banks maintained by `DualBankStorage`.
pub const BANK_COUNT: usize = 2;
const BANK_COUNT_U32: u32 = 2;

/// Callback-visible identity for one member of a stable dual-bank pair.
///
/// The bank index selects an allocation that never moves for the lifetime of its owner.
/// Its generation distinguishes a replacement worker from every previous worker that used
/// the same stable address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BankMetadata {
    /// Fixed index in the two-bank pair.
    pub index: usize,
    /// Nonzero worker generation stored in the bank header.
    pub generation: u64,
}

/// Active and inactive metadata for a stable pair of shared-memory banks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DualBankMetadata {
    /// The bank selected by the callback.
    pub active: BankMetadata,
    /// The stable bank reserved for a replacement worker.
    pub inactive: BankMetadata,
}

/// Complete fixed-layout contents of one independently usable shared bank.
///
/// This is the only structure intended to become an OS-mapped region. It
/// contains no Rust references, heap containers, strings, or Rust enum fields.
#[repr(C, align(64))]
pub struct SharedBank {
    /// Version and endpoint ownership control plane.
    pub header: ProtocolHeader,
    /// Fixed Alpha rack and plugin topology.
    pub racks: [RackDescriptor; MAX_RACKS],
    /// Fixed-capacity block data and state machine storage.
    pub slots: [BlockSlot; BLOCK_SLOT_COUNT],
    /// Worker-to-host latest native-editor parameter values, independent of audio slots.
    pub feedback: ParameterFeedbackBank,
}

impl SharedBank {
    /// Creates a compatible, zero-payload bank with the specified nonzero generation.
    ///
    /// # Errors
    ///
    /// Returns [`SharedMemoryError::Protocol`] when `generation` is zero, because a
    /// zero generation can never identify a request or a valid mapped bank.
    pub fn new(generation: u64) -> Result<Box<Self>, SharedMemoryError> {
        bank_init::new_boxed(generation)
    }

    /// Returns whether this bank's header, topology, and fixed layout match this build.
    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.header.is_compatible(bank_size_u32())
            && self.racks.iter().copied().all(RackDescriptor::is_valid)
    }

    /// Returns the nonzero worker generation currently stored in this bank header.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.header.generation.load(Ordering::Acquire)
    }

    /// Returns whether every slot is free and no request can still be owned by a worker.
    ///
    /// An abandoned, requested, processing, or complete slot is deliberately not quiescent.
    /// A replacement must never overwrite or reuse any of those states while its worker may
    /// still map this bank.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.slots
            .iter()
            .all(|slot| matches!(slot.metadata.state(), Ok(SlotState::Free)))
    }

    /// Reinitializes this already allocated bank for a replacement generation.
    ///
    /// The containing allocation remains at the same address. Callers must first ensure the
    /// old worker has exited and every slot is free; an abandoned or in-flight slot is never
    /// silently recycled.
    ///
    /// # Errors
    ///
    /// Returns [`SharedMemoryError::BankBusy`] if any old slot is not free, or a protocol
    /// error when `generation` is zero.
    pub fn reset_for_replacement(&mut self, generation: u64) -> Result<(), SharedMemoryError> {
        if !self.is_quiescent() {
            return Err(SharedMemoryError::BankBusy);
        }
        if generation == 0 {
            return Err(ProtocolError::InvalidTicket.into());
        }
        self.header = ProtocolHeader::new(bank_size_u32(), generation);
        self.racks.fill(RackDescriptor::EMPTY);
        for slot in &mut self.slots {
            *slot = BlockSlot::new();
        }
        self.feedback.clear_for_replacement();
        Ok(())
    }

    /// Validates mapped slot state and active request payloads without changing ownership.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when a slot has an unknown state or malformed active request.
    pub fn validate_mapped_contents(&self) -> Result<(), ProtocolError> {
        self.slots
            .iter()
            .try_for_each(BlockSlot::validate_mapped_contents)
    }

    /// Allocates and publishes a request into the selected free slot.
    ///
    /// Input audio and bounded event payloads must be written into the selected slot
    /// before its request is published. Callers can do that through `slots` while
    /// holding ordinary exclusive access to this owned bank.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range slot or a rejected protocol transition.
    pub fn request_block(
        &mut self,
        slot_index: usize,
        request: BlockRequest,
    ) -> Result<BlockTicket, SharedMemoryError> {
        let ticket = self.header.allocate_ticket()?;
        let slot = self
            .slots
            .get_mut(slot_index)
            .ok_or(SharedMemoryError::InvalidSlotIndex)?;
        slot.publish_request(ticket, request)?;
        Ok(ticket)
    }

    /// Allocates and publishes a timed request into the selected free slot.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range slot, zero timestamp, or rejected protocol
    /// transition.
    pub fn request_block_at(
        &mut self,
        slot_index: usize,
        request: BlockRequest,
        published_tick: u64,
    ) -> Result<BlockTicket, SharedMemoryError> {
        let slot = self
            .slots
            .get_mut(slot_index)
            .ok_or(SharedMemoryError::InvalidSlotIndex)?;
        if published_tick == 0 {
            return Err(ProtocolError::InvalidTimestamp.into());
        }
        let ticket = self.header.allocate_ticket()?;
        slot.publish_request_at(ticket, request, published_tick)?;
        Ok(ticket)
    }

    /// Returns one slot by its fixed index.
    #[must_use]
    pub fn slot(&self, slot_index: usize) -> Option<&BlockSlot> {
        self.slots.get(slot_index)
    }

    /// Returns one slot with exclusive mutable access.
    #[must_use]
    pub fn slot_mut(&mut self, slot_index: usize) -> Option<&mut BlockSlot> {
        self.slots.get_mut(slot_index)
    }
}

/// Stable owner of two independently addressable in-process banks.
///
/// Each bank lives in its own `Box`, so moving `DualBankStorage` itself cannot move a
/// bank allocation. The active index is only a selection signal; it never copies or
/// swaps the bank bytes.
#[repr(align(64))]
pub struct DualBankStorage {
    banks: [Box<SharedBank>; BANK_COUNT],
    active_bank: AtomicU32,
}

impl DualBankStorage {
    /// Creates two stable, compatible bank allocations.
    ///
    /// # Errors
    ///
    /// Returns [`SharedMemoryError::Protocol`] when `initial_generation` is zero.
    pub fn new(initial_generation: u64) -> Result<Self, SharedMemoryError> {
        let inactive_generation = initial_generation
            .checked_add(1)
            .ok_or(SharedMemoryError::GenerationExhausted)?;
        Ok(Self {
            banks: [
                SharedBank::new(initial_generation)?,
                SharedBank::new(inactive_generation)?,
            ],
            active_bank: AtomicU32::new(0),
        })
    }

    /// Returns an immutable bank reference by fixed index.
    #[must_use]
    pub fn bank(&self, bank_index: usize) -> Option<&SharedBank> {
        self.banks.get(bank_index).map(Box::as_ref)
    }

    /// Returns a mutable bank reference by fixed index for setup and tests.
    #[must_use]
    pub fn bank_mut(&mut self, bank_index: usize) -> Option<&mut SharedBank> {
        self.banks.get_mut(bank_index).map(Box::as_mut)
    }

    /// Loads the currently selected stable bank index.
    #[must_use]
    pub fn active_index(&self) -> usize {
        self.active_bank.load(Ordering::Acquire) as usize
    }

    /// Returns the currently selected bank.
    #[must_use]
    pub fn active_bank(&self) -> &SharedBank {
        // `active_bank` is written only by `select_active`; that method accepts the
        // two in-range indices, so this indexing operation is an internal invariant.
        &self.banks[self.active_index()]
    }

    /// Returns a snapshot of both stable bank identities.
    #[must_use]
    pub fn metadata(&self) -> DualBankMetadata {
        let active_index = self.active_index();
        let inactive_index = inactive_index(active_index);
        DualBankMetadata {
            active: BankMetadata {
                index: active_index,
                generation: self.banks[active_index].generation(),
            },
            inactive: BankMetadata {
                index: inactive_index,
                generation: self.banks[inactive_index].generation(),
            },
        }
    }

    /// Resets the inactive allocation for a replacement worker without moving it.
    ///
    /// The inactive bank must already be fully quiescent. In particular, an abandoned,
    /// processing, or completed slot is a hard error rather than a candidate for reuse.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero or active-matching generation, or when an inactive
    /// slot has not been safely retired.
    pub fn prepare_inactive(&mut self, generation: u64) -> Result<BankMetadata, SharedMemoryError> {
        let active_index = self.active_index();
        let inactive_index = inactive_index(active_index);
        if generation == 0 {
            return Err(ProtocolError::InvalidTicket.into());
        }
        if generation == self.banks[active_index].generation() {
            return Err(SharedMemoryError::DuplicateGeneration);
        }
        self.banks[inactive_index].reset_for_replacement(generation)?;
        Ok(BankMetadata {
            index: inactive_index,
            generation,
        })
    }

    /// Activates the prepared inactive allocation without moving either bank.
    ///
    /// The previous active bank remains allocated and is intentionally not recycled by this
    /// operation. A control-plane reaper must establish that its worker exited before the
    /// previous address can be prepared for another generation.
    ///
    /// # Errors
    ///
    /// Returns an error if the candidate is malformed, non-quiescent, or reuses the active
    /// generation.
    pub fn activate_prepared(&self) -> Result<BankMetadata, SharedMemoryError> {
        let active_index = self.active_index();
        let inactive_index = inactive_index(active_index);
        let candidate = &self.banks[inactive_index];
        if !candidate.is_compatible() {
            return Err(SharedMemoryError::Protocol(ProtocolError::InvalidRequest));
        }
        if !candidate.is_quiescent() {
            return Err(SharedMemoryError::BankBusy);
        }
        if candidate.generation() == self.banks[active_index].generation() {
            return Err(SharedMemoryError::DuplicateGeneration);
        }
        let inactive_index_u32 =
            u32::try_from(inactive_index).map_err(|_| SharedMemoryError::InvalidBankIndex)?;
        self.active_bank
            .store(inactive_index_u32, Ordering::Release);
        Ok(BankMetadata {
            index: inactive_index,
            generation: candidate.generation(),
        })
    }

    /// Selects a prepared, quiescent bank without moving or copying either allocation.
    ///
    /// # Errors
    ///
    /// Returns [`SharedMemoryError::InvalidBankIndex`] for an index outside the two stable
    /// allocations, or rejects a malformed, in-flight, abandoned, or duplicate-generation
    /// candidate rather than making it callback-visible.
    pub fn select_active(&self, bank_index: usize) -> Result<(), SharedMemoryError> {
        let bank_index_u32 =
            u32::try_from(bank_index).map_err(|_| SharedMemoryError::InvalidBankIndex)?;
        if bank_index_u32 >= BANK_COUNT_U32 {
            return Err(SharedMemoryError::InvalidBankIndex);
        }
        let current_index = self.active_index();
        let candidate = &self.banks[bank_index];
        if !candidate.is_compatible() {
            return Err(SharedMemoryError::Protocol(ProtocolError::InvalidRequest));
        }
        if !candidate.is_quiescent() {
            return Err(SharedMemoryError::BankBusy);
        }
        if bank_index != current_index
            && candidate.generation() == self.banks[current_index].generation()
        {
            return Err(SharedMemoryError::DuplicateGeneration);
        }
        self.active_bank.store(bank_index_u32, Ordering::Release);
        Ok(())
    }
}

impl Default for DualBankStorage {
    fn default() -> Self {
        Self::new(1).expect("the fixed default dual-bank generation is nonzero")
    }
}

/// Errors returned by owned bank selection and request operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedMemoryError {
    /// The requested slot does not exist in the four-slot fixed bank.
    InvalidSlotIndex,
    /// The requested bank does not exist in the dual-bank owner.
    InvalidBankIndex,
    /// An abandoned, requested, processing, or complete slot prevents safe replacement.
    BankBusy,
    /// A candidate worker generation collides with the active stable bank.
    DuplicateGeneration,
    /// The initial generation cannot reserve a distinct nonzero inactive generation.
    GenerationExhausted,
    /// A validated protocol state operation failed.
    Protocol(ProtocolError),
}

impl From<ProtocolError> for SharedMemoryError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

impl fmt::Display for SharedMemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSlotIndex => formatter.write_str("slot index is outside the fixed bank"),
            Self::InvalidBankIndex => {
                formatter.write_str("bank index is outside dual-bank storage")
            }
            Self::BankBusy => formatter.write_str("bank contains an abandoned or in-flight slot"),
            Self::DuplicateGeneration => {
                formatter.write_str("replacement bank generation matches the active bank")
            }
            Self::GenerationExhausted => {
                formatter.write_str("initial generation cannot reserve an inactive bank")
            }
            Self::Protocol(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SharedMemoryError {}

fn inactive_index(active_index: usize) -> usize {
    debug_assert!(active_index < BANK_COUNT);
    active_index ^ 1
}

fn bank_size_u32() -> u32 {
    u32::try_from(size_of::<SharedBank>()).expect("bank layout exceeds u32 byte count")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn bank_layout_is_aligned_and_header_first() {
        assert_eq!(align_of::<SharedBank>(), 64);
        assert_eq!(align_of::<DualBankStorage>(), 64);
        assert_eq!(size_of::<SharedBank>() % 64, 0);
        assert_eq!(offset_of!(SharedBank, header), 0);
        assert!(offset_of!(SharedBank, slots) > offset_of!(SharedBank, racks));

        let bank = SharedBank::new(1).unwrap();
        assert!(bank.is_compatible());
        assert_eq!(bank.header.bank_bytes, bank_size_u32());
    }

    #[test]
    fn old_layout_version_is_rejected() {
        let mut bank = SharedBank::new(1).expect("bank");
        bank.header.version = PROTOCOL_VERSION - 1;
        assert!(!bank.is_compatible());
    }

    #[test]
    fn rejects_zero_generation_before_a_bank_can_be_activated() {
        assert!(matches!(
            SharedBank::new(0),
            Err(SharedMemoryError::Protocol(ProtocolError::InvalidTicket))
        ));
        assert!(matches!(
            DualBankStorage::new(0),
            Err(SharedMemoryError::Protocol(ProtocolError::InvalidTicket))
        ));
    }

    #[test]
    fn dual_banks_have_stable_distinct_aligned_addresses() {
        let storage = DualBankStorage::new(4).unwrap();
        let first = std::ptr::from_ref(storage.bank(0).unwrap());
        let second = std::ptr::from_ref(storage.bank(1).unwrap());

        assert_ne!(first, second);
        assert_eq!(first.addr() % align_of::<SharedBank>(), 0);
        assert_eq!(second.addr() % align_of::<SharedBank>(), 0);
        storage.select_active(1).unwrap();
        assert_eq!(storage.active_index(), 1);
        assert_eq!(std::ptr::from_ref(storage.active_bank()), second);
        storage.select_active(0).unwrap();
        assert_eq!(std::ptr::from_ref(storage.active_bank()), first);
    }

    #[test]
    fn full_capacity_banks_initialize_on_two_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let storage = DualBankStorage::new(11).expect("dual banks");
                assert!(storage.bank(0).expect("first bank").is_compatible());
                assert!(storage.bank(1).expect("second bank").is_compatible());
            })
            .expect("small-stack thread")
            .join()
            .expect("bank initialization");
    }

    #[test]
    fn rejects_raw_topology_outside_alpha_main_bus_layouts() {
        let mut bank = SharedBank::new(1).unwrap();
        bank.racks[0].plugin_count = 1;
        bank.racks[0].plugins[0] = PluginDescriptor {
            identifier: [b'p'; PLUGIN_IDENTIFIER_BYTES],
            enabled: 1,
            input_channel_count: 3,
            output_channel_count: 2,
            reserved: 0,
        };

        assert!(!bank.is_compatible());
    }

    #[test]
    fn request_uses_fixed_slot_and_protocol_ticket() {
        let mut bank = SharedBank::new(9).unwrap();
        let ticket = bank
            .request_block(
                3,
                BlockRequest {
                    frame_count: 256,
                    input_channel_count: 2,
                    output_channel_count: 2,
                    midi_event_count: 256,
                    event_count: 256,
                    flags: 0,
                    sidechain_slots: 0,
                },
            )
            .unwrap();

        assert_eq!(ticket.generation, 9);
        assert_eq!(ticket.sequence, 1);
        assert_eq!(
            bank.slot(3).unwrap().metadata.state(),
            Ok(SlotState::Requested)
        );
        assert_eq!(
            bank.request_block(4, BlockRequest::default()),
            Err(SharedMemoryError::InvalidSlotIndex)
        );
    }

    #[test]
    fn small_blocks_publish_and_complete_in_fixed_slots() {
        for frames in [32, 64] {
            let mut bank = SharedBank::new(9).unwrap();
            let request = BlockRequest {
                frame_count: frames,
                input_channel_count: 2,
                output_channel_count: 2,
                midi_event_count: 0,
                event_count: 0,
                flags: 0,
                sidechain_slots: 0,
            };
            let ticket = bank.request_block_at(0, request, 10).unwrap();
            let slot = bank.slot_mut(0).unwrap();
            assert_eq!(slot.metadata.state(), Ok(SlotState::Requested));
            assert_eq!(slot.metadata.frame_count, frames);
            assert_eq!(slot.claim_for_processing_at(7, 11).unwrap(), ticket);
            slot.publish_completion_at(7, ticket, 12).unwrap();
            let completion = slot.completion_snapshot().unwrap().unwrap();
            assert_eq!(completion.request.frame_count, frames);
            assert_eq!(completion.ticket, ticket);
            slot.consume_completion(ticket).unwrap();
            assert_eq!(slot.metadata.state(), Ok(SlotState::Free));
        }
    }

    #[test]
    fn timed_request_preserves_its_publication_tick() {
        let mut bank = SharedBank::new(9).unwrap();
        let request = BlockRequest {
            frame_count: 256,
            input_channel_count: 2,
            output_channel_count: 2,
            midi_event_count: 256,
            event_count: 256,
            flags: 0,
            sidechain_slots: 0,
        };

        assert_eq!(
            bank.request_block_at(1, request, 0),
            Err(SharedMemoryError::Protocol(ProtocolError::InvalidTimestamp))
        );
        assert_eq!(bank.slot(1).unwrap().metadata.state(), Ok(SlotState::Free));

        let ticket = bank.request_block_at(2, request, 123).unwrap();
        let slot = bank.slot(2).unwrap();
        assert_eq!(ticket.generation, 9);
        assert_eq!(ticket.sequence, 1);
        assert_eq!(slot.metadata.state(), Ok(SlotState::Requested));
        assert_eq!(
            slot.metadata.request_published_tick.load(Ordering::Acquire),
            123
        );
    }

    #[test]
    fn replacement_keeps_both_addresses_stable_and_switches_generation() {
        let mut storage = DualBankStorage::new(7).unwrap();
        let first = std::ptr::from_ref(storage.bank(0).unwrap());
        let second = std::ptr::from_ref(storage.bank(1).unwrap());
        assert_eq!(
            storage.metadata(),
            DualBankMetadata {
                active: BankMetadata {
                    index: 0,
                    generation: 7,
                },
                inactive: BankMetadata {
                    index: 1,
                    generation: 8,
                },
            }
        );

        assert_eq!(
            storage.prepare_inactive(11).unwrap(),
            BankMetadata {
                index: 1,
                generation: 11,
            }
        );
        assert_eq!(std::ptr::from_ref(storage.bank(0).unwrap()), first);
        assert_eq!(std::ptr::from_ref(storage.bank(1).unwrap()), second);
        assert_eq!(
            storage.activate_prepared().unwrap(),
            BankMetadata {
                index: 1,
                generation: 11,
            }
        );
        assert_eq!(storage.metadata().active.generation, 11);
        assert_eq!(storage.metadata().inactive.generation, 7);
    }

    #[test]
    fn abandoned_inactive_slots_are_not_recycled_or_activated() {
        let mut storage = DualBankStorage::new(7).unwrap();
        let request = BlockRequest {
            frame_count: 128,
            input_channel_count: 2,
            output_channel_count: 2,
            midi_event_count: 0,
            event_count: 0,
            flags: 0,
            sidechain_slots: 0,
        };
        let ticket = storage
            .bank_mut(1)
            .unwrap()
            .request_block(0, request)
            .unwrap();
        storage
            .bank_mut(1)
            .unwrap()
            .slot(0)
            .unwrap()
            .abandon_request(ticket)
            .unwrap();

        assert_eq!(
            storage.prepare_inactive(9),
            Err(SharedMemoryError::BankBusy)
        );
        assert_eq!(storage.select_active(1), Err(SharedMemoryError::BankBusy));
    }

    #[test]
    fn initial_generation_must_reserve_a_distinct_inactive_generation() {
        assert!(matches!(
            DualBankStorage::new(u64::MAX),
            Err(SharedMemoryError::GenerationExhausted)
        ));
    }

    #[test]
    fn selecting_outside_the_dual_bank_is_rejected() {
        let storage = DualBankStorage::default();
        assert_eq!(
            storage.select_active(BANK_COUNT),
            Err(SharedMemoryError::InvalidBankIndex)
        );
    }
}
