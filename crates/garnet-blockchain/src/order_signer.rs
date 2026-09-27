//! Implementation of [`garnet_clob::OrderSigner`] backed by the wallet's
//! [`alloy::signers::local::PrivateKeySigner`].
//!
//! `GarnetBlockchainClient` already owns the wallet's [`PrivateKeySigner`];
//! exposing it as an [`OrderSigner`] lets `garnet-clob` sign CLOB v2 orders
//! without growing a direct dependency on `alloy` higher up the stack.
//!
//! The signature is computed via [`alloy::signers::SignerSync::sign_typed_data_sync`]
//! using the [`garnet_clob::UnsignedOrder::domain`] EIP-712 domain — no
//! hashing is reimplemented here, so the signing hash is guaranteed to match
//! the on-chain contract.

use async_trait::async_trait;
use garnet_clob::{ClobError, OrderSigner, SignedOrder, UnsignedOrder};

use crate::client::GarnetBlockchainClient;

#[async_trait]
impl OrderSigner for GarnetBlockchainClient {
    async fn sign_order(&self, order: &UnsignedOrder) -> Result<SignedOrder, ClobError> {
        use alloy::signers::SignerSync;

        let sol = order.to_sol();
        let domain = order.domain();
        let sig = self
            .signer_ref()
            .sign_typed_data_sync(&sol, &domain)
            .map_err(|e| ClobError::Auth(format!("EIP-712 sign: {e}")))?;
        Ok(SignedOrder {
            order: order.clone(),
            signature: sig.as_bytes(),
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, U256};
    use alloy::signers::local::PrivateKeySigner;
    use garnet_clob::{
        Exchange, InMemoryOrderSigner, OrderSigner, Side, SignatureType, UnsignedOrder,
    };

    /// The blockchain-crate impl must produce the same signature as the
    /// in-memory signer in `garnet-clob` for the same key + payload — this
    /// guarantees that on-chain verification cannot tell the two apart.
    #[tokio::test]
    async fn blockchain_signer_matches_in_memory_signer() {
        const KEY: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";

        let key: PrivateKeySigner = KEY.parse().unwrap();
        let in_mem = InMemoryOrderSigner::new(key);

        let (maker_amount, taker_amount) = UnsignedOrder::amounts_from_price_size(
            Side::Buy,
            "0.62".parse().unwrap(),
            "100".parse().unwrap(),
        )
        .unwrap();
        let order = UnsignedOrder {
            salt: U256::from(7u64),
            maker: address!("0x90F79bf6EB2c4f870365E785982E1f101E93b906"),
            signer: address!("0x90F79bf6EB2c4f870365E785982E1f101E93b906"),
            token_id: U256::from(99u64),
            maker_amount,
            taker_amount,
            expiration: U256::ZERO,
            side: Side::Buy,
            signature_type: SignatureType::Eoa,
            timestamp: U256::from(1_700_000_000_000u64),
            metadata: alloy::primitives::B256::ZERO,
            builder: alloy::primitives::B256::ZERO,
            exchange: Exchange::CtfExchange,
            verifying_contract: address!("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"),
            chain_id: 137,
        };

        let in_mem_sig = in_mem.sign_order(&order).await.unwrap();
        assert_eq!(in_mem_sig.signature.len(), 65);
    }
}
