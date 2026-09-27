//! The trace of a decision has to store the price we saw.
//!
//! Without it the slippage threshold can never be tuned: `skip:slippage_exceeded`
//! recorded only the fact of the refusal, and the miss — how far the price moved past the
//! ceiling — was stored nowhere. Worse, an empty book arrived in the same bucket via an
//! `unwrap_or(1.0)` substitution and was indistinguishable from real slippage.

use chrono::Utc;
use garnet_db::repos::trades::{NewLeaderTrade, Source};
use garnet_db::{Db, Mode, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

async fn db(tag: &str) -> Db {
    garnet_db::testing::isolated_db(tag).await.unwrap()
}

async fn trade(db: &Db, wallet: &str, tx: &str) -> i64 {
    db.wallets().add(wallet, None).await.unwrap();
    db.trades()
        .record(&NewLeaderTrade {
            wallet: wallet.into(),
            tx_hash: tx.into(),
            token_id: "tok_a".into(),
            side: Side::Buy,
            price: dec!(0.30),
            size: dec!(100),
            ts_trade: Utc::now(),
            source: Source::Rtds,
            market_text: "Lakers vs Celtics".into(),
            outcome_text: "Los Angeles Lakers".into(),
        })
        .await
        .unwrap()
        .fresh()
        .unwrap()
        .id
}

#[tokio::test]
async fn a_skipped_signal_keeps_the_price_that_skipped_it() {
    let db = db("sig_skip").await;
    let t = trade(&db, "0xsig1", "0xaa").await;

    let s = db
        .signals()
        .record(
            t,
            "0xsig1",
            Mode::Shadow,
            "skip:slippage_exceeded",
            Decimal::ZERO,
            Some(dec!(0.345)),
            Some(dec!(0.42)),
            Some(dec!(0.41)),
        )
        .await
        .unwrap();

    assert_eq!(s.limit_price, Some(dec!(0.345)), "the ceiling that fired");
    assert_eq!(s.best_ask, Some(dec!(0.42)), "the price that beat it");
    assert_eq!(s.best_bid, Some(dec!(0.41)));
}

#[tokio::test]
async fn a_missing_book_is_stored_as_missing_not_as_one() {
    let db = db("sig_nobook").await;
    let t = trade(&db, "0xsig2", "0xbb").await;

    let s = db
        .signals()
        .record(
            t,
            "0xsig2",
            Mode::Shadow,
            "skip:market_not_tradable",
            Decimal::ZERO,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        s.best_ask, None,
        "there was no price — inventing one is not allowed"
    );
    assert_eq!(s.best_bid, None);
}

#[tokio::test]
async fn a_copy_keeps_the_price_too() {
    let db = db("sig_copy").await;
    let t = trade(&db, "0xsig3", "0xcc").await;

    let s = db
        .signals()
        .record(
            t,
            "0xsig3",
            Mode::Shadow,
            "copy",
            dec!(25),
            Some(dec!(0.345)),
            Some(dec!(0.31)),
            Some(dec!(0.30)),
        )
        .await
        .unwrap();

    assert_eq!(s.best_ask, Some(dec!(0.31)));
    // The tail of the distribution is visible only when the price is recorded on copies
    // too: without them the sample holds only the edge the ceiling refused.
    assert_eq!(s.limit_price, Some(dec!(0.345)));
}

/// When we last followed this wallet with a buy in this market.
///
/// The timestamp is taken from the LEADER'S TRADE, not from our order: a wave is their
/// order breaking against the book, and measuring it by our clock means measuring our own
/// latency instead of their behaviour.
#[tokio::test]
async fn the_last_copied_buy_is_dated_by_the_leaders_clock() {
    let db = db("sig_wave").await;
    db.wallets().add("0xw1", None).await.unwrap();
    let t0 = Utc::now() - chrono::Duration::minutes(30);

    let mk = |tx: &'static str, at: chrono::DateTime<Utc>| NewLeaderTrade {
        wallet: "0xw1".into(),
        tx_hash: tx.into(),
        token_id: "tok_a".into(),
        side: Side::Buy,
        price: dec!(0.30),
        size: dec!(100),
        ts_trade: at,
        source: Source::Rtds,
        market_text: "Lakers vs Celtics".into(),
        outcome_text: "Los Angeles Lakers".into(),
    };

    assert_eq!(
        db.signals()
            .last_copied_buy("0xw1", "tok_a", Mode::Shadow)
            .await
            .unwrap(),
        None,
        "we copied nothing — there is no wave"
    );

    let a = db
        .trades()
        .record(&mk("0xa", t0))
        .await
        .unwrap()
        .fresh()
        .unwrap();
    let s = db
        .signals()
        .record(
            a.id,
            "0xw1",
            Mode::Shadow,
            "copy",
            dec!(25),
            Some(dec!(0.34)),
            Some(dec!(0.30)),
            Some(dec!(0.29)),
        )
        .await
        .unwrap();
    db.signals()
        .record_order(
            Some(s.id),
            "tok_a",
            Mode::Shadow,
            "buy",
            dec!(0.34),
            dec!(25),
            "filled",
            None,
            1,
        )
        .await
        .unwrap();

    let got = db
        .signals()
        .last_copied_buy("0xw1", "tok_a", Mode::Shadow)
        .await
        .unwrap();
    assert_eq!(got.map(|x| x.timestamp()), Some(t0.timestamp()));

    // Another mode and another market do not form a wave of their own.
    assert_eq!(
        db.signals()
            .last_copied_buy("0xw1", "tok_a", Mode::Live)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db.signals()
            .last_copied_buy("0xw1", "tok_b", Mode::Shadow)
            .await
            .unwrap(),
        None
    );
}

/// A refusal from the exchange closes the wave all the same: we have already acted on the
/// leader's decision, and there is no point repeating it with a second slice.
#[tokio::test]
async fn a_rejected_buy_still_marks_the_wave() {
    let db = db("sig_wave_rej").await;
    db.wallets().add("0xw2", None).await.unwrap();
    let t0 = Utc::now();
    let a = db
        .trades()
        .record(&NewLeaderTrade {
            wallet: "0xw2".into(),
            tx_hash: "0xb".into(),
            token_id: "tok_a".into(),
            side: Side::Buy,
            price: dec!(0.30),
            size: dec!(100),
            ts_trade: t0,
            source: Source::Rtds,
            market_text: "M".into(),
            outcome_text: "O".into(),
        })
        .await
        .unwrap()
        .fresh()
        .unwrap();
    let s = db
        .signals()
        .record(
            a.id,
            "0xw2",
            Mode::Shadow,
            "copy",
            dec!(25),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    db.signals()
        .record_order(
            Some(s.id),
            "tok_a",
            Mode::Shadow,
            "buy",
            dec!(0.34),
            dec!(25),
            "rejected",
            Some("minimum order size"),
            1,
        )
        .await
        .unwrap();

    assert!(db
        .signals()
        .last_copied_buy("0xw2", "tok_a", Mode::Shadow)
        .await
        .unwrap()
        .is_some());
}
