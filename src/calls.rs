//! Runtime call builders (type-safe via the linked runtime crate — no SCALE
//! hand-assembly, so the wire format can never drift from the chain).

use quip_protocol_runtime::{AccountId, Runtime, RuntimeCall};
use sp_runtime::MultiAddress;

/// `FaucetOps::mint(who, amount)` — signed directly by the funder, which must be
/// the `FaucetOps.Authority` account while the faucet is `Enabled`.
pub fn mint(who: AccountId, amount: u128) -> RuntimeCall {
    RuntimeCall::FaucetOps(pallet_faucet_ops::Call::<Runtime>::mint { who, amount })
}

/// `Balances::transfer_keep_alive(dest, value)` — signed by a pool account; keeps
/// the sender above the existential deposit so the account stays reusable.
pub fn transfer_keep_alive(dest: AccountId, value: u128) -> RuntimeCall {
    RuntimeCall::Balances(pallet_balances::Call::<Runtime>::transfer_keep_alive {
        dest: MultiAddress::Id(dest),
        value,
    })
}
