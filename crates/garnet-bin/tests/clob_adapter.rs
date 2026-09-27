//! The adapter between the carried-over CLOB client and Garnet's execution contract.

use garnet_bin::clob_adapter::ClobAdapter;
use garnet_clob::mock::MockClobClient;
use garnet_clob::types::{OrderKind, Side as ClobSide};
use garnet_core::execute::{ClobExec, ExecError, OrderRequest};
use garnet_core::shadow::FillSource;
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
async fn every_copy_order_goes_out_as_fak() {
    // The whole point of the change to the carried-over crate: GTC would leave the order
    // resting in the book as a maker leg, the opposite of copying a taker entry.
    let mock = MockClobClient::new();
    mock.expect_success("o1", "tok", dec!(0.42), dec!(50), ClobSide::Buy)
        .await;
    mock.add_open_order("o1", "tok", dec!(0.42), dec!(50), ClobSide::Buy)
        .await;
    mock.simulate_fill("o1", dec!(50)).await;

    let adapter = ClobAdapter::new(mock);
    adapter.place_ioc(&req(dec!(50))).await.unwrap();

    let sent = adapter.client().received_orders().await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].kind, OrderKind::Fak, "copying uses FAK only");
}

#[tokio::test]
async fn filled_size_comes_from_the_order_not_from_the_ack() {
    // The submission response carries the requested size, not the filled one: 50 against 40.
    let mock = MockClobClient::new();
    mock.expect_success("o2", "tok", dec!(0.42), dec!(50), ClobSide::Buy)
        .await;
    mock.add_open_order("o2", "tok", dec!(0.42), dec!(50), ClobSide::Buy)
        .await;
    mock.simulate_fill("o2", dec!(40)).await;

    let adapter = ClobAdapter::new(mock);
    let fill = adapter.place_ioc(&req(dec!(50))).await.unwrap();

    assert_eq!(fill.size, dec!(40), "a partial fill is read from the order");
    assert_eq!(fill.notional, dec!(16.80));
    assert_eq!(fill.source, FillSource::Clob);
    assert_eq!(fill.fee_usd, Decimal::ZERO, "the fee is set by the caller");
}

#[tokio::test]
async fn an_accepted_order_with_no_visible_fill_is_unknown_not_rejected() {
    // A defect paid for by a double buy on 2026-09-04: the exchange accepted the order,
    // `get_order` still showed zero filled, the adapter called that a refusal, and the retry
    // bought a second identical position — $2.00 instead of $1.00.
    let mock = MockClobClient::new();
    mock.expect_success("o3", "tok", dec!(0.42), dec!(50), ClobSide::Buy)
        .await;
    mock.add_open_order("o3", "tok", dec!(0.42), dec!(50), ClobSide::Buy)
        .await;
    // we do not simulate a fill: size_matched stays zero, the order is alive

    let adapter = ClobAdapter::new(mock).with_poll(2, std::time::Duration::ZERO);
    let err = adapter.place_ioc(&req(dec!(50))).await.unwrap_err();
    assert!(
        matches!(err, ExecError::Unknown(_)),
        "an unconfirmed outcome is not a refusal: {err}"
    );
}

#[tokio::test]
async fn an_exchange_cancel_is_a_rejection_not_a_silent_zero() {
    let mock = MockClobClient::new();
    mock.expect_success("o4", "tok", dec!(0.42), dec!(50), ClobSide::Buy)
        .await;
    mock.add_open_order("o4", "tok", dec!(0.42), dec!(50), ClobSide::Buy)
        .await;
    mock.simulate_cancel("o4").await;

    let adapter = ClobAdapter::new(mock);
    // The submission response came back with status Open, but the exchange cancelled the
    // order: nothing filled, and that is a refusal.
    let err = adapter.place_ioc(&req(dec!(50))).await.unwrap_err();
    assert!(
        matches!(err, ExecError::Rejected(_)),
        "a cancelled order with zero filled is an honest refusal: {err}"
    );
}

/// An exchange trade on our order.
fn trade(order_id: &str, size: Decimal, price: Decimal) -> garnet_clob::types::TradeInfo {
    garnet_clob::types::TradeInfo {
        trade_id: format!("t-{order_id}-{price}"),
        taker_order_id: order_id.to_string(),
        maker_order_ids: vec![],
        market: "0xcond".into(),
        token_id: "tok".into(),
        side: ClobSide::Buy,
        size,
        price,
        match_time: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn the_fill_price_comes_from_the_trades_not_from_the_limit() {
    // A live measurement on 2026-09-04: a limit of 0.021, a fill at 0.0181, and the position
    // was recorded at $0.525 instead of $0.4549 — a 15% overstatement. `OrderInfo.price` is
    // the **order's** price, not the fill's; the real price exists only in the trades. The
    // fee is computed from it too, so the error was doubled.
    let mock = MockClobClient::new();
    mock.expect_success("o1", "tok", dec!(0.021), dec!(50), ClobSide::Buy)
        .await;
    mock.add_open_order("o1", "tok", dec!(0.021), dec!(50), ClobSide::Buy)
        .await;
    mock.simulate_fill("o1", dec!(25)).await;
    mock.set_trades(vec![
        trade("o1", dec!(10), dec!(0.017)),
        trade("o1", dec!(15), dec!(0.019)),
    ])
    .await;

    let adapter = ClobAdapter::new(mock);
    let fill = adapter.place_ioc(&req(dec!(50))).await.unwrap();

    assert_eq!(fill.size, dec!(25));
    // (10*0.017 + 15*0.019) / 25 = 0.0182
    assert_eq!(fill.avg_price, dec!(0.0182));
    assert_eq!(fill.notional, dec!(0.455));
}

#[tokio::test]
async fn without_visible_trades_the_limit_price_is_the_fallback() {
    // The trade may not appear in the API in time. The order's price is then an upper bound:
    // it overstates the cost rather than understating it, and the reconciler will correct it.
    let mock = MockClobClient::new();
    mock.expect_success("o2", "tok", dec!(0.021), dec!(50), ClobSide::Buy)
        .await;
    mock.add_open_order("o2", "tok", dec!(0.021), dec!(50), ClobSide::Buy)
        .await;
    mock.simulate_fill("o2", dec!(25)).await;

    let adapter = ClobAdapter::new(mock);
    let fill = adapter.place_ioc(&req(dec!(50))).await.unwrap();

    assert_eq!(fill.avg_price, dec!(0.021));
}

#[tokio::test]
async fn a_fill_visible_only_in_the_trades_is_booked_not_called_unknown() {
    // A live measurement on 2026-09-04: the order filled, `get_order` showed zero, and the
    // adapter returned "unknown" — while the trade was sitting in the feed. The feed is the
    // source of truth: it carries both the size and the real price.
    let mock = MockClobClient::new();
    mock.expect_success("o5", "tok", dec!(0.82), dec!(1.5), ClobSide::Buy)
        .await;
    mock.add_open_order("o5", "tok", dec!(0.82), dec!(1.5), ClobSide::Buy)
        .await;
    // size_matched deliberately stays zero
    mock.set_trades(vec![trade("o5", dec!(1.518515), dec!(0.81))])
        .await;

    let adapter = ClobAdapter::new(mock).with_poll(2, std::time::Duration::ZERO);
    let fill = adapter.place_ioc(&req(dec!(1.5))).await.unwrap();

    assert_eq!(fill.size, dec!(1.518515));
    assert_eq!(fill.avg_price, dec!(0.81));
}
