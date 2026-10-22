//! Forwarding from one mixed-rack segment's worker to the next.
//!
//! A mixed rack runs one worker per contiguous run of same-platform plug-ins. The host sends
//! each block to the first segment only. After its own plug-ins, a segment's worker forwards the
//! block to the next segment's bank, waits for it within the rack's block, and completes its own
//! request with the downstream result. Slots are numbered from each worker's first slot, so the
//! forwarded block carries the events and sidechains of later slots renumbered.
//!
//! When the next segment fails (a missed deadline or a dead worker), this worker outputs its own
//! result delayed by the latency of everything after it, so the rack's latency stays the same,
//! and counts the failure in its bank header for the host, which replaces that segment. While a
//! segment is down, the host can point its upstream worker past it: the skipped plug-ins'
//! latency is then applied here, before the block goes on.

use std::{io, sync::atomic::Ordering, time::Duration};

use sp_protocol::payload::ChainTarget;
use sp_shared_memory::{
    BlockEvent, BlockRequest, BlockSlot, MAX_CHANNELS, MAX_PLUGINS_PER_RACK, ProtocolHeader,
};
use sp_shared_memory_os::{ChainLink, Forwarded, MonotonicClock};

use crate::worker_runtime::{BypassDelay, PlanarBlock, SLOT_FADE_FRAMES, WARMUP_BLOCKS, crossfade};

/// Each segment finishes this much earlier than the one before it, so the upstream worker has
/// time to notice the completion, copy it, and complete its own request. An eighth of the block,
/// and at least the polling slice of a waiting link.
const MIN_HOP_MARGIN: Duration = Duration::from_micros(100);
/// Misses a new link forgives before the next segment is first heard: a worker's first blocks
/// can be slow (first plug-in calls, first scheduling). Each forgiven miss plays the bypass.
const MAX_WARMUP_MISSES: u8 = 32;

/// The forwarding state of one segment's worker.
pub struct SegmentChain {
    link: Option<ChainLink>,
    /// The next segment's first slot in this worker's numbering.
    first_slot: usize,
    /// Latency of the segments skipped between this one and the next live one.
    skipped_latency: u32,
    /// The downstream chain's latency, as last read while its link worked.
    downstream_latency: u32,
    /// This segment's output, for the delayed bypass.
    history: BypassDelay,
    own: PlanarBlock,
    /// Own output delayed by the skipped latency, sent downstream.
    skipped: PlanarBlock,
    downstream: PlanarBlock,
    delayed: PlanarBlock,
    /// Consecutive good downstream blocks, up to [`WARMUP_BLOCKS`]; below that the downstream
    /// result is not heard.
    good_blocks: u8,
    audible: bool,
    fade_position: usize,
    /// Misses forgiven since the last retarget; [`MAX_WARMUP_MISSES`] once the next segment
    /// has been heard, so later misses count as failures.
    warmup_misses: u8,
    failure_reported: bool,
}

impl SegmentChain {
    /// Creates a chain with nothing after it. Its delay line is preallocated here, off the
    /// processing path.
    pub fn new() -> Self {
        Self {
            link: None,
            first_slot: MAX_PLUGINS_PER_RACK,
            skipped_latency: 0,
            downstream_latency: 0,
            history: BypassDelay::new(),
            own: PlanarBlock::new(),
            skipped: PlanarBlock::new(),
            downstream: PlanarBlock::new(),
            delayed: PlanarBlock::new(),
            good_blocks: 0,
            audible: false,
            fade_position: SLOT_FADE_FRAMES,
            warmup_misses: 0,
            failure_reported: false,
        }
    }

    /// Points this segment at a new next segment. A new link is heard after
    /// [`WARMUP_BLOCKS`] good blocks; until then the bypass plays.
    ///
    /// # Errors
    /// Returns an error, and keeps the current target, when the bank cannot be opened.
    pub fn retarget(&mut self, target: &ChainTarget) -> io::Result<()> {
        let link = target.bank.as_deref().map(ChainLink::open).transpose()?;
        self.downstream_latency = link.as_ref().map_or(0, ChainLink::downstream_latency);
        self.link = link;
        self.first_slot = usize::from(target.first_slot);
        self.skipped_latency = target.skipped_latency;
        self.good_blocks = 0;
        self.audible = false;
        self.fade_position = SLOT_FADE_FRAMES;
        self.warmup_misses = 0;
        self.failure_reported = false;
        Ok(())
    }

