//! Concurrent dev faucet for Quip substrate chains.
//!
//! tokio + jsonrpsee (multiplexed RPC, no global lock), reusing the Quip
//! runtime/crypto/client crates so the wire format never drifts. A dedicated base
//! wallet (sudo-topped-up) funds /request and the pool via a nonce lane, so only
//! rare top-ups touch the shared, contended sudo key.

mod calls;
mod chain;
mod config;
mod gate;
mod handlers;
mod nonce;
mod pool;
mod signer;

use std::{sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use axum::{
    routing::{get, post},
    Router,
};
use clap::Parser;
use quip_protocol_runtime::AccountId;
use quip_transaction_crypto::HybridPair;
use sp_core::crypto::Ss58Codec;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    chain::ChainClient,
    config::Config,
    gate::Gate,
    nonce::NonceLane,
    pool::{Pool, PoolAccount},
    signer::Funder,
};

/// Existential deposit (= MILLI_UNIT in the runtime). Pinned alongside the linked
/// runtime version; pool accounts must stay above it for `transfer_keep_alive`.
const EXISTENTIAL_DEPOSIT_PLANCKS: u128 = 1_000_000_000;

/// Dedicated hot wallet that funds users; sudo-topped-up, with a faucet-only nonce
/// lane so `/request` and pool funding pipeline without touching the sudo key.
pub struct BaseWallet {
    pub pair: HybridPair,
    pub account: AccountId,
    pub nonce: NonceLane,
}

pub struct AppState {
    pub cfg: Config,
    pub chain: ChainClient,
    pub funder: Funder,
    pub base: BaseWallet,
    pub gate: Gate,
    pub pool: Pool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = Config::parse();
    if cfg.allow_any_chain {
        warn!(
            "allow-any-chain set: dev-chain guard disabled; unsafe outside controlled environments"
        );
    }
    let funder = Funder::from_suri(&cfg.faucet_key).context("loading funder key")?;
    info!("funder: {}", funder.account.to_ss58check());

    let chain = ChainClient::connect(
        cfg.node_urls.clone(),
        funder.account.clone(),
        cfg.allow_any_chain,
    )
    .await?;

    // Verify the funder can actually mint before we touch anything on chain.
    ensure_funder_is_sudo(&chain, &funder).await?;

    // Dedicated base wallet: sudo-topped-up, funds /request + pool via a nonce lane.
    let (base_pair, base_account) = funder.derive_base()?;
    info!("base wallet: {}", base_account.to_ss58check());
    ensure_base_funded(&chain, &funder, &base_account, &cfg).await?;
    let base_next = chain.next_index(&base_account).await?;
    let base = BaseWallet {
        pair: base_pair,
        account: base_account,
        nonce: NonceLane::new(base_next),
    };

    let gate = Gate::new(
        cfg.lenient_window(),
        cfg.strict_window(),
        cfg.max_funded_balance,
        cfg.balance_query_fail_open(),
    );
    let pool = Pool::new(EXISTENTIAL_DEPOSIT_PLANCKS, cfg.pool_cooldown());

    let state = Arc::new(AppState {
        cfg,
        chain,
        funder,
        base,
        gate,
        pool,
    });

    init_pool(&state).await?;
    spawn_refresh(Arc::clone(&state));
    spawn_replenish(Arc::clone(&state));
    spawn_base_monitor(Arc::clone(&state));
    spawn_base_nonce_reconcile(Arc::clone(&state));

    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/request", post(handlers::request))
        .route("/sign", post(handlers::sign))
        .with_state(Arc::clone(&state));

