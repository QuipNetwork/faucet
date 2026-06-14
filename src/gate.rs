//! Balance gate + two-tier rate limiting (Goals 3 & 4).
//!
//! Denies funded destinations and lightly throttles empty ones, with a per-
//! destination in-flight reservation that stops two concurrent requests from both
//! funding the same empty account.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use quip_protocol_runtime::AccountId;

use crate::chain::ChainClient;

#[derive(Debug)]
pub enum GateDecision {
    /// Allowed — `dest_key` stays reserved; the caller MUST `release` it.
    Allow,
    RateLimited {
        retry_after: f64,
    },
    InFlight,
    Funded {
        free: u128,
    },
    Degraded {
        retry_after: f64,
    },
    Unavailable,
}

pub struct Gate {
    last_funded: Mutex<HashMap<String, Instant>>,
    in_flight: Mutex<HashSet<String>>,
    lenient: Duration,
    strict: Duration,
    max_funded: u128,
    fail_open: bool,
}

impl Gate {
    pub fn new(lenient: Duration, strict: Duration, max_funded: u128, fail_open: bool) -> Self {
        Self {
            last_funded: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            lenient,
            strict,
            max_funded,
            fail_open,
        }
    }

    /// Reserve `dest_key` and decide. Locks are always released before the async
    /// balance query (no `await` holding a lock).
    pub async fn check(
        &self,
        dest_key: &str,
        dest: &AccountId,
        chain: &ChainClient,
    ) -> GateDecision {
        let now = Instant::now();

        // Cheap in-memory lenient pre-check.
        {
            let last_funded = self.last_funded.lock();
            if let Some(funded_at) = last_funded.get(dest_key) {
                let elapsed = now.duration_since(*funded_at);
                if elapsed < self.lenient {
                    return GateDecision::RateLimited {
                        retry_after: (self.lenient - elapsed).as_secs_f64(),
                    };
                }
            }
        }
        // Reserve.
        {
            let mut in_flight = self.in_flight.lock();
            if in_flight.contains(dest_key) {
                return GateDecision::InFlight;
            }
            in_flight.insert(dest_key.to_owned());
        }

        match chain.free_balance(dest).await {
            Ok(free) => {
                if free > self.max_funded {
                    self.release(dest_key);
                    GateDecision::Funded { free }
                } else {
                    GateDecision::Allow
                }
            }
            Err(_) => {
                if !self.fail_open {
                    self.release(dest_key);
                    return GateDecision::Unavailable;
                }
                // fail-open: fall back to the strict window.
                let degraded = {
                    let last_funded = self.last_funded.lock();
                    last_funded.get(dest_key).and_then(|funded_at| {
                        let elapsed = now.duration_since(*funded_at);
                        (elapsed < self.strict).then(|| (self.strict - elapsed).as_secs_f64())
                    })
                };
                match degraded {
                    Some(retry_after) => {
                        self.release(dest_key);
                        GateDecision::Degraded { retry_after }
                    }
                    None => GateDecision::Allow,
                }
            }
        }
    }

    pub fn release(&self, dest_key: &str) {
        let _ = self.in_flight.lock().remove(dest_key);
    }

    pub fn commit(&self, dest_key: &str) {
        let _ = self
            .last_funded
            .lock()
            .insert(dest_key.to_owned(), Instant::now());
    }
}
