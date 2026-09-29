//! Runtime call builders (type-safe via the linked runtime crate — no SCALE
//! hand-assembly, so the wire format can never drift from the chain).

use quip_protocol_runtime::{AccountId, Runtime, RuntimeCall};
use sp_runtime::MultiAddress;

/// Direct `FaucetOps::mint`, signed by the Foundation-appointed authority.
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

#[cfg(test)]
mod tests {
    use super::*;
    use codec::Encode;
    #[test]
    fn mint_is_direct_and_authority_call_indices_are_stable() {
        let who = AccountId::from([1; 32]);
        assert_eq!(&mint(who, 100).encode()[..2], &[11, 0]);
        assert_eq!(
            &RuntimeCall::FaucetOps(pallet_faucet_ops::Call::<Runtime>::disable {}).encode()[..2],
            &[11, 1]
        );
        assert_eq!(
            &RuntimeCall::FaucetOps(pallet_faucet_ops::Call::<Runtime>::set_authority {
                authority: None
            })
            .encode()[..2],
            &[11, 2]
        );
    }
}
