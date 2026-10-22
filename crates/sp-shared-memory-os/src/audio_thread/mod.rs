//! Scoped real-time scheduling for the dedicated, paced plug-in processing thread.
//!
//! macOS applies a Mach time constraint sized to the block period. Linux, and a Windows worker
//! under Wine through `sp_wine_bridge`, apply `SCHED_FIFO`. When Linux denies real-time
//! priority (no `RLIMIT_RTPRIO`), the thread keeps ordinary scheduling, reports
//! [`SchedulingMode::Normal`], and warns once, unless `SUPERPOSITION_REQUIRE_REALTIME=1`
//! makes the denial an error.

use std::io;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", windows))]
mod posix_fifo;

#[cfg(target_os = "macos")]
pub use macos::AudioThreadPolicy;
#[cfg(any(target_os = "linux", windows))]
pub use posix_fifo::AudioThreadPolicy;

/// The scheduling the processing thread actually received.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulingMode {
    /// A real-time policy is active or will be applied on `enter`.
    RealTime,
    /// The platform denied real-time priority; the thread uses ordinary scheduling.
    Normal,
}

/// Block sizes the engine accepts, in frames.
pub const SUPPORTED_BLOCK_FRAMES: [u32; 4] = [32, 64, 128, 256];
/// Sample rates the engine accepts, in hertz.
pub const SUPPORTED_SAMPLE_RATES: [u32; 2] = [48_000, 96_000];

pub(crate) fn validate_format(frames: u32, sample_rate: u32) -> io::Result<()> {
    if SUPPORTED_BLOCK_FRAMES.contains(&frames) && SUPPORTED_SAMPLE_RATES.contains(&sample_rate) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported processing format: {frames} frames at {sample_rate} Hz"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioThreadPolicy, SchedulingMode};

    #[test]
    fn policy_validates_format_and_restores_the_dedicated_thread() {
        std::thread::spawn(|| {
            let mut policy = AudioThreadPolicy::new().expect("thread policy");
            assert!(policy.enter(127, 48_000).is_err());
            assert!(policy.enter(128, 44_100).is_err());
            for (frames, rate) in [(32, 48_000), (64, 48_000), (128, 96_000), (256, 96_000)] {
                policy.enter(frames, rate).expect("scheduling policy");
                if policy.mode() == SchedulingMode::RealTime && cfg!(target_os = "macos") {
                    assert!(
                        AudioThreadPolicy::new().is_err(),
                        "a second guard cannot restore this thread's active time constraint"
                    );
                }
                policy
                    .enter(frames, rate)
                    .expect("cached scheduling policy");
                policy.leave().expect("ordinary scheduling");
                policy.leave().expect("already ordinary");
            }
        })
        .join()
        .expect("thread joined");
    }
}
