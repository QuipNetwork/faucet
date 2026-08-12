//! axum HTTP handlers: `/health`, `/request` (faucet mints), `/sign` (pool signs).

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use quip_protocol_runtime::AccountId;
use quip_tools::format_hash;
use serde::Deserialize;
use serde_json::{json, Value};
use sp_core::crypto::Ss58Codec;
use tracing::{error, info};

use crate::{calls, gate::GateDecision, AppState};

type Reply = (StatusCode, Json<Value>);

#[derive(Deserialize)]
pub struct FundRequest {
    pub dest: String,
    pub amount: Option<u128>,
}

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

fn reply(status: StatusCode, body: Value) -> Reply {
    (status, Json(body))
}

fn err(status: StatusCode, msg: &str) -> Reply {
    reply(status, json!({ "error": msg }))
}

/// pallet-revive `AccountId32Mapper` fallback: an H160 EVM address is backed
/// by the native account `h160 ++ 0xEE * 12`.
fn evm_mapped_account(address: [u8; 20]) -> AccountId {
    let mut bytes = [0xEEu8; 32];
    bytes[..20].copy_from_slice(&address);
    AccountId::from(bytes)
}

/// Parse a dest as SS58, `0x`+64-hex (native AccountId), or `0x`+40-hex (H160
/// EVM address, mapped to its revive-backed native account) into
/// `(account, canonical_key)`.
fn parse_dest(dest: &str) -> Option<(AccountId, String)> {
    let account = match dest.strip_prefix("0x").or_else(|| dest.strip_prefix("0X")) {
        Some(body) => {
            let bytes = hex::decode(body).ok()?;
            match bytes.len() {
                32 => AccountId::from(<[u8; 32]>::try_from(bytes.as_slice()).ok()?),
                20 => evm_mapped_account(<[u8; 20]>::try_from(bytes.as_slice()).ok()?),
                _ => return None,
            }
        }
        None => AccountId::from_ss58check(dest).ok()?,
    };
    let key = format!("0x{}", hex::encode(AsRef::<[u8]>::as_ref(&account)));
    Some((account, key))
}

fn validate(req: &FundRequest, default_amount: u128) -> Result<(AccountId, String, u128), Reply> {
    let amount = req.amount.unwrap_or(default_amount);
    if amount == 0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "amount must be a positive integer (plancks)",
        ));
    }
    let (account, key) = parse_dest(&req.dest).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "invalid 'dest': not an SS58 address, 0x+64-hex AccountId, or 0x+40-hex EVM address",
        )
    })?;
    Ok((account, key, amount))
}

fn map_gate(decision: &GateDecision) -> Option<Reply> {
    match decision {
        GateDecision::Allow => None,
        GateDecision::RateLimited { retry_after } | GateDecision::Degraded { retry_after } => {
            Some(reply(
                StatusCode::TOO_MANY_REQUESTS,
                json!({ "error": "rate limited", "retry_after_seconds": retry_after }),
            ))
        }
        GateDecision::InFlight => Some(reply(
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "error": "request already in flight for this dest" }),
        )),
        GateDecision::Funded { free } => Some(reply(
            StatusCode::FORBIDDEN,
            json!({ "error": "destination already funded", "free_balance_plancks": free }),
        )),
        GateDecision::Unavailable => Some(reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "balance check unavailable" }),
        )),
    }
}

pub async fn request(State(state): State<Arc<AppState>>, Json(req): Json<FundRequest>) -> Reply {
    let (account, key, amount) = match validate(&req, state.cfg.amount) {
        Ok(parsed) => parsed,
        Err(resp) => return resp,
    };

    let decision = state.gate.check(&key, &account, &state.chain).await;
    if let Some(resp) = map_gate(&decision) {
        return resp;
    }

    // Allowed + reserved. Transfer from the base wallet (its nonce lane lets these
    // pipeline concurrently; sudo is only used to top the base wallet up).
    let call = calls::transfer_keep_alive(account, amount);
    let result = state
        .chain
        .submit_lane(
            &state.base.pair,
            &state.base.account,
            &state.base.nonce,
            call,
            state.cfg.drip_confirm_timeout(),
            state.cfg.drip_confirm_poll(),
        )
        .await;
    state.gate.release(&key);

    match result {
        Ok(hash) => {
            state.gate.commit(&key);
            info!("funded {key} amount={amount}");
            reply(
                StatusCode::OK,
                json!({ "extrinsic_hash": format_hash(&hash), "amount": amount, "dest": req.dest, "dest_account": key }),
            )
        }
        Err(submit_err) => {
            error!("/request submit failed: {submit_err:#}");
            err(StatusCode::BAD_GATEWAY, "transfer failed; see faucet logs")
        }
    }
}

