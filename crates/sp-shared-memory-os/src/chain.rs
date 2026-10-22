//! Forwarding a block to the next segment of a mixed rack.
//!
//! A rack whose plug-ins need different workers (native Linux and Windows under Wine) runs one
//! worker per contiguous segment. The host dispatches only to the first segment. Each segment's
//! worker processes its plug-ins and then, through a [`ChainLink`], requests the next segment on
//! that segment's bank, waits for its completion within the rack's block budget, and only then
//! completes its own request. The host sees one pipelined rack.
//!
//! A link that misses its deadline abandons the request and stays failed: its bank retains the
//! abandoned slot until the host replaces the downstream worker and gives the upstream worker a
//! fresh bank. Meanwhile the caller bypasses the downstream segment.

use std::{io, time::Duration};

use sp_shared_memory::{BlockRequest, BlockSlot, BlockTicket, SlotState};

use crate::{MonotonicClock, SharedMemoryRegion};

/// How long a link spins before it sleeps between checks; most segments finish within it.
const SPIN: Duration = Duration::from_micros(200);
/// The sleep between checks after the spin. Workers do not signal completions, so a waiting
/// link polls; this bounds the added latency.
const SLEEP_SLICE: Duration = Duration::from_micros(100);

/// The result of forwarding one block.
#[derive(Debug, Eq, PartialEq)]
pub enum Forwarded {
    /// The downstream segment completed; read its output with [`ChainLink::with_completion`].
    Completed,
    /// The link is failed or the downstream segment missed the deadline; bypass it.
    Bypass,
}

/// The upstream end of one segment-to-segment link.
pub struct ChainLink {
    region: SharedMemoryRegion,
    clock: MonotonicClock,
    live: Option<(usize, BlockTicket)>,
    failed: bool,
}

impl ChainLink {
    /// Opens the downstream segment's bank by its POSIX name.
    ///
    /// # Errors
    /// Returns an error when the bank cannot be opened or the clock is unavailable.
    pub fn open(bank_name: &str) -> io::Result<Self> {
        Ok(Self {
            region: SharedMemoryRegion::open(bank_name)?,
            clock: MonotonicClock::new()?,
            live: None,
            failed: false,
        })
    }

    /// Whether an earlier miss failed this link until the host replaces the downstream worker.
    #[must_use]
    pub const fn is_failed(&self) -> bool {
        self.failed
    }

    /// Requests one block downstream and waits until `deadline_tick` (on the shared clock).
    ///
    /// `fill` writes the request payload (input audio, sidechains, events, MIDI) into the free
    /// slot before it is published. Real-time safe: no allocation; it spins briefly, then sleeps
    /// in short slices.
    pub fn forward(
        &mut self,
        request: BlockRequest,
        deadline_tick: u64,
        fill: impl FnOnce(&mut BlockSlot),
    ) -> Forwarded {
        if self.failed {
            return Forwarded::Bypass;
        }
        self.release();
        let Some(slot_index) = self
            .region
            .bank()
            .slots
            .iter()
            .position(|slot| slot.metadata.state() == Ok(SlotState::Free))
        else {
            return self.fail();
        };
        let Some(slot) = self.region.bank_mut().slot_mut(slot_index) else {
            return self.fail();
        };
        fill(slot);
        let now = self.clock.now_ticks();
        let Ok(ticket) = self
            .region
            .bank_mut()
            .request_block_at(slot_index, request, now.max(1))
        else {
            return self.fail();
        };
        self.live = Some((slot_index, ticket));
        let _ = self.region.notify_request();
        let spin_until = now.saturating_add(self.clock.duration_to_ticks(SPIN));
        loop {
            let slot = &self.region.bank().slots[slot_index];
            match slot.completion_snapshot() {
                Ok(Some(snapshot)) if snapshot.ticket == ticket => return Forwarded::Completed,
                Ok(Some(_)) | Err(_) => return self.fail(),
                Ok(None) => {}
            }
            let now = self.clock.now_ticks();
            if now >= deadline_tick {
                return self.fail();
            }
            if now < spin_until {
                std::hint::spin_loop();
            } else {
                let left = self.clock.ticks_to_duration(deadline_tick - now);
                std::thread::sleep(left.min(SLEEP_SLICE));
            }
        }
    }

