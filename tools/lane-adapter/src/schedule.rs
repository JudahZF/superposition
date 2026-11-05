//! Lambda's block timing for a set of rack inserts, on the real clock.
//!
//! A Lambda block starts when the I/O hands over its input. Each insert task reads its rack in
//! its layer and publishes right after; the worker is woken after that layer's barrier. The
//! earliest a layer can read is the start of the block (earlier layers took no time); the latest
//! it can publish is the end of its admitted cost. [`Schedule`] uses both extremes: it reads
//! every rack at the block start and publishes each rack at its [`Placement`], so each rack gets
//! exactly its least window: from its placement to the next block start.

use std::time::Duration;

use sp_shared_memory_os::MonotonicClock;

use crate::{Insert, ReadOutcome};

/// When in the block a rack publishes, as a fraction of the block period.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement(f64);

impl Placement {
    /// A rack in the first layer publishes at the start of the block: a window of one block.
    pub const FIRST_LAYER: Self = Self(0.0);

    /// A rack in the last layer is woken after the pool run, which may take the whole admitted
    /// budget (Lambda's default is 0.7 of a block): a window of `1 − budget`.
    #[must_use]
    pub fn last_layer(budget: f64) -> Self {
        Self(budget.clamp(0.0, 1.0))
    }

    /// A rack whose window is `window` of the block (0–1).
    #[must_use]
    pub fn window(window: f64) -> Self {
        Self(1.0 - window.clamp(0.0, 1.0))
    }

    /// The fraction of the block before the rack publishes.
    #[must_use]
    pub const fn fraction(self) -> f64 {
        self.0
    }

    /// The rack's window: from publication to the next block start.
    #[must_use]
    pub fn window_of(self, period: Duration) -> Duration {
        period.mul_f64(1.0 - self.0)
    }
}

/// What one block of the schedule did.
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockReport {
    /// How late the block's reads started, in shared clock ticks.
    pub start_lateness: u64,
    /// How far past the next block start the block's last publication ended, in ticks
    /// (zero when on time). A late host shortens the next block, not the workers' window.
    pub overrun: u64,
}

/// Runs inserts on Lambda's block timing at a fixed period.
pub struct Schedule {
    clock: MonotonicClock,
    period: u64,
    next_start: u64,
    block: u64,
}

impl Schedule {
    /// A schedule whose first block starts one period from now.
    #[must_use]
    pub fn new(clock: MonotonicClock, period: Duration) -> Self {
        let period = clock.duration_to_ticks(period);
        Self {
            clock,
            period,
            next_start: clock.now_ticks() + period,
            block: 0,
        }
    }

    /// When the next block starts, on the shared clock: the read deadline of the requests
    /// published in the current block.
    #[must_use]
    pub const fn next_block_start(&self) -> u64 {
        self.next_start
    }

    /// The block period in shared clock ticks.
    #[must_use]
    pub const fn period_ticks(&self) -> u64 {
        self.period
    }

    /// Waits for the next block start and runs the block: reads every insert (calling
    /// `on_read` with its outcome and output), then publishes and wakes each insert at its
    /// placement, with the next block start as its read deadline. `placements` holds one
    /// placement per insert. Real-time safe.
    ///
    /// # Panics
    /// If `placements` and `inserts` differ in length, or `input` or `output` holds fewer than
    /// `frames * 2` samples.
    pub fn run_block(
        &mut self,
        inserts: &mut [Insert],
        placements: &[Placement],
        input: &[f32],
        frames: usize,
        output: &mut [f32],
        mut on_read: impl FnMut(usize, ReadOutcome, &[f32]),
    ) -> BlockReport {
        assert_eq!(inserts.len(), placements.len(), "one placement per insert");
        let start = self.next_start;
        self.wait_until(start);
        let mut report = BlockReport {
            start_lateness: self.clock.now_ticks().saturating_sub(start),
            overrun: 0,
        };
        self.next_start = start + self.period;
        let deadline = self.next_start;
        for (index, insert) in inserts.iter_mut().enumerate() {
            let outcome = insert.read(self.block, input, frames, output);
            on_read(index, outcome, &output[..frames * 2]);
        }
        // Publish in placement order; each publication is its layer's barrier.
        let mut published = 0_u64;
        while published.count_ones() as usize != inserts.len() {
            let (index, placement) = placements
                .iter()
                .enumerate()
                .filter(|(index, _)| published & (1 << index) == 0)
                .min_by(|(_, left), (_, right)| left.0.total_cmp(&right.0))
                .expect("an unpublished insert remains");
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "a fraction of one block period"
            )]
            let at = start + (self.period as f64 * placement.0) as u64;
            self.wait_until(at);
            inserts[index].write(self.block, input, frames, deadline);
            inserts[index].wake();
            published |= 1 << index;
        }
        report.overrun = self.clock.now_ticks().saturating_sub(deadline);
        if report.overrun > 0 {
            // Skip the blocks the host missed, as an I/O callback would.
            let missed = report.overrun / self.period + 1;
            self.next_start += missed * self.period;
        }
        self.block += 1;
        report
    }

    /// Sleeps until shortly before `tick`, then spins to it.
    fn wait_until(&self, tick: u64) {
        let spin = self.clock.duration_to_ticks(Duration::from_micros(150));
        loop {
            let now = self.clock.now_ticks();
            if now >= tick {
                return;
            }
            if tick - now > spin * 2 {
                std::thread::sleep(self.clock.ticks_to_duration(tick - now - spin));
            } else {
                std::hint::spin_loop();
            }
        }
    }
}
