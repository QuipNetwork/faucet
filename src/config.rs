//! CLI configuration.

use std::time::Duration;

use clap::Parser;

/// Default dispense per request: 10 UNIT on 12-decimal chains (ED is 0.001 UNIT
/// and a transfer fee ~0.00076 UNIT, so this is ~13k transactions of headroom).
pub const DEFAULT_AMOUNT_PLANCKS: u128 = 10_000_000_000_000;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "quip-faucet",
    about = "Concurrent dev faucet for Quip substrate chains"
)]
pub struct Config {
    /// Substrate node WebSocket URL. Repeat to add ordered failover nodes.
    #[arg(long = "node-url", required = true)]
    pub node_urls: Vec<String>,

    /// Funder SURI (the chain sudo key). Pool accounts derive from it.
    #[arg(long, default_value = "//Alice", env = "QUIP_FAUCET_FAUCET_KEY")]
    pub faucet_key: String,

    #[arg(long, default_value = "127.0.0.1")]
    pub listen_host: String,
    #[arg(long, default_value_t = 8087)]
    pub port: u16,

    /// Default funding amount in plancks (overridable per request).
    #[arg(long, default_value_t = DEFAULT_AMOUNT_PLANCKS)]
    pub amount: u128,

    /// Deny when the destination's free balance exceeds this. Defaults to one
    /// dispense: an account already holding a full hand-out doesn't need more,
    /// while dust (e.g. the existential deposit) stays eligible. Set 0 to deny
    /// any account holding funds.
    #[arg(long = "max-funded-balance-plancks", default_value_t = DEFAULT_AMOUNT_PLANCKS)]
    pub max_funded_balance: u128,

    /// Strict fallback window per destination (used when the balance query fails).
    #[arg(long, default_value_t = 60.0)]
    pub rate_limit_seconds: f64,
    /// Lenient window for confirmed-empty destinations (>= block time).
    #[arg(long, default_value_t = 5.0)]
    pub lenient_rate_limit_seconds: f64,
    /// On a balance-query failure, deny (503) instead of falling back to strict.
    #[arg(long, default_value_t = false)]
    pub balance_query_fail_closed: bool,

    /// Pre-funded `/sign` pool size. 0 disables the pool (`/sign` returns 503).
    #[arg(long, default_value_t = 8)]
    pub pool_size: u32,
    #[arg(long, default_value_t = 64)]
    pub pool_max_size: u32,
    #[arg(long, default_value_t = 100 * DEFAULT_AMOUNT_PLANCKS)]
    pub pool_fund_amount: u128,
    #[arg(long, default_value_t = 10 * DEFAULT_AMOUNT_PLANCKS)]
    pub pool_low_watermark: u128,
    #[arg(long, default_value_t = 20.0)]
    pub pool_cooldown_seconds: f64,
    #[arg(long, default_value_t = 30.0)]
    pub pool_replenish_interval_seconds: f64,
    #[arg(long, default_value_t = 30.0)]
    pub pool_idle_grow_seconds: f64,

    /// Top up the base wallet (via sudo) when its balance drops below this many
    /// dispenses of runway (`base_balance < base_min_txns * amount`).
    #[arg(long, default_value_t = 1000)]
    pub base_min_txns: u64,
    /// Top-up target for the base wallet, in dispenses (must exceed base-min-txns).
    #[arg(long, default_value_t = 10_000)]
    pub base_target_txns: u64,
    /// Seconds between base-wallet balance checks / top-ups.
    #[arg(long, default_value_t = 60.0)]
    pub base_monitor_interval_seconds: f64,

    /// Seconds between base-wallet nonce-lane reconcile checks (self-heal a stuck
    /// future-nonce gap; see `spawn_base_nonce_reconcile`).
    #[arg(long, default_value_t = 20.0)]
    pub base_nonce_reconcile_interval_seconds: f64,
    /// Consecutive reconcile checks showing no on-chain nonce progress while the lane
    /// sits ahead, before the lane is resynced. `checks * interval` must exceed
    /// inclusion latency so normal pipelining never trips it (default 3 * 20s = 60s).
    #[arg(long, default_value_t = 3)]
    pub base_nonce_stall_checks: u32,

    /// Allow running against non-dev chains (skips the startup name guard). UNSAFE.
    #[arg(long, default_value_t = false)]
    pub allow_any_chain: bool,
}

impl Config {
    pub fn lenient_window(&self) -> Duration {
        Duration::from_secs_f64(self.lenient_rate_limit_seconds)
    }
    pub fn strict_window(&self) -> Duration {
        Duration::from_secs_f64(self.rate_limit_seconds)
    }
    pub fn pool_cooldown(&self) -> Duration {
        Duration::from_secs_f64(self.pool_cooldown_seconds)
    }
    pub fn idle_grow(&self) -> Duration {
        Duration::from_secs_f64(self.pool_idle_grow_seconds)
    }
    pub fn balance_query_fail_open(&self) -> bool {
        !self.balance_query_fail_closed
    }
    /// Top up the base wallet when its balance falls below this.
    pub fn base_topup_threshold(&self) -> u128 {
        self.amount.saturating_mul(u128::from(self.base_min_txns))
    }
    /// Refill the base wallet up to this on top-up.
    pub fn base_topup_target(&self) -> u128 {
        self.amount
            .saturating_mul(u128::from(self.base_target_txns))
    }
    pub fn base_monitor_interval(&self) -> Duration {
        Duration::from_secs_f64(self.base_monitor_interval_seconds)
    }
    pub fn base_nonce_reconcile_interval(&self) -> Duration {
        Duration::from_secs_f64(self.base_nonce_reconcile_interval_seconds)
    }
}
