//! In-place construction of the fixed shared-memory layout.

#![allow(unsafe_code)]

use std::ptr;
use std::sync::atomic::AtomicU64;

use sp_protocol::{
    BLOCK_SLOT_COUNT, BlockSlot, MAX_PLUGINS_PER_RACK, MAX_RACKS, ProtocolHeader, RackDescriptor,
};

use super::parameter_feedback::{
    PARAMETER_FEEDBACK_CAPACITY_PER_SLOT, ParameterFeedbackCell, ParameterFeedbackSlot,
};
use super::{SharedBank, SharedMemoryError, bank_size_u32};

pub(super) fn new_boxed(generation: u64) -> Result<Box<SharedBank>, SharedMemoryError> {
    if generation == 0 {
        return Err(sp_protocol::ProtocolError::InvalidTicket.into());
    }
    let mut bank = Box::<SharedBank>::new_uninit();
    let destination = bank.as_mut_ptr();
    // SAFETY: the Box owns exclusive uninitialized storage for one SharedBank. Every field is
    // written exactly once with its proper constructor before assume_init; no reference to the
    // storage escapes. SharedBank contains only atomics and fixed, pointer-free protocol data.
    unsafe {
        ptr::addr_of_mut!((*destination).header)
            .write(ProtocolHeader::new(bank_size_u32(), generation));
        let racks = ptr::addr_of_mut!((*destination).racks).cast::<RackDescriptor>();
        for index in 0..MAX_RACKS {
            racks.add(index).write(RackDescriptor::EMPTY);
        }
        let slots = ptr::addr_of_mut!((*destination).slots).cast::<BlockSlot>();
        for index in 0..BLOCK_SLOT_COUNT {
            slots.add(index).write(BlockSlot::new());
        }
        let feedback = ptr::addr_of_mut!((*destination).feedback);
        let feedback_slots = ptr::addr_of_mut!((*feedback).slots).cast::<ParameterFeedbackSlot>();
        for index in 0..MAX_PLUGINS_PER_RACK {
            let slot = feedback_slots.add(index);
            ptr::addr_of_mut!((*slot).epoch).write(AtomicU64::new(0));
            ptr::addr_of_mut!((*slot).revision).write(AtomicU64::new(0));
            ptr::addr_of_mut!((*slot).overflow_count).write(AtomicU64::new(0));
            ptr::addr_of_mut!((*slot).restart_counts)
                .write(std::array::from_fn(|_| AtomicU64::new(0)));
            let cells = ptr::addr_of_mut!((*slot).cells).cast::<ParameterFeedbackCell>();
            for cell_index in 0..PARAMETER_FEEDBACK_CAPACITY_PER_SLOT {
                cells.add(cell_index).write(ParameterFeedbackCell::new());
            }
        }
        Ok(bank.assume_init())
    }
}
