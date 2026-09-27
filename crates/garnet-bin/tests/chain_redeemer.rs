//! Redemption addresses an outcome by number, not by label.

use garnet_bin::chain_redeemer::ChainRedeemer;
use garnet_blockchain::mock::{MockBlockchainClient, MockCall};
use garnet_core::market_meta::{parse_clob_market, MarketMeta};
use garnet_core::settle::Redeemer;
use garnet_types::market::OutcomeIndex;
use rust_decimal_macros::dec;

fn meta(token: &str) -> MarketMeta {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/clob_resolved_down.json");
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut m = parse_clob_market(&raw, token).unwrap();
    // The fixture's condition_id is not in B256 form — we substitute a real one.
    m.condition_id = "0x9a570ae93cf768e98bacbf414b26a3a0c2039301fc0b6bbbf7f6d82099b1c5e5".into();
    m
}

#[tokio::test]
async fn the_winning_side_is_redeemed_by_its_index_not_its_label() {
    // Down won — the condition's second outcome. The label is "Down", not "No".
    let m = meta("tok_sol_down");
    assert_eq!(m.outcome_index, Some(OutcomeIndex::Second));
    assert_eq!(m.outcome_label, "Down");

    let chain = MockBlockchainClient::new();
    let redeemer = ChainRedeemer::new(chain);
    let tx = redeemer.redeem(&m, dec!(100)).await.unwrap();
    assert!(tx.is_some(), "the redemption must return a transaction");

    let calls = redeemer.chain().calls().await;
    let redeems: Vec<&MockCall> = calls
        .iter()
        .filter(|c| matches!(c, MockCall::RedeemPosition(..)))
        .collect();
    assert_eq!(redeems.len(), 1);
    match redeems[0] {
        MockCall::RedeemPosition(_, neg_risk, outcome, size) => {
            assert_eq!(
                *outcome,
                OutcomeIndex::Second,
                "we redeem the second outcome"
            );
            assert_eq!(*size, dec!(100));
            assert!(!neg_risk);
        }
        other => panic!("expected a redemption, got {other:?}"),
    }
}

#[tokio::test]
async fn the_other_side_of_the_same_condition_is_a_different_index() {
    let m = meta("tok_sol_up");
    assert_eq!(m.outcome_index, Some(OutcomeIndex::First));
    assert_eq!(m.outcome_index.unwrap().index_set(), 1);

    let redeemer = ChainRedeemer::new(MockBlockchainClient::new());
    redeemer.redeem(&m, dec!(50)).await.unwrap();

    let calls = redeemer.chain().calls().await;
    match calls
        .iter()
        .find(|c| matches!(c, MockCall::RedeemPosition(..)))
        .unwrap()
    {
        MockCall::RedeemPosition(_, _, outcome, _) => {
            assert_eq!(
                *outcome,
                OutcomeIndex::First,
                "both sides are distinguishable by number"
            )
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_market_without_an_outcome_index_is_refused_loudly() {
    let mut m = meta("tok_sol_down");
    m.outcome_index = None;

    let redeemer = ChainRedeemer::new(MockBlockchainClient::new());
    let err = redeemer.redeem(&m, dec!(10)).await.unwrap_err();
    assert!(
        err.to_string().contains("the outcome position is unknown"),
        "got: {err}"
    );
    assert!(
        redeemer.chain().calls().await.is_empty(),
        "without knowing the side we do not go to the network at all"
    );
}
