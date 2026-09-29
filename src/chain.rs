//! Node client: connection + failover, cached context, balance query, submission.
//!
//! Uses the R2-native client helpers for build/sign/submit and wraps them with
//! multi-node failover and a cached chain context. jsonrpsee multiplexes
//! concurrent requests over one connection, so there is no global lock.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use crate::client::{
    build_signed_extrinsic, encode_extrinsic, fetch_chain_context, submit_extrinsic,
    submit_mint_extrinsic, ws_client, ChainContext,
};
use anyhow::{bail, Context, Result};
use codec::{Decode, Encode};
use jsonrpsee::{
    core::{client::ClientT, rpc_params},
    ws_client::WsClient,
};
use parking_lot::RwLock;
use quip_protocol_runtime::{AccountId, Hash, RuntimeCall};
use quip_transaction_crypto::HybridPair;
use sp_core::{
    crypto::Ss58Codec,
    hashing::{blake2_128, blake2_256, twox_128},
};
use tracing::warn;

use crate::nonce::NonceLane;

type AccountData = pallet_balances::AccountData<u128>;
type AccountInfo = frame_system::AccountInfo<u32, AccountData>;

const DEV_CHAIN_PREFIXES: [&str; 3] = ["Development", "Local Testnet", "quip-local"];

// Three refresh intervals (6s) plus 6s RPC headroom. Failed checks close
// immediately; a hung check naturally expires without flapping on each refresh.
const READINESS_TTL: Duration = Duration::from_secs(12);
#[derive(Default)]
struct Readiness {
    checked_at: Option<Instant>,
    remaining: u128,
}
impl Readiness {
    fn record(&mut self, remaining: Option<u128>) {
        self.checked_at = remaining.map(|_| Instant::now());
        self.remaining = remaining.unwrap_or(0);
    }
    fn is_ready(&self, now: Instant) -> bool {
        self.remaining > 0
            && self
                .checked_at
                .is_some_and(|at| now.duration_since(at) < READINESS_TTL)
    }
}

/// Connected node client with ordered failover.
pub struct ChainClient {
    urls: Vec<String>,
    client: RwLock<Arc<WsClient>>,
    idx: AtomicUsize,
    base_ctx: RwLock<ChainContext>,
    funder: AccountId,
    allow_any_chain: bool,
    readiness: RwLock<Readiness>,
    mint_failed: AtomicBool,
}

impl ChainClient {
    pub async fn connect(
        urls: Vec<String>,
        funder: AccountId,
        allow_any_chain: bool,
    ) -> Result<Self> {
        let (idx, client) = connect_first(&urls, allow_any_chain).await?;
        let base_ctx = fetch_chain_context(&client, &funder).await?;
        Ok(Self {
            urls,
            client: RwLock::new(Arc::new(client)),
            idx: AtomicUsize::new(idx),
            base_ctx: RwLock::new(base_ctx),
            funder,
            allow_any_chain,
            readiness: RwLock::new(Readiness::default()),
            mint_failed: AtomicBool::new(false),
        })
    }

    fn client(&self) -> Arc<WsClient> {
        self.client.read().clone()
    }

    async fn reconnect(&self) -> Result<()> {
        let n = self.urls.len();
        let start = self.idx.load(Ordering::SeqCst);
        for step in 1..=n {
            let i = (start + step) % n;
            match ws_client(&self.urls[i]).await {
                Ok(client) => {
                    if !self.allow_any_chain {
                        verify_dev_chain(&client).await?;
                    }
                    self.verify_faucet(&client).await?;
                    *self.client.write() = Arc::new(client);
                    self.idx.store(i, Ordering::SeqCst);
                    warn!("faucet failed over to node[{i}]: {}", self.urls[i]);
                    // Re-derive the cached context against the NEW node before any
                    // submission builds an extrinsic from it: the mortal era is
                    // anchored to `best_hash`/`best_number`, so reusing node[0]'s tip
                    // against node[1] can leave the tx unimportable (silently never
                    // lands). Best-effort — the 2s refresh task is the backstop.
                    match fetch_chain_context(&self.client(), &self.funder).await {
                        Ok(ctx) => *self.base_ctx.write() = ctx,
                        Err(err) => {
                            warn!("post-failover context refresh failed: {err:#}")
                        }
                    }
                    return Ok(());
                }
                Err(err) => warn!("node[{i}] reconnect failed: {err:#}"),
            }
        }
        bail!("all nodes unreachable")
    }

