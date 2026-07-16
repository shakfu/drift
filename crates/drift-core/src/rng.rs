//! Deterministic RNG wrapper.
//!
//! The whole simulation must be reproducible: same seed -> byte-identical run.
//! That rules out `thread_rng`/wall-clock entropy. We wrap a seedable ChaCha8
//! stream, whose internal state serializes, so a dumped world can be resumed and
//! still match a fresh run from the same seed.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

/// Seedable, serializable PRNG used everywhere the simulation needs randomness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetRng {
    inner: ChaCha8Rng,
}

impl DetRng {
    /// Construct from a 64-bit seed. Two `DetRng`s with the same seed produce
    /// identical sequences.
    pub fn from_seed(seed: u64) -> Self {
        Self {
            inner: ChaCha8Rng::seed_from_u64(seed),
        }
    }

    /// Uniform integer in `[low, high)`. Panics if `low >= high`, matching
    /// `rand`'s range contract.
    pub fn range_u32(&mut self, low: u32, high: u32) -> u32 {
        self.inner.gen_range(low..high)
    }

    /// Uniform integer in `[low, high)` over `usize`.
    pub fn range_usize(&mut self, low: usize, high: usize) -> usize {
        self.inner.gen_range(low..high)
    }

    /// Uniform `f64` in `[0, 1)`.
    pub fn unit_f64(&mut self) -> f64 {
        self.inner.gen::<f64>()
    }

    /// A full 64-bit draw — used to seed an independent child RNG deterministically
    /// (e.g. one per running battle, so a fight's evolution is isolated from what
    /// else happens between ticks).
    pub fn next_u64(&mut self) -> u64 {
        self.inner.gen::<u64>()
    }

    /// Deterministic tie-break helper: pick an index in `[0, len)`, or `None`
    /// when `len == 0`.
    pub fn choose_index(&mut self, len: usize) -> Option<usize> {
        if len == 0 {
            None
        } else {
            Some(self.range_usize(0, len))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = DetRng::from_seed(42);
        let mut b = DetRng::from_seed(42);
        for _ in 0..1000 {
            assert_eq!(a.range_u32(0, 1_000_000), b.range_u32(0, 1_000_000));
        }
    }

    #[test]
    fn different_seed_diverges() {
        let mut a = DetRng::from_seed(1);
        let mut b = DetRng::from_seed(2);
        let sa: Vec<u32> = (0..64).map(|_| a.range_u32(0, u32::MAX)).collect();
        let sb: Vec<u32> = (0..64).map(|_| b.range_u32(0, u32::MAX)).collect();
        assert_ne!(sa, sb);
    }

    #[test]
    fn serialized_state_resumes_identically() {
        let mut a = DetRng::from_seed(7);
        for _ in 0..10 {
            a.range_u32(0, 100);
        }
        // Round-trip the mid-stream state through JSON, then prove the restored
        // RNG produces the identical continuation. This is what makes a dumped
        // world resumable without breaking determinism.
        let text = serde_json::to_string(&a).expect("serialize DetRng");
        let mut b: DetRng = serde_json::from_str(&text).expect("deserialize DetRng");
        for _ in 0..100 {
            assert_eq!(a.range_u32(0, 100), b.range_u32(0, 100));
        }
    }
}
