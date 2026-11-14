//! The guest side: answer each request on bank 0 of one VM port with its own input.

use sp_protocol::{BlockSlot, BlockTicket, ProtocolError, SlotState};
use sp_shared_memory_os::{MonotonicClock, SharedMemoryRegion};
use std::{io, sync::atomic::Ordering, time::Duration};

/// The probe's worker ID in the slot ownership protocol.
const WORKER_ID: u32 = 1;
/// How long one wait for the doorbell lasts before the loop checks the slot again.
const WAIT: Duration = Duration::from_millis(100);

/// Answers requests until the bank fails. Returns the error that stopped it.
///
/// In the VM the guest's clock is its own domain, so the slot ticks are anchored at the
/// host's publication tick ([`MonotonicClock::slot_tick`]). The host measures the round trip
/// on its own clock.
pub(crate) fn serve(region: &mut SharedMemoryRegion, clock: MonotonicClock) -> io::Error {
    loop {
        let observed = region.request_wake_sequence();
        match answer(region, clock) {
            Ok(true) => {}
            Ok(false) => {
                if let Err(error) = region.wait_for_request(observed, WAIT) {
                    return error;
                }
            }
            Err(error) => return io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
        }
    }
}

/// Answers the request in slot 0 if there is one.
fn answer(region: &mut SharedMemoryRegion, clock: MonotonicClock) -> Result<bool, ProtocolError> {
    let Some(slot) = region.bank_mut().slot_mut(0) else {
        return Err(ProtocolError::UnexpectedState);
    };
    let Some(ticket) = requested_ticket(slot) else {
        return Ok(false);
    };
    let seen = clock.now_ticks();
    let published = slot.metadata.request_published_tick.load(Ordering::Acquire);
    match slot.claim_ticket_for_processing_at(WORKER_ID, ticket, clock.slot_tick(published, seen)) {
        Ok(_) => {}
        // The host abandoned or replaced the request first.
        Err(ProtocolError::UnexpectedState | ProtocolError::Owned | ProtocolError::StaleTicket) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    }
    let frames = (slot.metadata.frame_count as usize).min(slot.input_audio[0].len());
    let channels = (slot.metadata.output_channel_count as usize).min(slot.output_audio.len());
    for channel in 0..channels {
        slot.output_audio[channel][..frames].copy_from_slice(&slot.input_audio[channel][..frames]);
    }
    slot.publish_completion_at(WORKER_ID, ticket, clock.slot_tick(published, seen))?;
    Ok(true)
}

fn requested_ticket(slot: &BlockSlot) -> Option<BlockTicket> {
    if slot.metadata.state() != Ok(SlotState::Requested) {
        return None;
    }
    let ticket = BlockTicket {
        generation: slot.metadata.request_generation.load(Ordering::Acquire),
        sequence: slot.metadata.request_sequence.load(Ordering::Acquire),
    };
    ticket.is_valid().then_some(ticket)
}

/// Raises this process to the real-time class and pins this thread to `cpu` at
/// time-critical priority. Returns the priority class Windows gave.
#[cfg(windows)]
#[allow(unsafe_code, reason = "Win32 scheduling calls")]
pub(crate) fn enter_realtime(cpu: Option<u32>) -> io::Result<u32> {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, GetPriorityClass, REALTIME_PRIORITY_CLASS,
        SetPriorityClass, SetThreadAffinityMask, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    // SAFETY: pseudo-handles of this process and thread; plain flag arguments.
    unsafe {
        if SetPriorityClass(GetCurrentProcess(), REALTIME_PRIORITY_CLASS) == 0 {
            return Err(io::Error::last_os_error());
        }
        if SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) == 0 {
            return Err(io::Error::last_os_error());
        }
        if let Some(cpu) = cpu {
            let mask = 1_usize.checked_shl(cpu).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "CPU index is out of range")
            })?;
            if SetThreadAffinityMask(GetCurrentThread(), mask) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(GetPriorityClass(GetCurrentProcess()))
    }
}
