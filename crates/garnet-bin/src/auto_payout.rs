//! The payout the platform credits.
//!
//! On a Polymarket account with auto-payout enabled, winning positions are redeemed without
//! our involvement: the outcome tokens sit on the proxy, Polymarket's relay sends the
//! transaction, and the pUSD arrives in the balance by itself.
//!
//! In this configuration our `ChainRedeemer` is not merely unnecessary — it would not work:
//! `redeem_position` burns `msg.sender`'s tokens, while our sender is the EOA signer, which
//! does not hold them. A proxy transaction relay is implemented in neither the predecessor nor
//! Garnet.
//!
//! So in this configuration the check reads differently: what is verified is not our
//! redemption but the agreement of the credited payout with our ledger.

use garnet_core::market_meta::MarketMeta;
use garnet_core::settle::Redeemer;
use rust_decimal::Decimal;

pub struct AutoPayout;

impl Redeemer for AutoPayout {
    async fn redeem(&self, meta: &MarketMeta, size: Decimal) -> anyhow::Result<Option<String>> {
        // There is no hash and there cannot be: the transaction is not ours. The settlement
        // is still recorded as usual — the payout is due and must be in the ledger.
        println!(
            "the platform credits the payout: {} shares at {}, no transaction is sent",
            size, meta.token_id
        );
        Ok(None)
    }
}
