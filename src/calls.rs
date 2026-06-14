//! Runtime call builders (type-safe via the linked runtime crate — no SCALE
//! hand-assembly, so the wire format can never drift from the chain).

use quip_protocol_runtime::{AccountId, Runtime, RuntimeCall};
use sp_runtime::MultiAddress;

/// `FaucetOps::mint(who, amount)` — root-only; wrap in [`sudo`].
fn mint(who: AccountId, amount: u128) -> RuntimeCall {
    RuntimeCall::FaucetOps(pallet_faucet_ops::Call::<Runtime>::mint { who, amount })
}

/// `Sudo::sudo(FaucetOps::mint(...))` — the funder is the chain sudo key.
pub fn sudo_mint(who: AccountId, amount: u128) -> RuntimeCall {
    RuntimeCall::Sudo(pallet_sudo::Call::<Runtime>::sudo {
        call: Box::new(mint(who, amount)),
    })
}

/// `Balances::transfer_keep_alive(dest, value)` — signed by a pool account; keeps
/// the sender above the existential deposit so the account stays reusable.
pub fn transfer_keep_alive(dest: AccountId, value: u128) -> RuntimeCall {
    RuntimeCall::Balances(pallet_balances::Call::<Runtime>::transfer_keep_alive {
        dest: MultiAddress::Id(dest),
        value,
    })
}