    let addr = format!("{}:{}", state.cfg.listen_host, state.cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    info!(
        "faucet listening on http://{addr} pool={}",
        state.pool.len()
    );
    axum::serve(listener, app).await.context("serving")?;
    Ok(())
}

/// Derive + fund the pool before binding, then adopt any contiguous funded prefix
/// above the base size (a previously-grown pool).
async fn init_pool(state: &Arc<AppState>) -> Result<()> {
    if state.cfg.pool_size == 0 {
        // Pool disabled: skip both creation and the grown-prefix adoption scan
        // below (which starts at `pool_size` and would otherwise adopt any
        // pre-funded pool accounts). `/sign` returns 503 with an empty pool.
        info!("/sign pool disabled (--pool-size 0)");
        return Ok(());
    }
    for index in 0..state.cfg.pool_size {
        ensure_pool_account(state, index).await?;
    }
    let mut index = state.cfg.pool_size;
    while index < state.cfg.pool_max_size {
        let (pair, account) = state.funder.derive_pool(index)?;
        let free = state.chain.free_balance(&account).await?;
        if free <= EXISTENTIAL_DEPOSIT_PLANCKS {
            break; // first unfunded index ends the grown prefix
        }
        let tracked_free = if free < state.cfg.pool_low_watermark {
            fund_account(state, &account).await?;
            free.saturating_add(state.cfg.pool_fund_amount)
        } else {
            free
        };
        state.pool.push(PoolAccount {
            index,
            pair,
            account,
            tracked_free,
            last_handed_out: None,
            last_handed_nonce: None,
            in_flight: false,
        });
        index += 1;
    }
    info!("pool ready: {} accounts", state.pool.len());
    Ok(())
}

async fn ensure_pool_account(state: &Arc<AppState>, index: u32) -> Result<()> {
    let (pair, account) = state.funder.derive_pool(index)?;
    let mut tracked_free = state.chain.free_balance(&account).await?;
    if tracked_free < state.cfg.pool_low_watermark {
        fund_account(state, &account).await?;
        tracked_free = tracked_free.saturating_add(state.cfg.pool_fund_amount);
    }
    state.pool.push(PoolAccount {
        index,
        pair,
        account,
        tracked_free,
        last_handed_out: None,
        last_handed_nonce: None,
        in_flight: false,
    });
    Ok(())
}

/// Fund `account` by transferring `pool_fund_amount` from the base wallet, and wait
/// until it is on-chain.
async fn fund_account(state: &Arc<AppState>, account: &AccountId) -> Result<()> {
    let call = calls::transfer_keep_alive(account.clone(), state.cfg.pool_fund_amount);
    state
        .chain
        .submit_lane(
            &state.base.pair,
            &state.base.account,
            &state.base.nonce,
            call,
        )
        .await
        .context("funding pool account from base")?;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if state.chain.free_balance(account).await.unwrap_or(0) >= state.cfg.pool_fund_amount {
            return Ok(());
        }
    }
    warn!(
        "pool funding not confirmed in 30s for {}",
        account.to_ss58check()
    );
    Ok(())
}

/// Fail fast if the funder is not the chain's sudo key. Every dispense flows
/// through `Sudo::sudo(FaucetOps::mint)` (to top up the base wallet, which then
/// transfers to users), and the runtime authorizes that only for `Sudo::Key`. A
/// wrong key makes the node accept each extrinsic into its pool and then drop it,
/// so `/request` returns `200` yet nothing ever lands — the silent failure this
/// guards against. Crashing at startup makes the misconfiguration loud instead.
async fn ensure_funder_is_sudo(chain: &ChainClient, funder: &Funder) -> Result<()> {
    match chain.sudo_key().await.context("reading chain Sudo.Key")? {
        Some(key) if key == funder.account => {
            info!("funder confirmed as chain sudo key");
            Ok(())
        }
        Some(key) => bail!(
            "funder {} is NOT the chain sudo key (Sudo.Key = {}); every mint would be \
             accepted into the tx pool then dropped. Set --faucet-key / \
             QUIP_FAUCET_FAUCET_KEY to the sudo account.",
            funder.account.to_ss58check(),
            key.to_ss58check(),
        ),
        None => bail!(
            "chain reports no Sudo.Key; this faucet mints via Sudo::sudo and cannot operate \
             against it. Verify the --node-url target."
        ),
    }
}

/// Ensure the base wallet has runway: if its balance is below the top-up threshold
/// (cold start or drained), sudo-mint up to the target. Reused at startup and by
/// the background monitor — the only place the contended sudo key is used.
async fn ensure_base_funded(
    chain: &ChainClient,
    funder: &Funder,
    base_account: &AccountId,
    cfg: &Config,
) -> Result<()> {
    let balance = chain.free_balance(base_account).await?;
    let threshold = cfg.base_topup_threshold();
    if balance >= threshold {
        return Ok(());
    }
    let target = cfg.base_topup_target();
    let topup = target.saturating_sub(balance);
    info!("base wallet low ({balance} < {threshold}); sudo-minting {topup} to reach {target}");
    let call = calls::sudo_mint(base_account.clone(), topup);
    chain
        .submit_funder(&funder.pair, &funder.account, call)
        .await
        .context("topping up base wallet")?;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if chain.free_balance(base_account).await.unwrap_or(0) >= threshold {
            return Ok(());
        }
    }
    warn!("base top-up not confirmed in 30s");
    Ok(())
}

fn spawn_base_monitor(state: Arc<AppState>) {
    let interval = state.cfg.base_monitor_interval();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if let Err(err) =
                ensure_base_funded(&state.chain, &state.funder, &state.base.account, &state.cfg)
                    .await
            {
                warn!("base wallet monitor failed: {err:#}");
            }
        }
    });
}