    /// Latency the segments after this one add.
    pub fn latency(&self) -> u32 {
        let downstream = if self.link.is_some() {
            self.downstream_latency
        } else {
            0
        };
        self.skipped_latency.saturating_add(downstream)
    }

    /// Sends the slot's output, as this segment's plug-ins left it, through the rest of the
    /// chain and replaces it with the result. Real-time safe.
    pub fn process(
        &mut self,
        slot: &mut BlockSlot,
        header: &ProtocolHeader,
        clock: MonotonicClock,
        sample_rate: u32,
    ) {
        let frames = slot.metadata.frame_count as usize;
        let channels = (slot.metadata.output_channel_count as usize).min(MAX_CHANNELS);
        self.own.copy_from_output(slot, channels, frames);
        self.history.write(&self.own, frames);
        let skipped_latency = self.skipped_latency as usize;
        let Some(link) = self.link.as_mut() else {
            // Only skipped segments follow.
            self.history
                .read(&mut self.delayed, skipped_latency, frames);
            self.delayed.copy_to_slot(slot, channels, frames);
            return;
        };
        if !link.is_failed() {
            self.downstream_latency = link.downstream_latency();
        }
        let input = if skipped_latency == 0 {
            &self.own
        } else {
            self.history
                .read(&mut self.skipped, skipped_latency, frames);
            &self.skipped
        };
        let (published, deadline) = hop_ticks(
            slot.metadata.request_published_tick.load(Ordering::Acquire),
            clock,
            frames,
            sample_rate,
        );
        let first_slot = self.first_slot;
        let source: &BlockSlot = slot;
        let forwarded = link.forward(published, deadline, |next| {
            for channel in 0..channels {
                next.input_audio[channel][..frames]
                    .copy_from_slice(&input.channels()[channel][..frames]);
            }
            let midi = source.metadata.midi_event_count as usize;
            next.midi_events[..midi].copy_from_slice(&source.midi_events[..midi]);
            let events = forward_events(
                &source.events[..source.metadata.event_count as usize],
                first_slot,
                &mut next.events,
            );
            BlockRequest {
                frame_count: source.metadata.frame_count,
                input_channel_count: source.metadata.output_channel_count,
                output_channel_count: source.metadata.output_channel_count,
                midi_event_count: source.metadata.midi_event_count,
                event_count: u32::try_from(events).unwrap_or(u32::MAX),
                flags: 0,
                sidechain_slots: forward_sidechains(source, first_slot, next, frames),
            }
        });
        let completed = forwarded == Forwarded::Completed
            && link
                .with_completion(|next| {
                    for channel in 0..channels {
                        self.downstream.channels_mut()[channel][..frames]
                            .copy_from_slice(&next.output_audio[channel][..frames]);
                    }
                })
                .is_some()
            && self.downstream.is_finite(channels, frames);
        if forwarded == Forwarded::Bypass {
            if self.warmup_misses < MAX_WARMUP_MISSES {
                self.warmup_misses += 1;
                link.forgive();
            } else if !self.failure_reported {
                header.chain_link_failures.fetch_add(1, Ordering::Release);
                self.failure_reported = true;
            }
        }
        self.play(slot, channels, frames, completed);
    }

    /// Writes the downstream result to the slot once it is heard, and until then (or when the
    /// block was lost) this segment's output delayed by the latency after it.
    fn play(&mut self, slot: &mut BlockSlot, channels: usize, frames: usize, completed: bool) {
        self.good_blocks = if completed {
            self.good_blocks.saturating_add(1).min(WARMUP_BLOCKS)
        } else {
            0
        };
        let heard = self.good_blocks >= WARMUP_BLOCKS;
        if heard {
            // Once heard, the next segment gets no more grace.
            self.warmup_misses = MAX_WARMUP_MISSES;
        }
        let bypass_latency = self.skipped_latency as usize + self.downstream_latency as usize;
        self.history.read(&mut self.delayed, bypass_latency, frames);
        if heard != self.audible {
            self.audible = heard;
            // Joining fades in; a lost segment has no output to fade from.
            self.fade_position = if heard { 0 } else { SLOT_FADE_FRAMES };
        }
        if !heard {
            self.delayed.copy_to_slot(slot, channels, frames);
            return;
        }
        if self.fade_position < SLOT_FADE_FRAMES {
            crossfade(
                &mut self.downstream,
                &self.delayed,
                channels,
                frames,
                self.fade_position,
                true,
            );
            self.fade_position = (self.fade_position + frames).min(SLOT_FADE_FRAMES);
        }
        self.downstream.copy_to_slot(slot, channels, frames);
    }
}

