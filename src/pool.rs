//! Pre-funded pool backing `/sign`.
//!
//! Pure state + allocation policy; the funding/replenish/grow *orchestration*
//! (which needs the chain) lives in `main`. Allocation is round-robin from a
//! cursor, skipping in-flight / under-funded / cooling-down accounts, so load —
//! and therefore nonce reuse — spreads evenly across the buffer.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use quip_protocol_runtime::{AccountId, Hash, RuntimeCall};
use quip_transaction_crypto::HybridPair;

use crate::chain::ChainClient;

pub struct PoolAccount {
    pub index: u32,
    pub pair: HybridPair,
    pub account: AccountId,
    pub tracked_free: u128,
    pub last_handed_out: Option<Instant>,
    pub last_handed_nonce: Option<u32>,
    pub in_flight: bool,
}

/// A reserved pool account returned by [`Pool::allocate`].
pub struct Allocated {
    pub index: usize,
    pub account: AccountId,
    pub last_nonce: Option<u32>,
}

pub struct Pool {
    accounts: Mutex<Vec<PoolAccount>>,
    rr_cursor: Mutex<usize>,
    grow_pending: AtomicBool,
    last_sign_activity: Mutex<Option<Instant>>,
    existential_deposit: u128,
    cooldown: Duration,
}

impl Pool {
    pub fn new(existential_deposit: u128, cooldown: Duration) -> Self {
        Self {
            accounts: Mutex::new(Vec::new()),
            rr_cursor: Mutex::new(0),
            grow_pending: AtomicBool::new(false),
            last_sign_activity: Mutex::new(None),
            existential_deposit,
            cooldown,
        }
    }

    pub fn push(&self, account: PoolAccount) {
        self.accounts.lock().push(account);
    }

    pub fn len(&self) -> usize {
        self.accounts.lock().len()
    }

    /// Reserve the next eligible account (round-robin). `None` if none is eligible.
    pub fn allocate(&self, amount: u128) -> Option<Allocated> {
        let needed = amount.saturating_add(self.existential_deposit);
        let now = Instant::now();
        let mut accounts = self.accounts.lock();
        let n = accounts.len();
        if n == 0 {
            return None;
        }
        let mut cursor = self.rr_cursor.lock();
        for offset in 0..n {
            let i = (*cursor + offset) % n;
            let candidate = &accounts[i];
            if candidate.in_flight || candidate.tracked_free < needed {
                continue;
            }
            if let Some(handed) = candidate.last_handed_out {
                if now.duration_since(handed) < self.cooldown {
                    continue;
                }
            }
            accounts[i].in_flight = true;
            *cursor = (i + 1) % n;
            return Some(Allocated {
                index: i,
                account: accounts[i].account.clone(),
                last_nonce: accounts[i].last_handed_nonce,
            });
        }
        None
    }

    /// Sign `call` from reserved account `index` (sync; no broadcast).
    pub fn sign_with(
        &self,
        index: usize,
        chain: &ChainClient,
        call: RuntimeCall,
        nonce: u32,
    ) -> Option<(String, Hash)> {
        let accounts = self.accounts.lock();
        let account = accounts.get(index)?;
        Some(chain.build_signed_hex(&account.pair, call, nonce))
    }

    /// Commit a successful hand-out: charge the balance, stamp cooldown, release.
    pub fn complete(&self, index: usize, amount: u128, nonce: u32) {
        let mut accounts = self.accounts.lock();
        if let Some(account) = accounts.get_mut(index) {
            account.last_handed_out = Some(Instant::now());
            account.tracked_free = account.tracked_free.saturating_sub(amount);
            account.last_handed_nonce = Some(nonce);
            account.in_flight = false;
        }
        *self.last_sign_activity.lock() = Some(Instant::now());
    }

    /// Release a reservation without charging (sign failed).
    pub fn release(&self, index: usize) {
        if let Some(account) = self.accounts.lock().get_mut(index) {
            account.in_flight = false;
        }
    }

    /// Latch growth if the fresh on-chain nonce did not advance past the last one
    /// handed out for this account (the buffer wrapped before the receiver submitted).
    pub fn note_nonce(&self, last_nonce: Option<u32>, fresh_nonce: u32) {
        if matches!(last_nonce, Some(last) if fresh_nonce <= last) {
            self.grow_pending.store(true, Ordering::SeqCst);
        }
    }

    pub fn grow_pending(&self) -> bool {
        self.grow_pending.load(Ordering::SeqCst)
    }

    pub fn clear_growth(&self) {
        self.grow_pending.store(false, Ordering::SeqCst);
    }

    pub fn last_activity(&self) -> Option<Instant> {
        *self.last_sign_activity.lock()
    }

    /// `(index, account)` for every pool account (for periodic reconciliation).
    pub fn all(&self) -> Vec<(usize, AccountId)> {
        self.accounts
            .lock()
            .iter()
            .enumerate()
            .map(|(i, account)| (i, account.account.clone()))
            .collect()
    }

    pub fn set_tracked(&self, index: usize, free: u128) {
        if let Some(account) = self.accounts.lock().get_mut(index) {
            account.tracked_free = free;
        }
    }

    pub fn add_tracked(&self, index: usize, delta: u128) {
        if let Some(account) = self.accounts.lock().get_mut(index) {
            account.tracked_free = account.tracked_free.saturating_add(delta);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use quip_tools::{pair_from_suri, signer_account};

    use super::{Pool, PoolAccount};

    fn push(pool: &Pool, index: u32, free: u128) {
        let pair = pair_from_suri(&format!("//Alice//pool//{index}")).expect("derive");
        let account = signer_account(&pair);
        pool.push(PoolAccount {
            index,
            pair,
            account,
            tracked_free: free,
            last_handed_out: None,
            last_handed_nonce: None,
            in_flight: false,
        });
    }

    #[test]
    fn allocate_rotates_and_reserves() {
        let pool = Pool::new(0, Duration::from_secs(20));
        for i in 0..3 {
            push(&pool, i, 10_000);
        }
        let first = pool.allocate(1_000).expect("first");
        let second = pool.allocate(1_000).expect("second");
        assert_ne!(first.index, second.index); // round-robin + reserved
    }

    #[test]
    fn allocate_skips_low_balance() {
        let pool = Pool::new(0, Duration::from_secs(0));
        push(&pool, 0, 500);
        assert!(pool.allocate(1_000).is_none());
    }

    #[test]
    fn allocate_requires_amount_plus_existential_deposit() {
        let pool = Pool::new(1, Duration::from_secs(0));
        push(&pool, 0, 1_000);
        assert!(pool.allocate(1_000).is_none()); // needs 1_000 + ed(1)
    }

    #[test]
    fn allocate_none_when_empty() {
        let pool = Pool::new(0, Duration::from_secs(0));
        assert!(pool.allocate(1_000).is_none());
    }

    #[test]
    fn note_nonce_flags_growth_only_on_reuse() {
        let advanced = Pool::new(0, Duration::from_secs(0));
        advanced.note_nonce(Some(5), 6);
        assert!(!advanced.grow_pending());

        let reused = Pool::new(0, Duration::from_secs(0));
        reused.note_nonce(Some(5), 5);
        assert!(reused.grow_pending());
    }
}