fn spawn_refresh(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(err) = state.chain.refresh().await {
                warn!("chain context refresh failed: {err:#}");
            }
        }
    });
}

/// Self-heal the base-wallet nonce lane from a stuck future-nonce gap.
///
/// `ChainClient::submit_lane` is fire-and-forget (`author_submitExtrinsic`) and only
/// resyncs the lane on a *stale* rejection (nonce too low). A *future*-nonce gap — the
/// lane running ahead of the chain after a submitted tx was accepted into the pool then
/// dropped (e.g. a node WS reconnect) — is never rejected: every later transfer is
/// accepted into the pool's *future* queue (so `/request` returns 200 and logs `funded`)
/// but is never included, and the lane never recovers. The faucet then 200s indefinitely
/// while delivering nothing. This watchdog detects that and resyncs the lane to chain.
///
/// It acts only on a *sustained* stall: the chain's base `next_index` shows no progress
/// for `base_nonce_stall_checks` consecutive intervals while the lane sits ahead of it.
/// Normal pipelining (the lane briefly ahead of in-flight, soon-included txs) advances
/// the chain nonce within a block or two and resets the counter, so steady load never
/// trips it. (A more thorough alternative is to confirm inclusion via
/// `author_submitAndWatchExtrinsic` in `submit_lane`; this watchdog is the minimal,
/// off-the-hot-path fix.)
fn spawn_base_nonce_reconcile(state: Arc<AppState>) {
    let interval = state.cfg.base_nonce_reconcile_interval();
    let stall_limit = state.cfg.base_nonce_stall_checks;
    tokio::spawn(async move {
        let mut last_chain_next: Option<u32> = None;
        let mut stalled: u32 = 0;
        loop {
            tokio::time::sleep(interval).await;
            let chain_next = match state.chain.next_index(&state.base.account).await {
                Ok(n) => n,
                Err(err) => {
                    warn!("base nonce reconcile: next_index failed: {err:#}");
                    continue;
                }
            };
            let lane = state.base.nonce.current();
            let Some(prev) = last_chain_next.replace(chain_next) else {
                continue; // first tick only establishes a baseline
            };
            if chain_next > prev || lane <= chain_next {
                // Inclusions are advancing the on-chain nonce (healthy), or the lane
                // isn't ahead (nothing pending). Not a stall.
                stalled = 0;
                continue;
            }
            // No on-chain progress this interval while the lane is ahead → submitted
            // transfers aren't landing.
            stalled += 1;
            warn!(
                "base nonce stalled {stalled}/{stall_limit}: chain_next={chain_next} \
                 lane={lane} (transfers submitted but not included)"
            );
            if stalled >= stall_limit {
                warn!(
                    "base nonce lane stuck; resyncing lane {lane} -> chain next_index {chain_next}"
                );
                state.base.nonce.resync(chain_next);
                stalled = 0;
            }
        }
    });
}

fn spawn_replenish(state: Arc<AppState>) {
    let interval = Duration::from_secs_f64(state.cfg.pool_replenish_interval_seconds);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if let Err(err) = replenish_once(&state).await {
                warn!("pool replenish cycle failed: {err:#}");
            }
        }
    });
}

async fn replenish_once(state: &Arc<AppState>) -> Result<()> {
    for (index, account) in state.pool.all() {
        match state.chain.free_balance(&account).await {
            Ok(free) => {
                state.pool.set_tracked(index, free);
                if free < state.cfg.pool_low_watermark {
                    fund_account(state, &account).await?;
                    state.pool.add_tracked(index, state.cfg.pool_fund_amount);
                }
            }
            Err(err) => warn!("reconcile failed for {}: {err:#}", account.to_ss58check()),
        }
    }
    maybe_grow(state).await
}

/// Double the pool when nonce reuse was observed and the faucet is idle.
async fn maybe_grow(state: &Arc<AppState>) -> Result<()> {
    if !state.pool.grow_pending() {
        return Ok(());
    }
    let idle = state
        .pool
        .last_activity()
        .is_none_or(|at| at.elapsed() >= state.cfg.idle_grow());
    if !idle {
        return Ok(());
    }
    let current = state.pool.len() as u32;
    if current >= state.cfg.pool_max_size {
        state.pool.clear_growth();
        return Ok(());
    }
    let new_size = (current.saturating_mul(2)).min(state.cfg.pool_max_size);
    for index in current..new_size {
        ensure_pool_account(state, index).await?;
    }
    state.pool.clear_growth();
    info!("pool grown {current} -> {}", state.pool.len());
    Ok(())
}