/// The publication tick a block is forwarded with and the deadline for its result.
///
/// Every segment derives its deadline from the publication tick it received: one block period
/// after it, less one hop margin, so the first segment completes with time to spare for the
/// host. A segment forwards a tick one margin earlier, so each later segment finishes one margin
/// before the segment waiting on it. An untimed request (tick 0) starts now.
fn hop_ticks(published: u64, clock: MonotonicClock, frames: usize, sample_rate: u32) -> (u64, u64) {
    let origin = if published == 0 {
        clock.now_ticks()
    } else {
        published
    };
    let period = clock.duration_to_ticks(Duration::from_nanos(
        frames as u64 * 1_000_000_000 / u64::from(sample_rate.max(1)),
    ));
    let margin = (period / 8).max(clock.duration_to_ticks(MIN_HOP_MARGIN));
    (
        origin.saturating_sub(margin).max(1),
        origin.saturating_add(period).saturating_sub(margin),
    )
}

/// Copies the events meant for the next segment and later into `out`, renumbered from
/// `first_slot`, and returns how many. An event addressed to every slot (flags 0) goes on as it
/// is; one addressed to this segment's or a skipped segment's slot stays here.
pub fn forward_events(events: &[BlockEvent], first_slot: usize, out: &mut [BlockEvent]) -> usize {
    let mut count = 0;
    for event in events {
        let flags = match event.flags {
            0 => 0,
            flags => {
                let Some(slot) = (flags as usize - 1).checked_sub(first_slot) else {
                    continue;
                };
                u32::try_from(slot + 1).unwrap_or(u32::MAX)
            }
        };
        let Some(target) = out.get_mut(count) else {
            break;
        };
        *target = BlockEvent { flags, ..*event };
        count += 1;
    }
    count
}

