//! R2-native transaction construction and JSON-RPC helpers.
//!
//! These helpers deliberately live in the faucet: the old `quip-tools` package
//! is tied to the H3 `quip-validator` runtime and is not part of R2. Building
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

    let (spec_version, transaction_version) = ensure_runtime_compatible(client).await?;

    Ok(ChainContext {
        genesis_hash,
        best_hash,
        best_number,
        nonce,
        spec_version,
        transaction_version,
    })
}

/// Static calls and whole-block event decoding require the compiled runtime schema.
pub async fn ensure_runtime_compatible(client: &WsClient) -> Result<(u32, u32)> {
    let version: Value = client
        .request("state_getRuntimeVersion", rpc_params![])
        .await
        .context("fetching runtime version")?;
    check_runtime_version(&version)
}

fn check_runtime_version(version: &Value) -> Result<(u32, u32)> {
    let read = |field: &str| -> Result<u32> {
        let value = version
            .get(field)
            .and_then(Value::as_u64)
            .with_context(|| format!("state_getRuntimeVersion missing or invalid {field}"))?;
        u32::try_from(value).with_context(|| format!("runtime {field} exceeds u32"))
    };
    let spec = read("specVersion")?;
    let transaction = read("transactionVersion")?;
    anyhow::ensure!(
        spec == runtime::VERSION.spec_version && transaction == runtime::VERSION.transaction_version,
        "runtime mismatch: chain spec {spec}, transaction {transaction}; faucet compiled for spec {}, transaction {}; rebuild faucet for runtime {spec} before submitting transactions",
        runtime::VERSION.spec_version, runtime::VERSION.transaction_version,
    );
    Ok((spec, transaction))
}

pub async fn submit_extrinsic(client: &WsClient, encoded_extrinsic: &[u8]) -> Result<Hash> {
    ensure_runtime_compatible(client).await?;
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

/// Confirm a sudo call at a finalized block and inspect its inner result.
/// Pool acceptance and System::ExtrinsicSuccess alone do not prove sudo success.
pub async fn submit_sudo_extrinsic(client: &WsClient, bytes: &[u8]) -> Result<Hash> {
    use jsonrpsee::core::client::SubscriptionClientT;
    ensure_runtime_compatible(client).await?;
    let encoded = format!("0x{}", hex::encode(bytes));
    let transaction_hash: Hash = sp_core::hashing::blake2_256(bytes).into();
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let mut statuses = client
            .subscribe::<Value, _>(
                "author_submitAndWatchExtrinsic",
                rpc_params![encoded.clone()],
                "author_unwatchExtrinsic",
            )
            .await
            .context("submitting sudo extrinsic")?;
        while let Some(status) = statuses.next().await {
            let status = status.context("watching sudo extrinsic")?;
            if let Some(finalized) = status.get("finalized") {
                let block_hash: Hash = serde_json::from_value(finalized.clone())?;
                let block: Value = client
                    .request("chain_getBlock", rpc_params![block_hash])
                    .await?;
                let extrinsics = block
                    .pointer("/block/extrinsics")
                    .and_then(Value::as_array)
                    .context("finalized block has no extrinsics")?;
                let index = extrinsics
                    .iter()
                    .position(|item| {
                        item.as_str()
                            .is_some_and(|s| s.eq_ignore_ascii_case(&encoded))
                    })
                    .context("sudo extrinsic absent from finalized block")?;
                let index = u32::try_from(index).context("extrinsic index overflow")?;
                let mut key = sp_core::hashing::twox_128(b"System").to_vec();
                key.extend_from_slice(&sp_core::hashing::twox_128(b"Events"));
                let events: Option<String> = client
                    .request(
                        "state_getStorage",
                        rpc_params![format!("0x{}", hex::encode(key)), block_hash],
                    )
                    .await?;
                let events = events.context("missing finalized System.Events")?;
                let bytes = hex::decode(events.strip_prefix("0x").context("invalid event hex")?)?;
                check_sudo_events(&bytes, index)?;
                return Ok(transaction_hash);
            }
            if ["invalid", "dropped", "usurped", "finalityTimeout"]
                .iter()
                .any(|name| status.as_str() == Some(*name) || status.get(*name).is_some())
            {
                anyhow::bail!("sudo transaction did not finalize: {status}");
            }
        }
        anyhow::bail!("sudo transaction subscription ended without a finalized receipt")
    })
    .await
    .context("sudo confirmation timed out; outcome unknown, transaction was not resubmitted")?
}

