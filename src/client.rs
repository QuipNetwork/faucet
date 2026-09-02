//! R2-native transaction construction and JSON-RPC helpers.
//!
//! These helpers deliberately live in the faucet: the old `quip-tools` package
//! is tied to the H3 `quip-protocol-rs` runtime and is not part of R2. Building
//! directly with R2's runtime types keeps the signed-extension order and H4
//! signature envelope identical to the node.

use anyhow::{anyhow, Context, Result};
use codec::Encode;
use jsonrpsee::{
    core::{client::ClientT, rpc_params},
    ws_client::{WsClient, WsClientBuilder},
};
use quip_protocol_runtime::{
    self as runtime, BlockNumber, Hash, Nonce, RuntimeCall, SignedPayload, UncheckedExtrinsic,
};
use quip_transaction_crypto::{account_id_from_public, HybridPair, HybridTxSignature};
use serde_json::Value;
use sp_core::{crypto::Ss58Codec, Pair as _};
use sp_runtime::{generic::Era, traits::SaturatedConversion};

const MAX_RPC_MESSAGE_SIZE: u32 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ChainContext {
    pub genesis_hash: Hash,
    pub best_hash: Hash,
    pub best_number: BlockNumber,
    pub nonce: Nonce,
    pub spec_version: u32,
    pub transaction_version: u32,
}

pub fn pair_from_suri(suri: &str) -> Result<HybridPair> {
    HybridPair::from_string(suri, None).map_err(|err| anyhow!("invalid signer SURI: {err:?}"))
}

pub fn signer_account(pair: &HybridPair) -> runtime::AccountId {
    account_id_from_public(&pair.public())
}

pub fn build_signed_extrinsic(
    signer: &HybridPair,
    call: RuntimeCall,
    context: ChainContext,
) -> UncheckedExtrinsic {
    let period = runtime::configs::BlockHashCount::get()
        .checked_next_power_of_two()
        .map(|count| count / 2)
        .unwrap_or(2) as u64;
    let tx_extension = runtime::native_tx_extension(
        Era::mortal(period, context.best_number.saturated_into()),
        context.nonce,
        0,
    );
    let payload = SignedPayload::from_raw(
        call.clone(),
        tx_extension.clone(),
        (
            (),
            (),
            context.spec_version,
            context.transaction_version,
            context.genesis_hash,
            context.best_hash,
            (),
            (),
            (),
            None,
            (),
            (),
        ),
    );
    let signature = payload.using_encoded(|encoded| HybridTxSignature::sign(signer, encoded));

    sp_runtime::generic::UncheckedExtrinsic::new_signed(
        call,
        account_id_from_public(&signer.public()).into(),
        signature,
        tx_extension,
    )
    .into()
}

pub fn encode_extrinsic(extrinsic: &UncheckedExtrinsic) -> Vec<u8> {
    extrinsic.encode()
}

pub fn format_hash(hash: &Hash) -> String {
    format!("0x{}", hex::encode(hash.as_bytes()))
}

pub async fn ws_client(rpc_url: &str) -> Result<WsClient> {
    WsClientBuilder::default()
        .max_request_size(MAX_RPC_MESSAGE_SIZE)
        .max_response_size(MAX_RPC_MESSAGE_SIZE)
        .build(rpc_url)
        .await
        .with_context(|| format!("connecting to {rpc_url}"))
}

pub async fn fetch_chain_context(
    client: &WsClient,
    signer: &runtime::AccountId,
) -> Result<ChainContext> {
    let genesis_hash: Option<Hash> = client
        .request("chain_getBlockHash", rpc_params![0_u32])
        .await
        .context("fetching genesis hash")?;
    let genesis_hash = genesis_hash.context("node returned no genesis hash")?;

    let best_header: Option<Value> = client
        .request("chain_getHeader", rpc_params![])
        .await
        .context("fetching best header")?;
    let best_header = best_header.context("node returned no best header")?;
    let best_number = parse_header_number(&best_header)?;

    let best_hash: Option<Hash> = client
        .request("chain_getBlockHash", rpc_params![best_number])
        .await
        .context("fetching best block hash")?;
    let best_hash = best_hash.context("node returned no best block hash")?;

    let nonce: Nonce = client
        .request(
            "system_accountNextIndex",
            rpc_params![signer.to_ss58check()],
        )
        .await
        .context("fetching signer nonce")?;

    let runtime_version: Value = client
        .request("state_getRuntimeVersion", rpc_params![])
        .await
        .context("fetching runtime version")?;
    let spec_version = runtime_version
        .get("specVersion")
        .and_then(Value::as_u64)
        .context("state_getRuntimeVersion response missing specVersion")?
        .saturated_into();
    let transaction_version = runtime_version
        .get("transactionVersion")
        .and_then(Value::as_u64)
        .context("state_getRuntimeVersion response missing transactionVersion")?
        .saturated_into();

    Ok(ChainContext {
        genesis_hash,
        best_hash,
        best_number,
        nonce,
        spec_version,
        transaction_version,
    })
}

pub async fn submit_extrinsic(client: &WsClient, encoded_extrinsic: &[u8]) -> Result<Hash> {
    client
        .request(
            "author_submitExtrinsic",
            rpc_params![format!("0x{}", hex::encode(encoded_extrinsic))],
        )
        .await
        .context("submitting extrinsic")
}

fn parse_header_number(header: &Value) -> Result<BlockNumber> {
    let number = header
        .get("number")
        .and_then(Value::as_str)
        .context("chain_getHeader response missing number")?;
    let digits = number
        .strip_prefix("0x")
        .with_context(|| format!("block number {number} is not 0x-prefixed hex"))?;

    u32::from_str_radix(digits, 16).with_context(|| format!("parsing block number {number}"))
}

#[cfg(test)]
mod tests {
    use codec::{Decode, Encode};
    use quip_protocol_runtime::{Hash, Runtime, RuntimeCall, UncheckedExtrinsic, VERSION};

    use super::{build_signed_extrinsic, encode_extrinsic, pair_from_suri, ChainContext};

    #[test]
    fn r2_hybrid_extrinsic_round_trips() {
        let pair = pair_from_suri("//Alice").expect("valid dev SURI");
        let call = RuntimeCall::System(frame_system::Call::<Runtime>::remark {
            remark: b"faucet-h4-round-trip".to_vec(),
        });
        let context = ChainContext {
            genesis_hash: Hash::from([1_u8; 32]),
            best_hash: Hash::from([2_u8; 32]),
            best_number: 1,
            nonce: 0,
            spec_version: VERSION.spec_version,
            transaction_version: VERSION.transaction_version,
        };

        let encoded = encode_extrinsic(&build_signed_extrinsic(&pair, call, context));
        let decoded = UncheckedExtrinsic::decode(&mut encoded.as_slice()).expect("R2 decode");

        assert_eq!(decoded.encode(), encoded);
    }
}
