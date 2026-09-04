pub struct BloomFilter {
    bits: Vec<u64>,
    num_bits: u64,
    num_probes: u32,
}

impl BloomFilter {
    pub fn new(num_keys: usize, bits_per_key: usize) -> Self {
        let requested = num_keys.saturating_mul(bits_per_key).max(64);
        let words = requested.div_ceil(64);
        let num_bits = (words * 64) as u64;
        let num_probes =
            ((bits_per_key as f64 * std::f64::consts::LN_2).round() as u32).clamp(1, 30);

        Self {
            bits: vec![0; words],
            num_bits,
            num_probes,
        }
    }

    pub fn from_parts(bits: Vec<u64>, num_bits: u64, num_probes: u32) -> Self {
        Self {
            bits,
            num_bits,
            num_probes,
        }
    }

    pub fn num_bits(&self) -> u64 {
        self.num_bits
    }

    pub fn num_probes(&self) -> u32 {
        self.num_probes
    }

    pub fn words(&self) -> &[u64] {
        &self.bits
    }

    pub fn insert(&mut self, hash: u64) {
        for i in 0..self.num_probes {
            let bit = self.probe(hash, i);
            self.bits[(bit / 64) as usize] |= 1u64 << (bit % 64);
        }
    }

    pub fn contains(&self, hash: u64) -> bool {
        (0..self.num_probes).all(|i| {
            let bit = self.probe(hash, i);
            (self.bits[(bit / 64) as usize] >> (bit % 64)) & 1 == 1
        })
    }

    fn probe(&self, hash: u64, i: u32) -> u64 {
        let start = (hash as u32) as u64;
        let stride = (((hash >> 32) as u32) | 1) as u64;
        (start + i as u64 * stride) % self.num_bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash64;

    fn key(i: usize) -> Vec<u8> {
        format!("key{:08}", i).into_bytes()
    }

    #[test]
    fn inserted_keys_are_always_found() {
        let mut filter = BloomFilter::new(1000, 10);
        for i in 0..1000 {
            filter.insert(hash64(&key(i)));
        }
        for i in 0..1000 {
            assert!(filter.contains(hash64(&key(i))), "lost key {i}");
        }
    }

    #[test]
    fn empty_filter_contains_nothing() {
        let filter = BloomFilter::new(1000, 10);
        for i in 0..1000 {
            assert!(!filter.contains(hash64(&key(i))));
        }
    }

    #[test]
    fn zero_keys_is_usable() {
        let mut filter = BloomFilter::new(0, 10);
        assert!(!filter.contains(hash64(b"anything")));
        filter.insert(hash64(b"anything"));
        assert!(filter.contains(hash64(b"anything")));
    }

    #[test]
    fn probe_count_follows_bits_per_key() {
        assert_eq!(BloomFilter::new(10, 10).num_probes, 7);
        assert_eq!(BloomFilter::new(10, 4).num_probes, 3);
        assert_eq!(BloomFilter::new(10, 1).num_probes, 1);
        assert_eq!(BloomFilter::new(10, 0).num_probes, 1);
    }

    #[test]
    fn false_positive_rate_is_near_one_percent() {
        const N: usize = 10_000;
        let mut filter = BloomFilter::new(N, 10);
        for i in 0..N {
            filter.insert(hash64(&key(i)));
        }

        let positives = (N..N * 2)
            .filter(|&i| filter.contains(hash64(&key(i))))
            .count();
        let rate = positives as f64 / N as f64;

        assert!(rate < 0.03, "false positive rate {rate} is too high");
    }
}
