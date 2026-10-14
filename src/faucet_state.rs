//! Read the authority and budget from one block; missing controls fail closed.
use anyhow::{ensure, Context, Result};
use codec::DecodeAll;
use jsonrpsee::{
    core::{client::ClientT, rpc_params},
    ws_client::WsClient,
};
use pallet_faucet_ops::FaucetState;
use quip_protocol_runtime::{AccountId, Hash};

pub async fn check(client: &WsClient, signer: &AccountId) -> Result<u128> {
    let at: Hash = client
        .request("chain_getFinalizedHead", rpc_params![])
        .await?;
    let version = client
        .request("state_getRuntimeVersion", rpc_params![at])
        .await?;
    crate::client::check_runtime_version(&version)?;
    let authority: Option<AccountId> = storage(client, at, "FaucetOps", "Authority").await?;
    let state: Option<FaucetState> = storage(client, at, "FaucetOps", "State").await?;
    let budget: Option<u128> = storage(client, at, "EmissionController", "FaucetBudget").await?;
    let issued: Option<u128> = storage(client, at, "EmissionController", "FaucetIssued").await?;
    validate(
        signer,
        authority,
        state,
        budget.unwrap_or(0),
        issued.unwrap_or(0),
    )
}

async fn storage<T: DecodeAll>(
    client: &WsClient,
    at: Hash,
    pallet: &str,
    item: &str,
) -> Result<Option<T>> {
    let mut key = sp_core::hashing::twox_128(pallet.as_bytes()).to_vec();
    key.extend_from_slice(&sp_core::hashing::twox_128(item.as_bytes()));
    let raw: Option<String> = client
        .request(
            "state_getStorage",
            rpc_params![format!("0x{}", hex::encode(key)), at],
        )
        .await
        .with_context(|| format!("reading {pallet}.{item}"))?;
    raw.map(|raw| {
        let bytes = hex::decode(raw.strip_prefix("0x").context("invalid storage hex")?)?;
        T::decode_all(&mut bytes.as_slice()).with_context(|| format!("decoding {pallet}.{item}"))
    })
    .transpose()
}

fn validate(
    signer: &AccountId,
    authority: Option<AccountId>,
    state: Option<FaucetState>,
    budget: u128,
    issued: u128,
) -> Result<u128> {
    ensure!(
        authority.as_ref() == Some(signer),
        "configured funder is not FaucetOps.Authority (rotated, revoked or unset)"
    );
    ensure!(
        state == Some(FaucetState::Enabled),
        "faucet fuse is not Enabled (missing, paused or permanently disabled)"
    );
    let remaining = budget
        .checked_sub(issued)
        .context("faucet issued exceeds budget")?;
    ensure!(remaining > 0, "faucet budget exhausted");
    Ok(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_revocation_fuse_and_budget_fail_closed() {
        let signer = AccountId::from([1; 32]);
        let key = Some(signer.clone());
        assert_eq!(
            validate(&signer, key.clone(), Some(FaucetState::Enabled), 100, 40).unwrap(),
            60
        );
        for authority in [None, Some(AccountId::from([2; 32]))] {
            assert!(validate(&signer, authority, Some(FaucetState::Enabled), 100, 0).is_err());
        }
        for state in [
            None,
            Some(FaucetState::Paused),
            Some(FaucetState::PermanentlyDisabled),
        ] {
            assert!(validate(&signer, key.clone(), state, 100, 0).is_err());
        }
        for (budget, issued) in [(0, 0), (100, 100), (100, 101)] {
            assert!(validate(
                &signer,
                key.clone(),
                Some(FaucetState::Enabled),
                budget,
                issued
            )
            .is_err());
        }
    }
}
