//! A plug-in-free rack worker. It serves one bank the way `sp-plugin-worker`'s processing
//! thread does (futex wait, ticket claim, real-time policy, mixed-rack forwarding), but its
//! "plug-in" multiplies the block by a gain and can spin for a fixed time to stand in for
//! plug-in cost. The same code runs on a test thread, as a Linux process, and as a Windows
//! process under Wine.

use std::{
    io,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use sp_shared_memory::{
    BLOCK_SLOT_COUNT, BlockRequest, BlockSlot, MAX_CHANNELS, MAX_FRAMES, ProtocolError,
    ProtocolHeader, SlotState,
};
use sp_shared_memory_os::{
    AudioThreadPolicy, ChainLink, Forwarded, Hop, MonotonicClock, SharedMemoryRegion,
};

/// Valid downstream blocks before a new next segment is heard, as in the product worker.
const WARMUP_BLOCKS: u8 = 3;
/// Misses a new link forgives before the next segment is first heard, as in the product worker.
const MAX_WARMUP_MISSES: u8 = 32;
/// Longest wait between scans while no audio arrives.
const IDLE_WAIT: Duration = Duration::from_millis(10);

/// How an echo worker processes.
#[derive(Clone, Copy, Debug)]
pub struct EchoConfig {
    /// Owner ID the worker claims slots with (nonzero).
    pub worker_id: u32,
    /// Gain the worker's "plug-in" applies.
    pub gain: f32,
    /// Busy time per block, standing in for plug-in cost.
    pub load: Duration,
    /// Session rate; sets the one-block deadline of untimed requests.
    pub sample_rate: u32,
    /// Run the processing thread with the product's real-time policy.
    pub realtime: bool,
}

/// One echo worker on one bank, with an optional link to the next segment.
pub struct EchoWorker {
    region: SharedMemoryRegion,
    clock: MonotonicClock,
    config: EchoConfig,
    chain: EchoChain,
    policy: Option<AudioThreadPolicy>,
    /// Shared clock tick until which audio counts as running (two blocks after the last one).
    active_until: u64,
}

impl EchoWorker {
    /// Opens the bank by name. Call on the thread that will serve it.
    ///
    /// # Errors
    /// Returns an error when the bank cannot be opened, or the real-time policy is requested
    /// and unavailable (under Wine, without `sp_wine_bridge`).
    pub fn open(bank: &str, config: EchoConfig) -> io::Result<Self> {
        let region = SharedMemoryRegion::open(bank)?;
        let clock = MonotonicClock::new()?;
        let policy = config.realtime.then(AudioThreadPolicy::new).transpose()?;
        region
            .bank()
            .header
            .publish_worker_heartbeat(clock.now_ticks())
            .map_err(io::Error::other)?;
        Ok(Self {
            region,
            clock,
            config,
            chain: EchoChain::default(),
            policy,
            active_until: 0,
        })
    }

    /// Points this segment at the next one (a bank name), or at nothing.
    ///
    /// # Errors
    /// Returns an error, and keeps the current link, when the bank cannot be opened.
    pub fn retarget(&mut self, next: Option<&str>) -> io::Result<()> {
        self.chain.retarget(next)
    }

    /// Serves every requested slot once, then waits for the next request wake (at most two
    /// block periods while audio runs). Returns whether it processed a block.
    ///
    /// # Errors
    /// Returns an error for a protocol violation or a scheduling failure.
    pub fn serve_once(&mut self) -> io::Result<bool> {
        let header = &self.region.bank().header;
        header
            .worker_loop_tick
            .store(self.clock.now_ticks(), Ordering::Release);
        // Snapshot before scanning: a publication during the scan invalidates the wait.
        let wake_sequence = self.region.request_wake_sequence();
        header
            .worker_wait_sequence
            .store(wake_sequence, Ordering::Release);
        let mut processed = false;
        for slot_index in 0..BLOCK_SLOT_COUNT {
            processed |= self.serve_slot(slot_index)?;
        }
        let now = self.clock.now_ticks();
        self.region
            .bank()
            .header
            .publish_worker_heartbeat(now)
            .map_err(io::Error::other)?;
        if !processed {
            let timeout = if now < self.active_until {
                self.clock.ticks_to_duration(self.active_until - now)
            } else {
                IDLE_WAIT
            };
            self.region.wait_for_request(wake_sequence, timeout)?;
        }
        Ok(processed)
    }

    fn serve_slot(&mut self, slot_index: usize) -> io::Result<bool> {
        let clock = self.clock;
        let bank = self.region.bank_mut();
        let header = &bank.header;
        let Some(slot) = bank.slots.get_mut(slot_index) else {
            return Ok(false);
        };
        if slot.metadata.state() != Ok(SlotState::Requested) {
            return Ok(false);
        }
        let ticket = match slot.claim_for_processing_at(self.config.worker_id, clock.now_ticks()) {
            Ok(ticket) => ticket,
            Err(ProtocolError::UnexpectedState | ProtocolError::Owned) => return Ok(false),
            Err(error) => return Err(io::Error::other(error)),
        };
        let frames = slot.metadata.frame_count;
        if let Some(policy) = self.policy.as_mut() {
            policy.enter(frames, self.config.sample_rate)?;
        }
        let started = Instant::now();
        let frames = frames as usize;
        let channels = (slot.metadata.output_channel_count as usize).min(MAX_CHANNELS);
        // A mono input feeds every output channel; an instrument (no input) plays silence.
        let inputs = slot.metadata.input_channel_count as usize;
        for channel in 0..channels {
            for frame in 0..frames {
                let input = if inputs == 0 {
                    0.0
                } else {
                    slot.input_audio[channel.min(inputs - 1)][frame]
                };
                slot.output_audio[channel][frame] = input * self.config.gain;
            }
        }
        while started.elapsed() < self.config.load {
            std::hint::spin_loop();
        }
        self.chain
            .process(slot, header, clock, self.config.sample_rate);
        slot.publish_completion_at(self.config.worker_id, ticket, clock.now_ticks())
            .map_err(io::Error::other)?;
        let period = clock.duration_to_ticks(Duration::from_nanos(
            frames as u64 * 1_000_000_000 / u64::from(self.config.sample_rate.max(1)),
        ));
        self.active_until = clock.now_ticks().saturating_add(period * 2);
        Ok(true)
    }
}

/// The forwarding half of a mixed-rack segment: a simplified `SegmentChain` of the product
/// worker. A lost next segment is bypassed (this segment's own output plays) and counted once
/// in this bank's `chain_link_failures`; a new link is heard after [`WARMUP_BLOCKS`] good
/// blocks and forgives up to [`MAX_WARMUP_MISSES`] misses before that.
#[derive(Default)]
struct EchoChain {
    link: Option<ChainLink>,
    good_blocks: u8,
    warmup_misses: u8,
    failure_reported: bool,
}

impl EchoChain {
    fn retarget(&mut self, next: Option<&str>) -> io::Result<()> {
        self.link = next.map(ChainLink::open).transpose()?;
        self.good_blocks = 0;
        self.warmup_misses = 0;
        self.failure_reported = false;
        Ok(())
    }

    fn process(
        &mut self,
        slot: &mut BlockSlot,
        header: &ProtocolHeader,
        clock: MonotonicClock,
        sample_rate: u32,
    ) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let frames = slot.metadata.frame_count as usize;
        let channels = (slot.metadata.output_channel_count as usize).min(MAX_CHANNELS);
        let hop = Hop::derive(
            slot.metadata.request_published_tick.load(Ordering::Acquire),
            slot.metadata.deadline_offset_ticks,
            clock,
            frames,
            sample_rate,
        );
        let source: &BlockSlot = slot;
        let forwarded = link.forward(hop.forward_tick, hop.deadline_tick, |next| {
            for channel in 0..channels {
                next.input_audio[channel][..frames]
                    .copy_from_slice(&source.output_audio[channel][..frames]);
            }
            BlockRequest {
                frame_count: source.metadata.frame_count,
                input_channel_count: source.metadata.output_channel_count,
                output_channel_count: source.metadata.output_channel_count,
                midi_event_count: 0,
                event_count: 0,
                flags: 0,
                sidechain_slots: 0,
            }
        });
        let mut downstream = [[0.0; MAX_FRAMES]; MAX_CHANNELS];
        let completed = forwarded == Forwarded::Completed
            && link
                .with_completion(|next| {
                    for (to, from) in downstream.iter_mut().zip(&next.output_audio).take(channels) {
                        to[..frames].copy_from_slice(&from[..frames]);
                    }
                })
                .is_some()
            && downstream[..channels]
                .iter()
                .all(|channel| channel[..frames].iter().all(|sample| sample.is_finite()));
        if completed {
            self.good_blocks = self.good_blocks.saturating_add(1).min(WARMUP_BLOCKS);
        } else {
            self.good_blocks = 0;
            if self.warmup_misses < MAX_WARMUP_MISSES {
                self.warmup_misses += 1;
                link.forgive();
            } else if !self.failure_reported {
                header.chain_link_failures.fetch_add(1, Ordering::Release);
                self.failure_reported = true;
            }
        }
        if self.good_blocks >= WARMUP_BLOCKS {
            // Once heard, the next segment gets no more grace.
            self.warmup_misses = MAX_WARMUP_MISSES;
            for (to, from) in slot.output_audio.iter_mut().zip(&downstream).take(channels) {
                to[..frames].copy_from_slice(&from[..frames]);
            }
        }
    }
}
