//! Fixed-size measurement containers for the in-game overlay. No allocation
//! after construction, cheap enough to update every frame.

/// Linear histogram of durations. Values past the last bucket are counted in
/// it, so percentiles saturate at the range instead of lying low.
pub struct Hist {
    buckets: [u32; Hist::N],
    bucket_ns: u64,
    count: u64,
    sum_ns: u64,
    max_ns: u64,
}

impl Hist {
    pub const N: usize = 512;

    pub const fn new(bucket_ns: u64) -> Self {
        Hist { buckets: [0; Hist::N], bucket_ns, count: 0, sum_ns: 0, max_ns: 0 }
    }

    #[inline]
    pub fn record(&mut self, ns: u64) {
        let i = ((ns / self.bucket_ns) as usize).min(Hist::N - 1);
        self.buckets[i] += 1;
        self.count += 1;
        self.sum_ns += ns;
        self.max_ns = self.max_ns.max(ns);
    }

    pub fn reset(&mut self) {
        *self = Hist::new(self.bucket_ns);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn max_ns(&self) -> u64 {
        self.max_ns
    }

    pub fn mean_ns(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.sum_ns / self.count
        }
    }

    pub fn bucket_ns(&self) -> u64 {
        self.bucket_ns
    }

    pub fn buckets(&self) -> &[u32; Hist::N] {
        &self.buckets
    }

    /// Upper edge of the bucket holding the `p` quantile (0.0..=1.0).
    pub fn percentile_ns(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((self.count as f64 * p).ceil() as u64).clamp(1, self.count);
        let mut acc = 0u64;
        for (i, &b) in self.buckets.iter().enumerate() {
            acc += b as u64;
            if acc >= target {
                return ((i as u64 + 1) * self.bucket_ns).min(self.max_ns.max(self.bucket_ns));
            }
        }
        self.max_ns
    }
}

/// Ring of the most recent samples, for the frame-time graph.
pub struct Ring<const N: usize> {
    data: [f32; N],
    next: usize,
    len: usize,
}

impl<const N: usize> Ring<N> {
    pub const fn new() -> Self {
        Ring { data: [0.0; N], next: 0, len: 0 }
    }

    #[inline]
    pub fn push(&mut self, v: f32) {
        self.data[self.next] = v;
        self.next = (self.next + 1) % N;
        self.len = (self.len + 1).min(N);
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `i` = 0 is the oldest retained sample.
    #[inline]
    pub fn get(&self, i: usize) -> f32 {
        self.data[(self.next + N - self.len + i) % N]
    }
}

impl<const N: usize> Default for Ring<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Running mean and standard deviation (Welford), for hit-error statistics.
#[derive(Default, Clone, Copy)]
pub struct Running {
    n: u64,
    mean: f64,
    m2: f64,
}

impl Running {
    pub fn push(&mut self, x: f64) {
        self.n += 1;
        let d = x - self.mean;
        self.mean += d / self.n as f64;
        self.m2 += d * (x - self.mean);
    }
    pub fn n(&self) -> u64 {
        self.n
    }
    pub fn mean(&self) -> f64 {
        self.mean
    }
    pub fn stddev(&self) -> f64 {
        if self.n < 2 {
            0.0
        } else {
            (self.m2 / (self.n - 1) as f64).sqrt()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_follow_the_distribution() {
        let mut h = Hist::new(100);
        for i in 0..1000u64 {
            h.record(i * 10); // 0..10_000 ns uniformly
        }
        let p50 = h.percentile_ns(0.5);
        let p99 = h.percentile_ns(0.99);
        assert!((4_900..=5_100).contains(&p50), "{p50}");
        assert!((9_800..=10_000).contains(&p99), "{p99}");
    }

    #[test]
    fn overflow_saturates_in_the_last_bucket() {
        let mut h = Hist::new(10);
        h.record(1_000_000);
        assert_eq!(h.percentile_ns(0.5), 1_000_000.min(Hist::N as u64 * 10).max(5120));
    }

    #[test]
    fn ring_keeps_the_newest_in_order() {
        let mut r = Ring::<4>::new();
        for i in 0..6 {
            r.push(i as f32);
        }
        assert_eq!((0..4).map(|i| r.get(i)).collect::<Vec<_>>(), vec![2.0, 3.0, 4.0, 5.0]);
    }
}