    /// Reads the completed downstream slot. Call only after [`Forwarded::Completed`].
    pub fn with_completion<R>(&self, read: impl FnOnce(&BlockSlot) -> R) -> Option<R> {
        let (slot_index, _) = self.live?;
        Some(read(&self.region.bank().slots[slot_index]))
    }

    /// Frees the completed slot for the next block. `forward` also does this first.
    pub fn release(&mut self) {
        if let Some((slot_index, ticket)) = self.live.take() {
            let _ = self.region.bank().slots[slot_index].consume_completion(ticket);
        }
    }

    fn fail(&mut self) -> Forwarded {
        if let Some((slot_index, ticket)) = self.live.take() {
            // Only an unclaimed request can be abandoned; a claimed one stays with its worker,
            // which the host replaces.
            let _ = self.region.bank().slots[slot_index].abandon_request(ticket);
        }
        self.failed = true;
        Forwarded::Bypass
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use sp_shared_memory::{BlockRequest, SlotState};

    use super::{ChainLink, Forwarded};
    use crate::{MonotonicClock, SharedMemoryRegion};

    fn request(frames: u32) -> BlockRequest {
        BlockRequest {
            frame_count: frames,
            input_channel_count: 2,
            output_channel_count: 2,
            midi_event_count: 0,
            event_count: 0,
            flags: 0,
            sidechain_slots: 0,
        }
    }

    /// A downstream "worker" that doubles its input until stopped.
    fn spawn_doubler(name: String, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut region = SharedMemoryRegion::open(&name).unwrap();
            let clock = MonotonicClock::new().unwrap();
            while !stop.load(Ordering::Acquire) {
                let observed = region.request_wake_sequence();
                let pending = region
                    .bank()
                    .slots
                    .iter()
                    .position(|slot| slot.metadata.state() == Ok(SlotState::Requested));
                let Some(index) = pending else {
                    let _ = region.wait_for_request(observed, Duration::from_millis(5));
                    continue;
                };
                let slot = region.bank_mut().slot_mut(index).unwrap();
                let ticket = slot.claim_for_processing_at(7, clock.now_ticks()).unwrap();
                // The test always forwards 64-frame blocks.
                for channel in 0..2 {
                    for frame in 0..64 {
                        slot.output_audio[channel][frame] = slot.input_audio[channel][frame] * 2.0;
                    }
                }
                slot.publish_completion_at(7, ticket, clock.now_ticks())
                    .unwrap();
            }
        })
    }

    #[test]
    fn forwards_blocks_and_reads_the_downstream_result() {
        let host = SharedMemoryRegion::create(3).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = spawn_doubler(host.name().to_owned(), Arc::clone(&stop));
        let mut link = ChainLink::open(host.name()).unwrap();
        let clock = MonotonicClock::new().unwrap();
        for block in 0..200_u8 {
            let deadline = clock.now_ticks() + clock.duration_to_ticks(Duration::from_secs(1));
            let value = f32::from(block);
            let result = link.forward(request(64), deadline, |slot| {
                slot.input_audio[0][..64].fill(value);
                slot.input_audio[1][..64].fill(-value);
            });
            assert_eq!(result, Forwarded::Completed);
            let output = link
                .with_completion(|slot| (slot.output_audio[0][63], slot.output_audio[1][0]))
                .unwrap();
            assert_eq!(output, (value * 2.0, -value * 2.0));
        }
        stop.store(true, Ordering::Release);
        let _ = host.notify_request();
        worker.join().unwrap();
    }

    #[test]
    fn a_missed_deadline_fails_the_link_until_it_is_replaced() {
        let host = SharedMemoryRegion::create(5).unwrap();
        let mut link = ChainLink::open(host.name()).unwrap();
        let clock = MonotonicClock::new().unwrap();
        // No downstream worker: the request is never claimed.
        let deadline = clock.now_ticks() + clock.duration_to_ticks(Duration::from_millis(2));
        assert_eq!(
            link.forward(request(64), deadline, |_| {}),
            Forwarded::Bypass
        );
        assert!(link.is_failed());
        assert!(
            host.bank()
                .slots
                .iter()
                .any(|slot| slot.metadata.state() == Ok(SlotState::Abandoned)),
            "the missed request is abandoned, not left for a late worker"
        );
        let later = clock.now_ticks() + clock.duration_to_ticks(Duration::from_secs(1));
        assert_eq!(link.forward(request(64), later, |_| {}), Forwarded::Bypass);
    }
}
