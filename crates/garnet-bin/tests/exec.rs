//! The executor's fork: a live client or an explicit refusal.
//!
//! `ClobExec` is a trait with an `async fn`, so `dyn` cannot be built for it, and the choice
//! of "keys or no keys" has to be made by type rather than by a box.
//! There is one requirement of the fork: the absence of keys **must not turn a live wallet
//! into shadow**. Swapping a mode behind the operator's back is the worst kind of silent
//! error: they look at rows showing a profit and think it is their money.

use garnet_bin::exec::Exec;
use garnet_clob::mock::MockClobClient;
use garnet_clob::types::Side as ClobSide;
use garnet_core::execute::{ClobExec, OrderRequest};
use garnet_db::Side;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn req(size: Decimal) -> OrderRequest {
    OrderRequest {
        token_id: "tok".into(),
        side: Side::Buy,
        limit_price: dec!(0.42),
        size_shares: size,
        neg_risk: false,
    }
}

#[tokio::test]
async fn without_keys_a_live_order_is_refused_not_quietly_shadowed() {
    // The default type is the production client: the "no keys" branch creates none of its own.
    let off: Exec = Exec::off();
    let err = off.place_ioc(&req(dec!(50))).await.unwrap_err();
    assert!(
        err.to_string().contains("keys"),
        "the refusal must name the reason, got: {err}"
    );
}

#[tokio::test]
async fn with_keys_the_order_reaches_the_exchange() {
    let mock = MockClobClient::new();
    mock.expect_success("o1", "tok", dec!(0.42), dec!(50), ClobSide::Buy)
        .await;
    mock.add_open_order("o1", "tok", dec!(0.42), dec!(50), ClobSide::Buy)
        .await;
    mock.simulate_fill("o1", dec!(50)).await;

    let exec = Exec::live(mock);
    let fill = exec.place_ioc(&req(dec!(50))).await.unwrap();

    assert_eq!(fill.size, dec!(50));
    let sent = exec.client().unwrap().received_orders().await;
    assert_eq!(sent.len(), 1, "the order must reach the client, not a stub");
}
