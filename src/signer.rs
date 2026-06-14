//! Funder key + deterministic pool-account derivation.

use anyhow::Result;
use quip_protocol_runtime::AccountId;
use quip_tools::{pair_from_suri, signer_account};
use quip_transaction_crypto::HybridPair;

/// The funded sudo key the faucet signs/derives from.
pub struct Funder {
    pub pair: HybridPair,
    pub account: AccountId,
    suri: String,
}

impl Funder {
    pub fn from_suri(suri: &str) -> Result<Self> {
        let pair = pair_from_suri(suri)?;
        let account = signer_account(&pair);
        Ok(Self {
            pair,
            account,
            suri: suri.to_owned(),
        })
    }

    /// Derive pool account `index` by hard-derivation from the funder secret:
    /// `<funder>//pool//<index>`. Deterministic → same accounts every boot, no
    /// key file, no stranded funds.
    pub fn derive_pool(&self, index: u32) -> Result<(HybridPair, AccountId)> {
        let suri = format!("{}//pool//{index}", self.suri);
        let pair = pair_from_suri(&suri)?;
        let account = signer_account(&pair);
        Ok((pair, account))
    }

    /// Derive the dedicated base (hot) wallet, `<funder>//faucet//base`. It is
    /// funded by sudo and used to transfer to users — its nonce is not contended,
    /// so a nonce lane gives `/request` concurrency without touching the sudo key.
    pub fn derive_base(&self) -> Result<(HybridPair, AccountId)> {
        let suri = format!("{}//faucet//base", self.suri);
        let pair = pair_from_suri(&suri)?;
        let account = signer_account(&pair);
        Ok((pair, account))
    }
}