    /// Refresh the cached genesis/best context (a mortal era needs a recent best
    /// hash). Drive periodically from a background task.
    pub async fn refresh(&self) -> Result<()> {
        let client = self.client();
        let ctx = match fetch_chain_context(&client, &self.funder).await {
            Ok(ctx) => ctx,
            Err(error) => {
                self.readiness.write().record(None);
                return Err(error);
            }
        };
        self.verify_faucet(&client).await?;
        *self.base_ctx.write() = ctx;
        Ok(())
    }

    /// A single direct mint submission. Ambiguous receipts are never retried.
    pub async fn submit_funder(
        &self,
        signer: &HybridPair,
        account: &AccountId,
        who: AccountId,
        amount: u128,
    ) -> Result<Hash> {
        anyhow::ensure!(account == &self.funder, "unexpected funder account");
        for attempt in 0..5 {
            let client = self.client();
            let remaining = self.verify_faucet(&client).await?;
            anyhow::ensure!(
                amount > 0 && amount <= remaining,
                "top-up exceeds remaining faucet budget ({remaining}) or is zero"
            );
            // Refetch both nonce and era context on every definite stale rejection.
            let ctx = fetch_chain_context(&client, account).await?;
            let call = crate::calls::mint(who.clone(), amount);
            let bytes = encode_extrinsic(&build_signed_extrinsic(signer, call, ctx));
            match submit_mint_extrinsic(&client, &bytes, &who, amount).await {
                Ok(hash) => return Ok(hash),
                Err(error) if error.is_stale_nonce() && attempt < 4 => {
                    warn!("authority nonce rejected (attempt {attempt}); refreshing nonce");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Err(error) => {
                    if error.is_ambiguous() {
                        self.mint_failed.store(true, Ordering::SeqCst);
                        self.readiness.write().record(None);
                    }
                    return Err(error.into());
                }
            }
        }
        unreachable!("last attempt returns its result")
    }

    /// Cached finalized controls: hot paths do not repeat the storage RPCs.
    pub fn is_ready(&self) -> bool {
        !self.mint_failed.load(Ordering::SeqCst) && self.readiness.read().is_ready(Instant::now())
    }

    pub async fn ensure_faucet_ready(&self) -> Result<u128> {
        self.verify_faucet(&self.client()).await
    }

    async fn verify_faucet(&self, client: &WsClient) -> Result<u128> {
        anyhow::ensure!(
            !self.mint_failed.load(Ordering::SeqCst),
            "previous mint failed or has an unknown outcome; reconcile before restarting faucet"
        );
        let result = crate::faucet_state::check(client, &self.funder).await;
        self.readiness.write().record(result.as_ref().ok().copied());
        result
    }

    /// Submit a transfer from the dedicated base wallet, drawing the nonce from its
    /// lane (concurrent — the base wallet is faucet-only), then **confirm the drip
    /// actually landed on-chain** before reporting success. On a stale rejection,
    /// resync the lane from chain and retry.
    ///
    /// `author_submitExtrinsic` returning `Ok` only proves pool-acceptance, not
    /// inclusion: a tx accepted-then-dropped (mortal-era expiry, WS reconnect,
    /// failover) advances the lane while the chain never consumes the nonce,
    /// stranding every later drip in the pool's future queue (accepted, so the
    /// caller sees 200/`funded`, but never included). To prevent that silent
    /// failure we wait until the chain consumes the nonce; on a confirmed miss we
    /// heal the lane inline (so the next drip lands immediately, without waiting for
    /// the ~60s reconcile watchdog) and return an honest error.
    pub async fn submit_lane(
        &self,
        signer: &HybridPair,
        account: &AccountId,
        lane: &NonceLane,
        call: RuntimeCall,
        confirm_timeout: Duration,
        confirm_poll: Duration,
    ) -> Result<Hash> {
        anyhow::ensure!(self.is_ready(), "faucet controls unavailable or stale");
        let mut last_err = String::new();
        for attempt in 0..5 {
            let nonce = lane.allocate();
            let mut ctx = *self.base_ctx.read();
            ctx.nonce = nonce;
            let extrinsic = build_signed_extrinsic(signer, call.clone(), ctx);
            let bytes = encode_extrinsic(&extrinsic);
            let client = self.client();
            match submit_extrinsic(&client, &bytes).await {
                Ok(hash) => {
                    if self
                        .wait_for_base_inclusion(account, nonce, confirm_timeout, confirm_poll)
                        .await
                    {
                        return Ok(hash);
                    }
                    // Confirmed miss: the nonce was not consumed within the budget.
                    // Heal the lane so the next drip lands immediately. Only lower it
                    // on a real gap, so we don't clobber a concurrent healer's fresh
                    // allocation. We return an honest error rather than resubmitting:
                    // the inclusion signal only resolves at the deadline, so an
                    // in-request retry would just double the latency — the inline heal
                    // already restores service for the next request.
                    let onchain = self.account_nonce_onchain(account).await?;
                    if onchain > nonce {
                        return Ok(hash); // landed on the final poll boundary after all
                    }
                    if lane.current() > onchain {
                        lane.resync(onchain);
                    }
                    warn!(
                        "drip nonce {nonce} not included within {}s; healed lane to \
                         on-chain nonce {onchain}",
                        confirm_timeout.as_secs()
                    );
                    bail!(
                        "drip nonce {nonce} submitted but not included within {}s",
                        confirm_timeout.as_secs()
                    );
                }
                Err(err) => {
                    let msg = format!("{err:#}");
                    let stale = msg.contains("outdated")
                        || msg.contains("Stale")
                        || msg.contains("Priority is too low");
                    if stale && attempt < 4 {
                        let fresh = self.next_index(account).await?;
                        lane.resync(fresh);
                        last_err = msg;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                    return Err(err).context("submitting base transfer");
                }
            }
        }
        bail!("base submit stale after retries: {last_err}")
    }

    /// Poll `account`'s **on-chain** nonce until it advances past `nonce` (the chain
    /// consumed that nonce → our tx was included in a block) or `timeout` elapses.
    /// The base wallet is faucet-only (single signer), so the tx occupying a given
    /// nonce is unambiguously ours — no block scan or subscription needed. Returns
    /// `true` if inclusion was observed within the budget.
    async fn wait_for_base_inclusion(
        &self,
        account: &AccountId,
        nonce: u32,
        timeout: Duration,
        poll: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.account_nonce_onchain(account).await {
                Ok(onchain) if onchain > nonce => return true,
                Ok(_) => {}
                Err(err) => warn!("drip inclusion poll failed: {err:#}"),
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// The account's nonce as recorded in **on-chain state** at the best block.
    /// Unlike `system_accountNextIndex` (which is pool-adjusted and advances the
    /// moment a tx enters the ready queue), this only advances once a tx from the
    /// account is actually included in a block — so it is the correct signal that a
    /// drip landed, and it stays put at the gap when a future-nonce tx is stranded.
    /// Returns 0 if the account does not yet exist.
    async fn account_nonce_onchain(&self, account: &AccountId) -> Result<u32> {
        let key = account_storage_key(account);
        let client = self.client();
        let raw: Option<String> = client
            .request("state_getStorage", rpc_params![key])
            .await
            .context("querying account nonce")?;
        match raw {
            None => Ok(0),
            Some(encoded) => {
                let stripped = encoded.strip_prefix("0x").unwrap_or(&encoded);
                let bytes = hex::decode(stripped).context("decoding account storage")?;
                let info =
                    AccountInfo::decode(&mut bytes.as_slice()).context("decoding AccountInfo")?;
                Ok(info.nonce)
            }
        }
    }

    /// Build + sign `call` from `signer` with `nonce` WITHOUT submitting; returns
    /// the submittable hex and the extrinsic hash (for `/sign` hand-out).
    pub fn build_signed_hex(
        &self,
        signer: &HybridPair,
        call: RuntimeCall,
        nonce: u32,
    ) -> (String, Hash) {
        let mut ctx = *self.base_ctx.read();
        ctx.nonce = nonce;
        let extrinsic = build_signed_extrinsic(signer, call, ctx);
        let bytes = encode_extrinsic(&extrinsic);
        let hash = Hash::from(blake2_256(&bytes));
        (format!("0x{}", hex::encode(&bytes)), hash)
    }

    /// Free balance of `account` in plancks (0 if the account does not exist).
    /// Idempotent → fails over once on a transport error.
    pub async fn free_balance(&self, account: &AccountId) -> Result<u128> {
        let key = account_storage_key(account);
        for attempt in 0..2 {
            let client = self.client();
            let raw: std::result::Result<Option<String>, _> = client
                .request("state_getStorage", rpc_params![key.clone()])
                .await;
            match raw {
                Ok(None) => return Ok(0),
                Ok(Some(encoded)) => {
                    let stripped = encoded.strip_prefix("0x").unwrap_or(&encoded);
                    let bytes = hex::decode(stripped).context("decoding account storage")?;
                    let info = AccountInfo::decode(&mut bytes.as_slice())
                        .context("decoding AccountInfo")?;
                    return Ok(info.data.free);
                }
                Err(err) => {
                    if attempt == 1 {
                        return Err(err).context("querying free balance");
                    }
                    self.reconnect().await?;
                }
            }
        }
        bail!("free_balance retry exhausted")
    }

    /// The chain's next nonce for `account` (seeds nonce lanes / resync on drift).
    pub async fn next_index(&self, account: &AccountId) -> Result<u32> {
        let ss58 = account.to_ss58check();
        let client = self.client();
        let nonce: u32 = client
            .request("system_accountNextIndex", rpc_params![ss58])
            .await
            .context("fetching account next index")?;
        Ok(nonce)
    }
}

async fn connect_first(urls: &[String], allow_any_chain: bool) -> Result<(usize, WsClient)> {
    let mut last_err = None;
    for (i, url) in urls.iter().enumerate() {
        match ws_client(url).await {
            Ok(client) => {
                if !allow_any_chain {
                    if let Err(err) = verify_dev_chain(&client).await {
                        warn!("node[{i}] {url} rejected: {err:#}");
                        last_err = Some(err);
                        continue;
                    }
                }
                return Ok((i, client));
            }
            Err(err) => {
                warn!("node[{i}] {url} unreachable: {err:#}");
                last_err = Some(err);
            }
        }
    }
    match last_err {
        Some(err) => Err(err).context("no usable node"),
        None => bail!("no node urls configured"),
    }
}

async fn verify_dev_chain(client: &WsClient) -> Result<()> {
    let name: String = client
        .request("system_chain", rpc_params![])
        .await
        .context("fetching chain name")?;
    if DEV_CHAIN_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        Ok(())
    } else {
        bail!("refusing non-dev chain {name:?}; pass --allow-any-chain to override")
    }
}

fn account_storage_key(account: &AccountId) -> String {
    let mut key = twox_128(b"System").to_vec();
    key.extend(twox_128(b"Account"));
    let encoded = account.encode();
    key.extend(blake2_128(&encoded)); // Blake2_128Concat hasher = blake2_128(x) ++ x
    key.extend(encoded);
    format!("0x{}", hex::encode(key))
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    #[test]
    fn refresh_keeps_last_success_until_failure_or_expiry() {
        let mut state = Readiness::default();
        assert!(!state.is_ready(Instant::now()));
        state.record(Some(100));
        let at = state.checked_at.unwrap();
        // Starting a refresh does not invalidate the previous successful sample.
        assert!(state.is_ready(at + Duration::from_secs(2)));
        assert!(state.is_ready(at + Duration::from_secs(11)));
        assert!(!state.is_ready(at + READINESS_TTL));
        state.record(None);
        assert!(!state.is_ready(Instant::now()));
        state.record(Some(100));
        assert!(state.is_ready(Instant::now()));
        state.record(Some(0));
        assert!(!state.is_ready(Instant::now()));
    }
}
