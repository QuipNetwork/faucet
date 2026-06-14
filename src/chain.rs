//! Node client: connection + failover, cached context, balance query, submission.
//!
//! Reuses `quip-tools` for the proven build/sign/submit path and wraps it with
//! multi-node failover and a cached chain context. jsonrpsee multiplexes
//! concurrent requests over one connection, so there is no global lock.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{bail, Context, Result};
use codec::{Decode, Encode};
use jsonrpsee::{
    core::{client::ClientT, rpc_params},
    ws_client::WsClient,
};
use parking_lot::RwLock;
use quip_protocol_runtime::{AccountId, Hash, RuntimeCall};
use quip_tools::{
    build_signed_extrinsic, encode_extrinsic, fetch_chain_context, submit_extrinsic, ws_client,
    ChainContext,
};
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

/// Connected node client with ordered failover.
pub struct ChainClient {
    urls: Vec<String>,
    client: RwLock<Arc<WsClient>>,
    idx: AtomicUsize,
    base_ctx: RwLock<ChainContext>,
    funder: AccountId,
    allow_any_chain: bool,
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
                    *self.client.write() = Arc::new(client);
                    self.idx.store(i, Ordering::SeqCst);
                    warn!("faucet failed over to node[{i}]: {}", self.urls[i]);
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
        let ctx = fetch_chain_context(&client, &self.funder).await?;
        *self.base_ctx.write() = ctx;
        Ok(())
    }

    /// Submit a sudo/funder extrinsic, fetching the funder nonce fresh each try and
    /// retrying on a stale-nonce rejection. The funder (chain sudo key) may be a
    /// shared, active account, so a cached/lane nonce goes stale — fetch-fresh +
    /// retry is the correct model. Fire-and-forget (`author_submitExtrinsic`);
    /// not failed over (resubmitting a possibly-landed tx could double-fund).
    pub async fn submit_funder(
        &self,
        signer: &HybridPair,
        account: &AccountId,
        call: RuntimeCall,
    ) -> Result<Hash> {
        let mut last_err = String::new();
        for attempt in 0..5 {
            let nonce = self.next_index(account).await?;
            let mut ctx = *self.base_ctx.read();
            ctx.nonce = nonce;
            let extrinsic = build_signed_extrinsic(signer, call.clone(), ctx);
            let bytes = encode_extrinsic(&extrinsic);
            let client = self.client();
            match submit_extrinsic(&client, &bytes).await {
                Ok(hash) => return Ok(hash),
                Err(err) => {
                    let msg = format!("{err:#}");
                    let stale = msg.contains("outdated")
                        || msg.contains("Stale")
                        || msg.contains("Priority is too low");
                    if stale && attempt < 4 {
                        warn!("funder nonce stale (attempt {attempt}); refetching");
                        last_err = msg;
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        continue;
                    }
                    return Err(err).context("submitting funder extrinsic");
                }
            }
        }
        bail!("funder submit stale after retries: {last_err}")
    }

    /// Submit a transfer from the dedicated base wallet, drawing the nonce from its
    /// lane (concurrent — the base wallet is faucet-only). On a stale rejection,
    /// resync the lane from chain and retry. Fire-and-forget.
    pub async fn submit_lane(
        &self,
        signer: &HybridPair,
        account: &AccountId,
        lane: &NonceLane,
        call: RuntimeCall,
    ) -> Result<Hash> {
        let mut last_err = String::new();
        for attempt in 0..5 {
            let nonce = lane.allocate();
            let mut ctx = *self.base_ctx.read();
            ctx.nonce = nonce;
            let extrinsic = build_signed_extrinsic(signer, call.clone(), ctx);
            let bytes = encode_extrinsic(&extrinsic);
            let client = self.client();
            match submit_extrinsic(&client, &bytes).await {
                Ok(hash) => return Ok(hash),
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
