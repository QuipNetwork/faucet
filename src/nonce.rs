//! Sequential nonce allocator for the dedicated **base wallet**.
//!
//! The base wallet is funded by sudo and is the only account the faucet submits
//! transfers from, so its nonce is not contended — a local fetch-and-add lets
//! `/request` transfers (and pool funding) pipeline concurrently. (The chain sudo
//! key is *not* dedicated, so its submissions fetch the nonce fresh instead; see
//! `ChainClient::submit_funder`.)

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

    /// The next nonce that *would* be allocated, without reserving it. Used by the
    /// reconcile watchdog to compare the lane against the chain's `next_index` and
    /// detect a stuck future-nonce gap.
    pub fn current(&self) -> u32 {
        self.next.load(Ordering::SeqCst) as u32
    }

    /// Reset to the chain's reported next index (after a stale rejection / drift).
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
    }

    #[test]
    fn resync_resets_the_counter() {
        let lane = NonceLane::new(5);
        assert_eq!(lane.allocate(), 5);
        lane.resync(10);
        assert_eq!(lane.allocate(), 10);
    }

    // QUI-831: after a drip is confirmed not to have landed, the lane has run
    // ahead of the chain (a future-nonce gap). Healing it to the chain's next
    // index lowers the counter so the next drip refills the gap instead of piling
    // more stranded future nonces onto the pool.
    #[test]
    fn resync_down_refills_a_future_nonce_gap() {
        let lane = NonceLane::new(5);
        assert_eq!(lane.allocate(), 5); // nonce 5 submitted but dropped
        assert_eq!(lane.allocate(), 6); // 6, 7 stranded in the future queue
        assert_eq!(lane.allocate(), 7);
        assert_eq!(lane.current(), 8); // lane ahead; chain still expects 5
        lane.resync(5); // heal to chain next_index
        assert_eq!(lane.allocate(), 5); // next drip refills the gap at 5
    }
}
