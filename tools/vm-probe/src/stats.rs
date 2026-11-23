//! Round-trip statistics: a histogram in whole microseconds and the report.

use serde::Serialize;
use std::time::Duration;

/// Round trips longer than this go into the last bucket.
const BUCKETS_US: usize = 10_000;

/// Counts of round trips by whole microsecond.
pub(crate) struct Histogram {
    counts: Vec<u64>,
    total: u64,
    max: Duration,
}

impl Histogram {
    pub(crate) fn new() -> Self {
        Self {
            counts: vec![0; BUCKETS_US + 1],
            total: 0,
            max: Duration::ZERO,
        }
    }

    pub(crate) fn record(&mut self, round_trip: Duration) {
        let bucket = usize::try_from(round_trip.as_micros())
            .unwrap_or(BUCKETS_US)
            .min(BUCKETS_US);
        self.counts[bucket] += 1;
        self.total += 1;
        self.max = self.max.max(round_trip);
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// The smallest whole microsecond at or above `fraction` of the round trips. The last
    /// bucket holds every longer round trip, so 10 000 means "10 ms or more"; the maximum
    /// has the true worst value.
    pub(crate) fn percentile_us(&self, fraction: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "a rank within a u64 count"
        )]
        let rank = ((self.total as f64) * fraction).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (bucket, count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return bucket as u64;
            }
        }
        BUCKETS_US as u64
    }

    /// The median, the 99.99th percentile and the maximum, for a progress line.
    pub(crate) fn summary(&self) -> String {
        format!(
            "p50 {:>4} us  p99.99 {:>5} us  max {:>7.1} us",
            self.percentile_us(0.5),
            self.percentile_us(0.9999),
            self.max_us()
        )
    }

    pub(crate) fn max_us(&self) -> f64 {
        self.max.as_secs_f64() * 1e6
    }

    /// The non-empty buckets as `[microseconds, count]`.
    pub(crate) fn nonzero(&self) -> Vec<[u64; 2]> {
        self.counts
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(bucket, count)| [bucket as u64, *count])
            .collect()
    }
}

/// The result of one host run.
#[derive(Serialize)]
pub(crate) struct Report {
    pub frames: u32,
    pub sample_rate: u32,
    pub window_us: f64,
    pub realtime: bool,
    pub cpu: Option<usize>,
    pub blocks: u64,
    /// Round trips longer than the window.
    pub misses: u64,
    /// Requests with no completion within the hang limit. The run stops at the first.
    pub lost: u64,
    /// Blocks that the host itself started late (the host, not the guest, was slow).
    pub host_overruns: u64,
    /// Doorbell rings that failed.
    pub ring_failures: u64,
    pub p50_us: u64,
    pub p99_us: u64,
    pub p99_99_us: u64,
    pub max_us: f64,
    pub histogram_us: Vec<[u64; 2]>,
}
