//! Concurrent dev faucet for Quip substrate chains.
//!
//! tokio + jsonrpsee (multiplexed RPC, no global lock) + per-account nonce lanes,
//! reusing the Quip runtime/crypto/client crates so the wire format never drifts.

mod calls;
mod chain;
mod config;
mod gate;
mod handlers;
mod nonce;
mod pool;
mod signer;

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{
    routing::{get, post},
    Router,
};
use clap::Parser;
use quip_protocol_runtime::AccountId;
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

pub struct AppState {
    pub cfg: Config,
    pub chain: ChainClient,
    pub funder: Funder,
    pub funder_nonce: NonceLane,
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
    let funder = Funder::from_suri(&cfg.faucet_key).context("loading funder key")?;
    info!("funder: {}", funder.account.to_ss58check());

    let chain = ChainClient::connect(
        cfg.node_urls.clone(),
        funder.account.clone(),
        cfg.allow_any_chain,
    )
    .await?;
    let funder_next = chain.next_index(&funder.account).await?;

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
        funder_nonce: NonceLane::new(funder_next),
        gate,
        pool,
    });

    init_pool(&state).await?;
    spawn_refresh(Arc::clone(&state));
    spawn_replenish(Arc::clone(&state));

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

/// Sudo-mint `pool_fund_amount` to `account` and wait until it is on-chain.
async fn fund_account(state: &Arc<AppState>, account: &AccountId) -> Result<()> {
    let nonce = state.funder_nonce.allocate();
    let call = calls::sudo_mint(account.clone(), state.cfg.pool_fund_amount);
    state
        .chain
        .submit(&state.funder.pair, call, nonce)
        .await
        .context("funding pool account")?;
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
