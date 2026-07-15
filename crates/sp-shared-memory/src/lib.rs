#![forbid(unsafe_code)]
//! In-process, aligned dual-bank storage for the Phase 0 shared-memory contract.
//!
//! Phase 0 owns `SharedBank` values in aligned `Box` allocations. The allocations
//! provide stable bank addresses and exercise the exact protocol layout without OS
//! mapping. The macOS Phase 1 mapping lives in `sp-shared-memory-macos`, preserving
//! this crate's platform-neutral, safe fixed-layout and state-transition contract.

use std::array;
use std::fmt;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};

pub use sp_protocol::{
    BLOCK_SLOT_COUNT, BlockEvent, BlockMetadata, BlockRequest, BlockSlot, BlockTicket, BlockTiming,
    CompletionSnapshot, MAX_CHANNELS, MAX_EVENTS, MAX_FRAMES, MAX_MIDI_EVENTS,
    MAX_PLUGINS_PER_RACK, MAX_RACKS, MidiEvent, PLUGIN_IDENTIFIER_BYTES, PROTOCOL_MAGIC,
    PROTOCOL_VERSION, PluginDescriptor, ProtocolError, ProtocolHeader, RackDescriptor, SlotState,
};

/// Number of stable banks maintained by `DualBankStorage`.
pub const BANK_COUNT: usize = 2;
const BANK_COUNT_U32: u32 = 2;

/// Complete fixed-layout contents of one independently usable shared bank.
///
/// This is the only structure intended to become an OS-mapped region in Phase 1. It
/// contains no Rust references, heap containers, strings, or Rust enum fields.
#[repr(C, align(64))]
pub struct SharedBank {
    /// Version and endpoint ownership control plane.
    pub header: ProtocolHeader,
    /// Fixed Alpha rack and plugin topology.
    pub racks: [RackDescriptor; MAX_RACKS],
    /// Fixed-capacity block data and state machine storage.
    pub slots: [BlockSlot; BLOCK_SLOT_COUNT],
}

impl SharedBank {
    /// Creates a compatible, zero-payload bank with the specified nonzero generation.
    #[must_use]
    pub fn new(generation: u64) -> Self {
        let bank_bytes = bank_size_u32();
        Self {
            header: ProtocolHeader::new(bank_bytes, generation),
            racks: [RackDescriptor::EMPTY; MAX_RACKS],
            slots: array::from_fn(|_| BlockSlot::new()),
        }
    }

    /// Returns whether this bank's header and fixed layout match this build.
    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.header.is_compatible(bank_size_u32())
    }

    /// Allocates and publishes a request into the selected free slot.
    ///
    /// Input audio and bounded event payloads must be written into the selected slot
    /// before its request is published. Phase 0 users can do that through `slots` while
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
        let ticket = self.header.allocate_ticket();
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
        let ticket = self.header.allocate_ticket();
        slot.publish_request_at(ticket, request, published_tick)?;
        Ok(ticket)
    }

    /// Returns one slot by its fixed index.
    #[must_use]
    pub fn slot(&self, slot_index: usize) -> Option<&BlockSlot> {
        self.slots.get(slot_index)
    }

    /// Returns one slot with Phase 0 exclusive mutable access.
    #[must_use]
    pub fn slot_mut(&mut self, slot_index: usize) -> Option<&mut BlockSlot> {
        self.slots.get_mut(slot_index)
    }
}

impl Default for SharedBank {
    fn default() -> Self {
        Self::new(1)
    }
}

/// Stable owner of two independently addressable Phase 0 banks.
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
    #[must_use]
    pub fn new(initial_generation: u64) -> Self {
        Self {
            banks: array::from_fn(|_| Box::new(SharedBank::new(initial_generation))),
            active_bank: AtomicU32::new(0),
        }
    }

    /// Returns an immutable bank reference by fixed index.
    #[must_use]
    pub fn bank(&self, bank_index: usize) -> Option<&SharedBank> {
        self.banks.get(bank_index).map(Box::as_ref)
    }

    /// Returns a mutable bank reference by fixed index for Phase 0 setup and tests.
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

    /// Selects one existing bank without moving or copying either allocation.
    ///
    /// # Errors
    ///
    /// Returns [`SharedMemoryError::InvalidBankIndex`] for an index outside the two
    /// stable allocations.
    pub fn select_active(&self, bank_index: usize) -> Result<(), SharedMemoryError> {
        let bank_index =
            u32::try_from(bank_index).map_err(|_| SharedMemoryError::InvalidBankIndex)?;
        if bank_index >= BANK_COUNT_U32 {
            return Err(SharedMemoryError::InvalidBankIndex);
        }
        self.active_bank.store(bank_index, Ordering::Release);
        Ok(())
    }
}

impl Default for DualBankStorage {
    fn default() -> Self {
        Self::new(1)
    }
}

/// Errors returned by owned Phase 0 bank selection and request operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedMemoryError {
    /// The requested slot does not exist in the four-slot fixed bank.
    InvalidSlotIndex,
    /// The requested bank does not exist in the dual-bank owner.
    InvalidBankIndex,
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
            Self::Protocol(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SharedMemoryError {}

fn bank_size_u32() -> u32 {
    u32::try_from(size_of::<SharedBank>()).expect("Phase 0 bank layout exceeds u32 byte count")
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

        let bank = SharedBank::new(1);
        assert!(bank.is_compatible());
        assert_eq!(bank.header.bank_bytes, bank_size_u32());
    }

    #[test]
    fn dual_banks_have_stable_distinct_aligned_addresses() {
        let storage = DualBankStorage::new(4);
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
    fn request_uses_fixed_slot_and_protocol_ticket() {
        let mut bank = SharedBank::new(9);
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
    fn timed_request_preserves_its_publication_tick() {
        let mut bank = SharedBank::new(9);
        let request = BlockRequest {
            frame_count: 256,
            input_channel_count: 2,
            output_channel_count: 2,
            midi_event_count: 256,
            event_count: 256,
            flags: 0,
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
    fn selecting_outside_the_dual_bank_is_rejected() {
        let storage = DualBankStorage::default();
        assert_eq!(
            storage.select_active(BANK_COUNT),
            Err(SharedMemoryError::InvalidBankIndex)
        );
    }
}
