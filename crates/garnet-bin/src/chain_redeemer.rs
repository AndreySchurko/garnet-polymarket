//! Redemption on top of the carried-over blockchain client.
//!
//! The contract addresses an outcome by the pair "condition + outcome index", and a
//! `token_id` does not contain that: one and the same `condition_id` describes both sides,
//! while the index says which of them we are redeeming. In the predecessor that index was
//! called `TokenSide::Yes/No` — a name that openly invited reading the position as a label
//! and redeeming the wrong side.

use garnet_blockchain::traits::BlockchainClient;
use garnet_blockchain::types::B256;
use garnet_core::market_meta::MarketMeta;
use garnet_core::settle::Redeemer;
use rust_decimal::Decimal;
use std::str::FromStr;

pub struct ChainRedeemer<B: BlockchainClient> {
    chain: B,
}

impl<B: BlockchainClient> ChainRedeemer<B> {
    pub fn new(chain: B) -> Self {
        Self { chain }
    }

    pub fn chain(&self) -> &B {
        &self.chain
    }
}

impl<B: BlockchainClient + Sync> Redeemer for ChainRedeemer<B> {
    async fn redeem(&self, meta: &MarketMeta, size: Decimal) -> anyhow::Result<Option<String>> {
        let outcome = meta.outcome_index.ok_or_else(|| {
            anyhow::anyhow!(
                "market {}: the outcome position is unknown, there is nothing to address the redemption with",
                meta.token_id
            )
        })?;

        let condition = B256::from_str(&meta.condition_id)
            .map_err(|e| anyhow::anyhow!("condition_id {}: {e}", meta.condition_id))?;

        let tx = self
            .chain
            .redeem_position(condition, meta.neg_risk, outcome, size)
            .await?;

        Ok(Some(format!("{tx:#x}")))
    }
}