fn check_sudo_events(bytes: &[u8], index: u32) -> Result<()> {
    use codec::DecodeAll;
    use quip_protocol_runtime::RuntimeEvent;
    let events = Vec::<frame_system::EventRecord<RuntimeEvent, Hash>>::decode_all(&mut &bytes[..])
        .context("decoding finalized events with pinned runtime; check runtime compatibility")?;
    let mut outer_success = false;
    let mut inner_success = false;
    for record in events {
        if record.phase != frame_system::Phase::ApplyExtrinsic(index) {
            continue;
        }
        match record.event {
            RuntimeEvent::System(frame_system::Event::ExtrinsicSuccess { .. }) => {
                outer_success = true
            }
            RuntimeEvent::System(frame_system::Event::ExtrinsicFailed {
                dispatch_error, ..
            }) => anyhow::bail!("sudo extrinsic failed: {dispatch_error:?}"),
            RuntimeEvent::Sudo(pallet_sudo::Event::Sudid { sudo_result }) => {
                sudo_result.map_err(|error| anyhow!("sudo inner call failed: {error:?}"))?;
                inner_success = true;
            }
            _ => {}
        }
    }
    anyhow::ensure!(
        outer_success && inner_success,
        "finalized sudo receipt lacks successful System and Sudid events"
    );
    Ok(())
}

#[cfg(test)]
mod sudo_receipt_tests {
    use super::*;
    use frame_system::{EventRecord, Phase};
    use quip_protocol_runtime::RuntimeEvent;
    fn record(index: u32, event: RuntimeEvent) -> EventRecord<RuntimeEvent, Hash> {
        EventRecord {
            phase: Phase::ApplyExtrinsic(index),
            event,
            topics: vec![],
        }
    }
    fn success() -> RuntimeEvent {
        RuntimeEvent::System(frame_system::Event::ExtrinsicSuccess {
            dispatch_info: Default::default(),
        })
    }
    #[test]
    fn outer_success_does_not_hide_inner_bad_origin() {
        let bytes = vec![
            record(
                2,
                RuntimeEvent::Sudo(pallet_sudo::Event::Sudid {
                    sudo_result: Err(sp_runtime::DispatchError::BadOrigin),
                }),
            ),
            record(2, success()),
        ]
        .encode();
        assert!(check_sudo_events(&bytes, 2)
            .unwrap_err()
            .to_string()
            .contains("inner call failed"));
    }
    #[test]
    fn requires_both_success_events_for_exact_extrinsic() {
        let events = vec![
            record(
                2,
                RuntimeEvent::Sudo(pallet_sudo::Event::Sudid {
                    sudo_result: Ok(()),
                }),
            ),
            record(2, success()),
        ];
        assert!(check_sudo_events(&events.encode(), 2).is_ok());
        assert!(check_sudo_events(&events.encode(), 1).is_err());
        assert!(check_sudo_events(&vec![record(2, success())].encode(), 2).is_err());
        assert!(check_sudo_events(&events[..1].encode(), 2).is_err());
    }
    #[test]
    fn rejects_outer_failure_and_malformed_events() {
        let bytes = vec![record(
            2,
            RuntimeEvent::System(frame_system::Event::ExtrinsicFailed {
                dispatch_error: sp_runtime::DispatchError::BadOrigin,
                dispatch_info: Default::default(),
            }),
        )]
        .encode();
        assert!(check_sudo_events(&bytes, 2).is_err());
        assert!(check_sudo_events(&[0xff], 2).is_err());
    }
}

#[cfg(test)]
mod runtime_version_tests {
    use super::*;
    #[test]
    fn accepts_only_compiled_runtime_versions() {
        let mut version = serde_json::json!({
            "specVersion": runtime::VERSION.spec_version,
            "transactionVersion": runtime::VERSION.transaction_version,
        });
        assert!(check_runtime_version(&version).is_ok());
        version["specVersion"] = serde_json::json!(runtime::VERSION.spec_version + 1);
        assert!(check_runtime_version(&version)
            .unwrap_err()
            .to_string()
            .contains("rebuild faucet"));
        version["specVersion"] = serde_json::json!(runtime::VERSION.spec_version);
        version["transactionVersion"] = serde_json::json!(runtime::VERSION.transaction_version + 1);
        assert!(check_runtime_version(&version).is_err());
    }
    #[test]
    fn rejects_missing_malformed_and_overflow_versions() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"specVersion": "119"}),
            serde_json::json!({"specVersion": u64::MAX, "transactionVersion": 7}),
        ] {
            assert!(check_runtime_version(&value).is_err());
        }
    }
}
