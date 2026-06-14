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

/// Parse a dest as SS58 or `0x`+64-hex into `(account, canonical_key)`.
fn parse_dest(dest: &str) -> Option<(AccountId, String)> {
    let account = match dest.strip_prefix("0x").or_else(|| dest.strip_prefix("0X")) {
        Some(body) => {
            if body.len() != 64 {
                return None;
            }
            let bytes = hex::decode(body).ok()?;
            let arr: [u8; 32] = bytes.try_into().ok()?;
            AccountId::from(arr)
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
            "invalid 'dest': not an SS58 or 0x-hex AccountId",
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

    // Allowed + reserved. Pipeline the sudo-mint via the funder nonce lane.
    let nonce = state.funder_nonce.allocate();
    let call = calls::sudo_mint(account, amount);
    let result = state.chain.submit(&state.funder.pair, call, nonce).await;
    state.gate.release(&key);

    match result {
        Ok(hash) => {
            state.gate.commit(&key);
            info!("funded {key} amount={amount} nonce={nonce}");
            reply(
                StatusCode::OK,
                json!({ "extrinsic_hash": format_hash(&hash), "amount": amount, "dest": req.dest }),
            )
        }
        Err(submit_err) => {
            error!("/request submit failed: {submit_err:#}");
            // Resync the lane so a failed nonce doesn't gap-stall later mints.
            if let Ok(next) = state.chain.next_index(&state.funder.account).await {
                state.funder_nonce.resync(next);
            }
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
                }),
            )
        }
        None => {
            state.pool.release(allocated.index);
            err(StatusCode::BAD_GATEWAY, "sign failed; see faucet logs")
        }
    }
}
