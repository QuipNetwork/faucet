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

pub(crate) fn check_runtime_version(version: &Value) -> Result<(u32, u32)> {
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

/// Distinguish authoritative rejection from a possibly accepted transaction.
#[derive(Debug)]
pub enum MintSubmitError {
    Rejected {
        error: anyhow::Error,
        retryable_nonce: bool,
    },
    Ambiguous(anyhow::Error),
}
impl std::fmt::Display for MintSubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { error, .. } => write!(f, "mint rejected: {error:#}"),
            Self::Ambiguous(error) => write!(
                f,
                "mint outcome unknown; reconcile before restart: {error:#}"
            ),
        }
    }
}
impl std::error::Error for MintSubmitError {}
impl MintSubmitError {
    fn rejected(error: anyhow::Error) -> Self {
        Self::Rejected {
            error,
            retryable_nonce: false,
        }
    }
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, Self::Ambiguous(_))
    }
    pub fn is_stale_nonce(&self) -> bool {
        matches!(
            self,
            Self::Rejected {
                retryable_nonce: true,
                ..
            }
        )
    }
}

fn submission_error(error: jsonrpsee::core::client::Error) -> MintSubmitError {
    if let jsonrpsee::core::client::Error::Call(ref rpc) = error {
        // SDK author error codes: definite pool/method rejections only.
        // AlreadyImported (1013), generic server errors and transport failures
        // can refer to an accepted transaction and must remain ambiguous.
        if matches!(rpc.code(), 1001 | 1002 | 1010..=1012 | 1014..=1016 | 1018..=1021 | -32602..=-32600)
        {
            let message = rpc.to_string().to_ascii_lowercase();
            let retryable_nonce = rpc.code() == 1014
                || (rpc.code() == 1010
                    && (message.contains("outdated") || message.contains("stale")));
            return MintSubmitError::Rejected {
                error: error.into(),
                retryable_nonce,
            };
        }
    }
    MintSubmitError::Ambiguous(error.into())
}

fn terminal_mint_status(status: &Value, inclusion_seen: &mut bool) -> Option<MintSubmitError> {
    let matches = |name| status.as_str() == Some(name) || status.get(name).is_some();
    *inclusion_seen |= matches("inBlock") || matches("retracted");
    if matches("invalid") && *inclusion_seen {
        Some(MintSubmitError::Ambiguous(anyhow!(
            "invalid after inclusion/retraction; canonical outcome must be reconciled"
        )))
    } else if matches("invalid") {
        Some(MintSubmitError::rejected(anyhow!(
            "invalid mint transaction"
        )))
    } else if ["dropped", "usurped", "finalityTimeout"]
        .into_iter()
        .any(matches)
    {
        Some(MintSubmitError::Ambiguous(anyhow!(
            "mint did not finalize: {status}"
        )))
    } else {
        None
    }
}

/// Require a finalized receipt with System success and both mint events.
pub async fn submit_mint_extrinsic(
    client: &WsClient,
    bytes: &[u8],
    who: &runtime::AccountId,
    amount: u128,
) -> std::result::Result<Hash, MintSubmitError> {
    use jsonrpsee::core::client::SubscriptionClientT;
    ensure_runtime_compatible(client)
        .await
        .map_err(MintSubmitError::rejected)?;
    let encoded = format!("0x{}", hex::encode(bytes));
    let transaction_hash: Hash = sp_core::hashing::blake2_256(bytes).into();
    confirm_with_timeout(std::time::Duration::from_secs(120), async {
        let mut statuses = client
            .subscribe::<Value, _>(
                "author_submitAndWatchExtrinsic",
                rpc_params![encoded.clone()],
                "author_unwatchExtrinsic",
            )
            .await
            .map_err(submission_error)?;
        let mut inclusion_seen = false;
        while let Some(status) = statuses.next().await {
            let status = status.map_err(|error| MintSubmitError::Ambiguous(error.into()))?;
            if let Some(finalized) = status.get("finalized") {
                finalized_mint_receipt(client, finalized, &encoded, who, amount)
                    .await
                    .map_err(MintSubmitError::Ambiguous)?;
                return Ok(transaction_hash);
            }
            if let Some(error) = terminal_mint_status(&status, &mut inclusion_seen) {
                return Err(error);
            }
        }
        Err(MintSubmitError::Ambiguous(anyhow!(
            "mint subscription ended without a finalized receipt"
        )))
    })
    .await
}