pub async fn sign(State(state): State<Arc<AppState>>, Json(req): Json<FundRequest>) -> Reply {
    let (account, key, amount) = match validate(&req, state.cfg.amount) {
        Ok(parsed) => parsed,
        Err(resp) => return resp,
    };

    let decision = state.gate.check(&key, &account, &state.chain).await;
    if let Some(resp) = map_gate(&decision) {
        return resp;
    }

    let allocated = match state.pool.allocate(amount) {
        Some(allocated) => allocated,
        None => {
            state.gate.release(&key);
            return reply(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "error": "faucet pool temporarily exhausted" }),
            );
        }
    };

    // Fresh on-chain nonce for the pool account (no local counter → no gap).
    let nonce = match state.chain.next_index(&allocated.account).await {
        Ok(nonce) => nonce,
        Err(nonce_err) => {
            error!("/sign nonce fetch failed: {nonce_err:#}");
            state.pool.release(allocated.index);
            state.gate.release(&key);
            return err(StatusCode::BAD_GATEWAY, "sign failed; see faucet logs");
        }
    };
    state.pool.note_nonce(allocated.last_nonce, nonce);

    let call = calls::transfer_keep_alive(account, amount);
    let signed = state
        .pool
        .sign_with(allocated.index, &state.chain, call, nonce);
    state.gate.release(&key);

    match signed {
        Some((signed_extrinsic, hash)) => {
            // Optimistic: a handed-out tx is equivalent to funding.
            state.gate.commit(&key);
            state.pool.complete(allocated.index, amount, nonce);
            reply(
                StatusCode::OK,
                json!({
                    "signed_extrinsic": signed_extrinsic,
                    "extrinsic_hash": format_hash(&hash),
                    "nonce": nonce,
                    "from": allocated.account.to_ss58check(),
                    "amount": amount,
                    "dest": req.dest,
                    "dest_account": key,
                    "mode": "hybrid",
                }),
            )
        }
        None => {
            state.pool.release(allocated.index);
            err(StatusCode::BAD_GATEWAY, "sign failed; see faucet logs")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dest_accepts_ss58() {
        let account = AccountId::from([7u8; 32]);
        let ss58 = account.to_ss58check();
        let (parsed, key) = parse_dest(&ss58).expect("SS58 dest should parse");
        assert_eq!(parsed, account);
        assert_eq!(key, format!("0x{}", hex::encode([7u8; 32])));
    }

    #[test]
    fn parse_dest_accepts_native_hex() {
        let dest = format!("0x{}", hex::encode([3u8; 32]));
        let (parsed, key) = parse_dest(&dest).expect("64-hex dest should parse");
        assert_eq!(parsed, AccountId::from([3u8; 32]));
        assert_eq!(key, dest);
    }

    #[test]
    fn parse_dest_maps_evm_address_to_revive_account() {
        // 0x7a718C...4CF9 ++ 0xEE * 12 — the pallet-revive fallback account.
        let (parsed, key) = parse_dest("0x7a718C27469499AaE7c652C0D1A95BD14eCa4CF9")
            .expect("40-hex EVM dest should parse");
        let expected =
            "7a718c27469499aae7c652c0d1a95bd14eca4cf9eeeeeeeeeeeeeeeeeeeeeeee";
        assert_eq!(
            key,
            format!("0x{expected}"),
            "H160 must map to h160 ++ 0xEE*12"
        );
        assert_eq!(
            AsRef::<[u8]>::as_ref(&parsed),
            hex::decode(expected).unwrap().as_slice()
        );
    }

    #[test]
    fn parse_dest_evm_hex_is_case_insensitive() {
        let lower = parse_dest("0x7a718c27469499aae7c652c0d1a95bd14eca4cf9");
        let checksummed = parse_dest("0x7a718C27469499AaE7c652C0D1A95BD14eCa4CF9");
        assert_eq!(lower.map(|(_, key)| key), checksummed.map(|(_, key)| key));
    }

    #[test]
    fn parse_dest_rejects_bad_lengths_and_input() {
        assert!(parse_dest("0xabcd").is_none());
        assert!(parse_dest(&format!("0x{}", "ab".repeat(21))).is_none());
        assert!(parse_dest("0xnot-hex-at-all").is_none());
        assert!(parse_dest("").is_none());
        assert!(parse_dest("not-an-address").is_none());
    }
}
