//! A fixed-size latency histogram for long soaks: allocated once, then updated without
//! allocation on the real-time thread.

/// Counts values in fixed-width buckets, plus one overflow bucket, keeping the exact maximum.
pub struct Histogram {
    width: u64,
    buckets: Box<[u64]>,
    count: u64,
    max: u64,
}

impl Histogram {
    /// `buckets` buckets of `width` each; larger values land in an overflow bucket.
    ///
    /// # Panics
    /// If `width` or `buckets` is zero.
    #[must_use]
    pub fn new(width: u64, buckets: usize) -> Self {
        assert!(width > 0 && buckets > 0, "a histogram needs buckets");
        Self {
            width,
            buckets: vec![0; buckets + 1].into_boxed_slice(),
            count: 0,
            max: 0,
        }
    }

    /// Records one value. Real-time safe.
    pub fn record(&mut self, value: u64) {
        let last = self.buckets.len() - 1;
        let index = usize::try_from(value / self.width).map_or(last, |index| index.min(last));
        self.buckets[index] += 1;
        self.count += 1;
        self.max = self.max.max(value);
    }

    /// Values recorded.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The largest value recorded.
    #[must_use]
    pub fn max(&self) -> u64 {
        self.max
    }

    /// The upper edge of the bucket that holds the `quantile` (0–1) value, capped at the
    /// maximum (the maximum itself for the overflow bucket); `None` before any value.
    #[must_use]
    pub fn quantile(&self, quantile: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "a rank within the recorded count"
        )]
        let rank = ((self.count as f64 * quantile).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (index, bucket) in self.buckets[..self.buckets.len() - 1].iter().enumerate() {
            seen += bucket;
            if seen >= rank {
                let edge = (index as u64 + 1) * self.width;
                return Some(edge.min(self.max));
            }
        }
        Some(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::Histogram;

    #[test]
    fn quantiles_are_bucket_edges_capped_at_the_maximum() {
        let mut histogram = Histogram::new(10, 100);
        for value in 1..=10_000 {
            histogram.record(value % 1000);
        }
        histogram.record(5_000);
        assert_eq!(histogram.count(), 10_001);
        assert_eq!(histogram.max(), 5_000);
        assert_eq!(histogram.quantile(0.5), Some(510));
        assert_eq!(histogram.quantile(0.9999), Some(1_000));
        assert_eq!(histogram.quantile(1.0), Some(5_000));
        assert_eq!(Histogram::new(1, 1).quantile(0.5), None);
    }
}