async fn finalized_mint_receipt(
    client: &WsClient,
    finalized: &Value,
    encoded: &str,
    who: &runtime::AccountId,
    amount: u128,
) -> Result<()> {
    let block_hash: Hash = serde_json::from_value(finalized.clone())?;
    let version: Value = client
        .request("state_getRuntimeVersion", rpc_params![block_hash])
        .await?;
    check_runtime_version(&version)?;
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
                .is_some_and(|s| s.eq_ignore_ascii_case(encoded))
        })
        .context("mint extrinsic absent from finalized block")?;
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
    check_mint_events(&bytes, index, who, amount)
}

async fn confirm_with_timeout<T>(
    duration: std::time::Duration,
    confirmation: impl std::future::Future<Output = std::result::Result<T, MintSubmitError>>,
) -> std::result::Result<T, MintSubmitError> {
    tokio::time::timeout(duration, confirmation)
        .await
        .map_err(|_| {
            MintSubmitError::Ambiguous(anyhow!(
                "mint confirmation timed out; transaction was not resubmitted"
            ))
        })?
}

fn check_mint_events(
    bytes: &[u8],
    index: u32,
    who: &runtime::AccountId,
    amount: u128,
) -> Result<()> {
    use codec::DecodeAll;
    use quip_protocol_runtime::RuntimeEvent;
    let events = Vec::<frame_system::EventRecord<RuntimeEvent, Hash>>::decode_all(&mut &bytes[..])
        .context("decoding finalized events with pinned runtime; check runtime compatibility")?;
    let mut outer_success = false;
    let mut minted = false;
    let mut issued = false;
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
            }) => anyhow::bail!("mint extrinsic failed: {dispatch_error:?}"),
            RuntimeEvent::FaucetOps(pallet_faucet_ops::Event::Minted {
                who: recipient,
                amount: value,
            }) => {
                anyhow::ensure!(
                    &recipient == who && value == amount && !minted,
                    "unexpected or duplicate mint event"
                );
                minted = true;
            }
            RuntimeEvent::EmissionController(pallet_emission_controller::Event::FaucetMinted {
                who: recipient,
                amount: value,
            }) => {
                anyhow::ensure!(
                    &recipient == who && value == amount && !issued,
                    "unexpected or duplicate issuance event"
                );
                issued = true;
            }
            _ => {}
        }
    }
    anyhow::ensure!(
        outer_success && minted && issued,
        "finalized mint receipt lacks successful System and matching FaucetOps.Minted / EmissionController.FaucetMinted events"
    );
    Ok(())
}

