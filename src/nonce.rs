//! Per-account sequential nonce allocator.
//!
//! Lets the faucet pipeline many extrinsics it submits itself (the funder's
//! sudo-mints) with sequential nonces and concurrent submission — the critical
//! section is a single fetch-and-add, never held across submit or inclusion. This
//! is the concurrency fix for `/request`: throughput is no longer one-per-block.
//!
//! NOT used for `/sign` (the receiver submits there, so a local counter could gap-
//! stall; that path re-fetches the on-chain nonce per handout instead).

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub struct NonceLane {
    next: AtomicU64,
}

impl NonceLane {
    pub fn new(seed: u32) -> Self {
        Self {
            next: AtomicU64::new(u64::from(seed)),
        }
    }

    /// Reserve the next nonce.
    pub fn allocate(&self) -> u32 {
        self.next.fetch_add(1, Ordering::SeqCst) as u32
    }

    /// Reset to the chain's reported next index (after a submit error / drift).
    pub fn resync(&self, chain_next: u32) {
        self.next.store(u64::from(chain_next), Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::NonceLane;

    #[test]
    fn allocates_sequentially_from_seed() {
        let lane = NonceLane::new(5);
        assert_eq!(lane.allocate(), 5);
        assert_eq!(lane.allocate(), 6);
        assert_eq!(lane.allocate(), 7);
    }

    #[test]
    fn resync_resets_the_counter() {
        let lane = NonceLane::new(5);
        assert_eq!(lane.allocate(), 5);
        lane.resync(10);
        assert_eq!(lane.allocate(), 10);
    }
}