/// Copies the sidechains of the next segment's slots and later into `to`, renumbered from
/// `first_slot`, and returns their request mask.
pub fn forward_sidechains(
    from: &BlockSlot,
    first_slot: usize,
    to: &mut BlockSlot,
    frames: usize,
) -> u32 {
    let mask = from
        .metadata
        .sidechain_slots
        .checked_shr(u32::try_from(first_slot).unwrap_or(u32::MAX))
        .unwrap_or(0);
    for slot in (0..MAX_PLUGINS_PER_RACK).filter(|slot| mask & (1 << slot) != 0) {
        for channel in 0..MAX_CHANNELS {
            to.sidechain_audio[slot][channel][..frames]
                .copy_from_slice(&from.sidechain_audio[slot + first_slot][channel][..frames]);
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    use sp_protocol::{BLOCK_EVENT_PARAMETER, BLOCK_EVENT_SLOT_BYPASS, payload::ChainTarget};
    use sp_shared_memory::{BlockEvent, BlockRequest, BlockSlot, BlockTicket, SlotState};
    use sp_shared_memory_os::{MonotonicClock, SharedMemoryRegion};

    use super::{SegmentChain, forward_events, forward_sidechains};

    /// A low rate gives the 4-frame test blocks a 40 ms deadline.
    const RATE: u32 = 100;

    fn event(event_type: u32, flags: u32) -> BlockEvent {
        BlockEvent {
            frame_offset: 0,
            event_type,
            key: 7,
            value: 0.5,
            flags,
        }
    }

    /// Slots 0..3 are this segment's; slot 3 onwards is the next segment's slot 0.
    #[test]
    fn events_for_later_slots_go_on_renumbered_and_broadcasts_go_everywhere() {
        let events = [
            event(BLOCK_EVENT_PARAMETER, 1),
            event(BLOCK_EVENT_PARAMETER, 4),
            event(BLOCK_EVENT_SLOT_BYPASS, 3),
            event(BLOCK_EVENT_SLOT_BYPASS, 6),
            event(BLOCK_EVENT_PARAMETER, 0),
        ];
        let mut out = [BlockEvent::default(); 8];
        let count = forward_events(&events, 3, &mut out);
        assert_eq!(
            out[..count]
                .iter()
                .map(|event| (event.event_type, event.flags))
                .collect::<Vec<_>>(),
            [
                (BLOCK_EVENT_PARAMETER, 1),
                (BLOCK_EVENT_SLOT_BYPASS, 3),
                (BLOCK_EVENT_PARAMETER, 0)
            ]
        );
    }

    #[test]
    fn sidechains_for_later_slots_move_down_with_their_mask() {
        let mut from = BlockSlot::new();
        from.metadata.sidechain_slots = 0b1010_0010;
        from.sidechain_audio[5][1][3] = 0.5;
        from.sidechain_audio[7][0][0] = -0.25;
        let mut to = BlockSlot::new();
        assert_eq!(forward_sidechains(&from, 4, &mut to, 4), 0b1010);
        assert!((to.sidechain_audio[1][1][3] - 0.5).abs() < f32::EPSILON);
        assert!((to.sidechain_audio[3][0][0] + 0.25).abs() < f32::EPSILON);
    }

    fn requested(value: f32) -> BlockSlot {
        let mut slot = BlockSlot::new();
        slot.publish_request(
            BlockTicket {
                generation: 1,
                sequence: 1,
            },
            BlockRequest {
                frame_count: 4,
                input_channel_count: 2,
                output_channel_count: 2,
                midi_event_count: 0,
                event_count: 0,
                flags: 0,
                sidechain_slots: 0,
            },
        )
        .unwrap();
        // The rack has already written this segment's output.
        for channel in &mut slot.output_audio {
            channel[..4].fill(value);
        }
        slot
    }

    /// A downstream segment that adds one, with two samples of latency it reports but does not
    /// apply (so its output shows which path played).
    fn spawn_adder(name: String, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut region = SharedMemoryRegion::open(&name).unwrap();
            region
                .bank()
                .header
                .worker_latency_samples
                .store(2, Ordering::Release);
            let clock = MonotonicClock::new().unwrap();
            while !stop.load(Ordering::Acquire) {
                let observed = region.request_wake_sequence();
                let Some(index) = region
                    .bank()
                    .slots
                    .iter()
                    .position(|slot| slot.metadata.state() == Ok(SlotState::Requested))
                else {
                    let _ = region.wait_for_request(observed, Duration::from_millis(5));
                    continue;
                };
                let slot = region.bank_mut().slot_mut(index).unwrap();
                let ticket = slot.claim_for_processing_at(3, clock.now_ticks()).unwrap();
                for channel in 0..2 {
                    for frame in 0..4 {
                        slot.output_audio[channel][frame] = slot.input_audio[channel][frame] + 1.0;
                    }
                }
                slot.publish_completion_at(3, ticket, clock.now_ticks())
                    .unwrap();
            }
        })
    }

    /// The downstream result is heard after warmup; when the downstream worker stops, this
    /// segment's own output plays, delayed by the downstream latency, and the failure is counted
    /// once.
    #[test]
    fn a_lost_segment_is_bypassed_with_its_latency_and_counted_once() {
        let own_bank = SharedMemoryRegion::create(1).unwrap();
        let next_bank = SharedMemoryRegion::create(2).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = spawn_adder(next_bank.name().to_owned(), Arc::clone(&stop));
        thread::sleep(Duration::from_millis(20));
        let clock = MonotonicClock::new().unwrap();
        let mut chain = SegmentChain::new();
        chain
            .retarget(&ChainTarget {
                bank: Some(next_bank.name().to_owned()),
                first_slot: 1,
                skipped_latency: 0,
            })
            .unwrap();
        assert_eq!(chain.latency(), 2);
        let header = &own_bank.bank().header;
        let mut outputs = Vec::new();
        // 4-frame blocks: the fade into the downstream result takes four of them.
        for block in 0..12_u8 {
            let mut slot = requested(f32::from(block));
            chain.process(&mut slot, header, clock, RATE);
            outputs.push(slot.output_audio[0][..4].to_vec());
        }
        assert_eq!(
            outputs[1],
            [0.0, 0.0, 1.0, 1.0],
            "warming: own output two samples late"
        );
        assert_eq!(outputs[11], [12.0; 4], "heard: own output plus one");
        stop.store(true, Ordering::Release);
        let _ = next_bank.notify_request();
        worker.join().unwrap();

        let mut slot = requested(20.0);
        chain.process(&mut slot, header, clock, RATE);
        assert_eq!(slot.output_audio[0][..4], [11.0, 11.0, 20.0, 20.0]);
        let mut slot = requested(21.0);
        chain.process(&mut slot, header, clock, RATE);
        assert_eq!(slot.output_audio[0][..4], [20.0, 20.0, 21.0, 21.0]);
        assert_eq!(header.chain_link_failures.load(Ordering::Acquire), 1);
        assert_eq!(chain.latency(), 2, "the rack keeps its latency");
    }
}
