//! Callback counters written by the JACK thread and read coherently by the control thread.

use std::sync::atomic::{AtomicU64, Ordering, fence};

use sp_audio_io::{BlockFrames, CallbackTelemetry};

/// How one callback ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockOutcome {
    Rendered,
    Silenced,
    InvalidFrames,
    InvalidBuffers,
    InvalidChannels,
}

/// A single-writer sequence lock over [`CallbackTelemetry`].
///
/// The callback is the only writer. It makes the sequence odd, updates the counters, and makes it
/// even again. Readers retry until they see the same even sequence before and after, so every
/// snapshot satisfies [`CallbackTelemetry::is_coherent`]. Writing never blocks or allocates.
#[derive(Default)]
pub(crate) struct TelemetryCell {
    sequence: AtomicU64,
    callbacks: AtomicU64,
    rendered: AtomicU64,
    silenced: AtomicU64,
    invalid_frames: AtomicU64,
    invalid_buffers: AtomicU64,
    invalid_channels: AtomicU64,
    histogram: [AtomicU64; 4],
    /// The callback thread's kernel ID, 0 until its first block, for `promote_callback`.
    callback_thread: AtomicU64,
}

impl TelemetryCell {
    /// Records the calling thread as the callback thread, once.
    pub(crate) fn record_callback_thread(&self) {
        if self.callback_thread.load(Ordering::Relaxed) == 0 {
            self.callback_thread
                .store(sp_shared_memory_os::current_thread_id(), Ordering::Release);
        }
    }

    /// The callback thread's kernel ID, once it has run.
    pub(crate) fn callback_thread(&self) -> Option<u64> {
        Some(self.callback_thread.load(Ordering::Acquire)).filter(|&thread| thread != 0)
    }

    pub(crate) fn record(&self, outcome: BlockOutcome, frames: BlockFrames) {
        let sequence = self.sequence.load(Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        let bump = |counter: &AtomicU64| {
            counter.store(counter.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        };
        bump(&self.callbacks);
        match outcome {
            BlockOutcome::Rendered | BlockOutcome::Silenced => {
                bump(if outcome == BlockOutcome::Rendered {
                    &self.rendered
                } else {
                    &self.silenced
                });
                bump(
                    &self.histogram[match frames {
                        BlockFrames::Frames32 => 0,
                        BlockFrames::Frames64 => 1,
                        BlockFrames::Frames128 => 2,
                        BlockFrames::Frames256 => 3,
                    }],
                );
            }
            BlockOutcome::InvalidFrames => bump(&self.invalid_frames),
            BlockOutcome::InvalidBuffers => bump(&self.invalid_buffers),
            BlockOutcome::InvalidChannels => bump(&self.invalid_channels),
        }
        self.sequence
            .store(sequence.wrapping_add(2), Ordering::Release);
    }

    pub(crate) fn snapshot(&self) -> CallbackTelemetry {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
            let snapshot = CallbackTelemetry {
                callbacks: load(&self.callbacks),
                rendered: load(&self.rendered),
                silenced: load(&self.silenced),
                invalid_frames: load(&self.invalid_frames),
                invalid_buffers: load(&self.invalid_buffers),
                invalid_channels: load(&self.invalid_channels),
                invalid_bytes: 0,
                frame_histogram_32: load(&self.histogram[0]),
                frame_histogram_64: load(&self.histogram[1]),
                frame_histogram_128: load(&self.histogram[2]),
                frame_histogram_256: load(&self.histogram[3]),
            };
            fence(Ordering::Acquire);
            if self.sequence.load(Ordering::Relaxed) == before {
                return snapshot;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sp_audio_io::BlockFrames;

    use super::{BlockOutcome, TelemetryCell};

    #[test]
    fn concurrent_snapshots_are_always_coherent() {
        let cell = Arc::new(TelemetryCell::default());
        let writer = {
            let cell = Arc::clone(&cell);
            std::thread::spawn(move || {
                let outcomes = [
                    BlockOutcome::Rendered,
                    BlockOutcome::Silenced,
                    BlockOutcome::InvalidFrames,
                    BlockOutcome::InvalidChannels,
                ];
                for index in 0..200_000 {
                    cell.record(outcomes[index % outcomes.len()], BlockFrames::Frames64);
                }
            })
        };
        while !writer.is_finished() {
            assert!(cell.snapshot().is_coherent());
        }
        writer.join().unwrap();
        let snapshot = cell.snapshot();
        assert_eq!(snapshot.callbacks, 200_000);
        assert_eq!(snapshot.frame_histogram_64, 100_000);
        assert!(snapshot.is_coherent());
    }
}
