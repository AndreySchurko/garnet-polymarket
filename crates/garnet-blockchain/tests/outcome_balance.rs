//! The balance of outcome tokens is what the reconciler checks the ledger against.
//!
//! A position lives as two records: a row in our database and an ERC-1155 balance on
//! the address holding the collateral. A divergence between them means one of the two
//! pictures of the world is wrong, and it can only be noticed by reading the second.

use garnet_blockchain::mock::MockBlockchainClient;
use garnet_blockchain::traits::BlockchainClient;
use rust_decimal_macros::dec;

/// A real Polymarket `token_id`: a 78-digit decimal number.
const TOKEN: &str = "68025992808468258985261421546519089032442352263424563387732402443351264297260";

#[tokio::test]
async fn an_untouched_outcome_has_a_zero_balance() {
    let chain = MockBlockchainClient::new();
    assert_eq!(chain.balance_of_outcome(TOKEN).await.unwrap(), dec!(0));
}

#[tokio::test]
async fn the_balance_is_read_back_in_shares() {
    // CTF tokens have six decimals, like USDC: 25 shares are 25000000 units.
    // A scale error here would quietly turn the reconciler into a generator of false
    // divergences.
    let chain = MockBlockchainClient::new();
    chain.set_outcome_balance(TOKEN, dec!(25.5)).await;
    assert_eq!(chain.balance_of_outcome(TOKEN).await.unwrap(), dec!(25.5));
}

#[tokio::test]
async fn a_token_id_that_is_not_a_number_is_an_error_not_a_zero() {
    // A zero in place of an unreadable identifier would look like "there is no
    // position" and would create a divergence out of nowhere.
    let chain = MockBlockchainClient::new();
    assert!(chain.balance_of_outcome("not a number").await.is_err());
}