#[cfg(test)]
mod mint_receipt_tests {
    use super::*;
    use frame_system::{EventRecord, Phase};
    use quip_protocol_runtime::{AccountId, RuntimeEvent};
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
    fn minted() -> RuntimeEvent {
        RuntimeEvent::FaucetOps(pallet_faucet_ops::Event::Minted {
            who: AccountId::from([1; 32]),
            amount: 100,
        })
    }
    fn issued() -> RuntimeEvent {
        RuntimeEvent::EmissionController(pallet_emission_controller::Event::FaucetMinted {
            who: AccountId::from([1; 32]),
            amount: 100,
        })
    }
    fn check(events: Vec<EventRecord<RuntimeEvent, Hash>>) -> Result<()> {
        check_mint_events(&events.encode(), 2, &AccountId::from([1; 32]), 100)
    }
    #[test]
    fn requires_both_events_for_exact_extrinsic_and_payment() {
        assert!(check(vec![
            record(2, issued()),
            record(2, minted()),
            record(2, success())
        ])
        .is_ok());
        assert!(check(vec![
            record(2, issued()),
            record(1, minted()),
            record(2, success())
        ])
        .is_err());
        assert!(check(vec![
            record(2, issued()),
            record(2, minted()),
            record(1, success())
        ])
        .is_err());
        assert!(check(vec![record(2, minted()), record(2, success())]).is_err());
        assert!(check(vec![
            record(1, issued()),
            record(2, minted()),
            record(2, success())
        ])
        .is_err());
        assert!(check(vec![
            record(2, issued()),
            record(2, issued()),
            record(2, minted()),
            record(2, success())
        ])
        .is_err());
        assert!(check(vec![record(2, success())]).is_err());
        assert!(check(vec![record(2, minted())]).is_err());
        let events = vec![
            record(2, issued()),
            record(2, minted()),
            record(2, success()),
        ]
        .encode();
        assert!(check_mint_events(&events, 2, &AccountId::from([2; 32]), 100).is_err());
        assert!(check_mint_events(&events, 2, &AccountId::from([1; 32]), 101).is_err());
        assert!(check_mint_events(&[0xff], 2, &AccountId::from([1; 32]), 100).is_err());
    }
    #[test]
    fn dispatch_failure_overrides_success() {
        let failure = RuntimeEvent::System(frame_system::Event::ExtrinsicFailed {
            dispatch_error: sp_runtime::DispatchError::BadOrigin,
            dispatch_info: Default::default(),
        });
        assert!(check(vec![
            record(2, issued()),
            record(2, minted()),
            record(2, success()),
            record(2, failure)
        ])
        .is_err());
        assert!(check(vec![
            record(2, issued()),
            record(2, minted()),
            record(2, minted()),
            record(2, success())
        ])
        .is_err());
    }
    #[test]
    fn fuse_and_budget_dispatch_errors_fail_top_up() {
        for dispatch_error in [
            pallet_faucet_ops::Error::<runtime::Runtime>::Disabled.into(),
            pallet_emission_controller::Error::<runtime::Runtime>::BudgetExceeded.into(),
        ] {
            assert!(check(vec![record(
                2,
                RuntimeEvent::System(frame_system::Event::ExtrinsicFailed {
                    dispatch_error,
                    dispatch_info: Default::default(),
                })
            )])
            .is_err());
        }
    }
    #[test]
    fn unrelated_scheduled_funding_failure_is_not_a_mint_failure() {
        let mut unrelated = record(
            0,
            RuntimeEvent::EmissionController(pallet_emission_controller::Event::FundingFailed {
                subnet: 0,
                amount: 100,
                error: sp_runtime::DispatchError::BadOrigin,
            }),
        );
        unrelated.phase = Phase::Initialization;
        assert!(check(vec![
            unrelated,
            record(2, issued()),
            record(2, minted()),
            record(2, success())
        ])
        .is_ok());
    }
    #[tokio::test]
    async fn unknown_outcome_times_out_without_retrying() {
        let result =
            confirm_with_timeout::<()>(std::time::Duration::from_millis(1), std::future::pending())
                .await;
        assert!(result.unwrap_err().to_string().contains("outcome unknown"));
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

#[cfg(test)]
mod submission_classification_tests {
    use super::*;
    use jsonrpsee::{core::client::Error, types::ErrorObjectOwned};
    fn rpc(code: i32, message: &str) -> MintSubmitError {
        submission_error(Error::Call(ErrorObjectOwned::owned(
            code, message, None::<()>,
        )))
    }
    #[test]
    fn only_definite_nonce_rejections_are_retryable() {
        for (code, message) in [
            (1010, "Transaction is outdated"),
            (1010, "Stale"),
            (1014, "Priority is too low"),
        ] {
            let error = rpc(code, message);
            assert!(error.is_stale_nonce());
            assert!(!error.is_ambiguous());
        }
        for (code, message) in [
            (1010, "Invalid signature"),
            (1010, "Payment"),
            (-32601, "Method not found"),
        ] {
            let error = rpc(code, message);
            assert!(!error.is_stale_nonce());
            assert!(!error.is_ambiguous());
        }
        for error in [
            rpc(1013, "Already imported"),
            rpc(-32603, "Internal error"),
            submission_error(Error::RequestTimeout),
            submission_error(Error::Custom("Stale transport failure".into())),
        ] {
            assert!(error.is_ambiguous());
            assert!(!error.is_stale_nonce());
        }
        assert!(
            !MintSubmitError::rejected(anyhow!("runtime mismatch before submission"))
                .is_ambiguous()
        );
    }
    #[test]
    fn invalid_after_inclusion_or_retraction_remains_ambiguous() {
        for prior in [
            serde_json::json!({"inBlock": "0x01"}),
            serde_json::json!({"retracted": "0x01"}),
        ] {
            let mut inclusion_seen = false;
            assert!(terminal_mint_status(&prior, &mut inclusion_seen).is_none());
            assert!(
                terminal_mint_status(&serde_json::json!("ready"), &mut inclusion_seen).is_none()
            );
            let error =
                terminal_mint_status(&serde_json::json!("invalid"), &mut inclusion_seen).unwrap();
            assert!(error.is_ambiguous());
            assert!(!error.is_stale_nonce());
        }
    }
    #[test]
    fn invalid_does_not_latch_but_lost_finality_does() {
        assert!(
            !terminal_mint_status(&serde_json::json!("invalid"), &mut false)
                .unwrap()
                .is_ambiguous()
        );
        for status in [
            serde_json::json!("dropped"),
            serde_json::json!({"usurped": "0x01"}),
            serde_json::json!({"finalityTimeout": "0x02"}),
        ] {
            let error = terminal_mint_status(&status, &mut false).unwrap();
            assert!(error.is_ambiguous());
            assert!(!error.is_stale_nonce());
        }
        assert!(terminal_mint_status(&serde_json::json!("ready"), &mut false).is_none());
    }
}
