//! Reconciliation with attribution of orders in flight (invariants 47 and 48).
//!
//! Between submission and fill, the chain and the ledger diverge **by construction**.
//! A reconciler that shouts about that shouts often; a reconciler that shouts often
//! stops being read — and it is the only mechanism that notices a real
//! divergence.

use garnet_core::reconcile::{
    judge, reconcile_once, tolerance_for, AlertSink, ChainBalances, InFlight, Verdict,
};
use garnet_db::{Db, Mode};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::Mutex;

const WINDOW: i64 = 900;

async fn db(tag: &str) -> Db {
    let db = garnet_db::testing::isolated_db(tag).await.unwrap();
    db.wallets().add("0xrec", None).await.unwrap();
    db
}

struct StubChain(HashMap<String, Decimal>);

impl ChainBalances for StubChain {
    async fn balance_of(&self, token_id: &str) -> anyhow::Result<Decimal> {
        Ok(self.0.get(token_id).copied().unwrap_or(Decimal::ZERO))
    }
}

/// A chain that does not answer. This is not a zero balance — it is the absence of an answer.
struct DeadChain;

impl ChainBalances for DeadChain {
    async fn balance_of(&self, _token_id: &str) -> anyhow::Result<Decimal> {
        anyhow::bail!("the RPC did not answer")
    }
}

struct NoMeta;

impl garnet_core::detect::MarketSource for NoMeta {
    async fn get(&self, _token_id: &str) -> anyhow::Result<garnet_core::market_meta::MarketMeta> {
        anyhow::bail!("no metadata")
    }
}

#[derive(Default)]
struct SpyAlerts(Mutex<Vec<(String, String)>>);

impl AlertSink for SpyAlerts {
    async fn alert(&self, subject: &str, text: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().push((subject.into(), text.into()));
        Ok(())
    }
}

impl SpyAlerts {
    fn published(&self, subject: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| s == subject)
            .count()
    }
}

// ---------------------------------------------------------------------------
// The pure logic of the verdict
// ---------------------------------------------------------------------------

#[test]
fn an_in_flight_sell_explains_a_negative_delta() {
    // The chain holds less than the ledger: the tokens have gone and we have not
    // booked the fill yet.
    let v = judge(
        dec!(100),
        Some(dec!(80)),
        InFlight {
            buy: Decimal::ZERO,
            sell: dec!(20),
        },
    );
    assert!(
        matches!(v, Verdict::Explained { in_flight, .. } if in_flight == dec!(20)),
        "a sale in flight explains the shortfall: {v:?}"
    );
}

#[test]
fn an_in_flight_buy_explains_a_positive_delta() {
    // The chain holds more: a buy was taken on and has not reached the ledger yet.
    let v = judge(
        dec!(100),
        Some(dec!(120)),
        InFlight {
            buy: dec!(20),
            sell: Decimal::ZERO,
        },
    );
    assert!(matches!(v, Verdict::Explained { .. }), "{v:?}");
}

#[test]
fn a_buy_in_flight_does_not_explain_missing_tokens() {
    // A buy brings tokens in, it does not take them away. Explaining a shortfall by
    // one means closing our eyes to a loss with precisely what does not bear on it.
    let v = judge(
        dec!(100),
        Some(dec!(80)),
        InFlight {
            buy: dec!(50),
            sell: Decimal::ZERO,
        },
    );
    assert!(
        matches!(v, Verdict::Unattributed { delta } if delta == dec!(-20)),
        "the shortfall stays an alarm: {v:?}"
    );
}

#[test]
fn a_delta_larger_than_the_order_stays_an_alarm() {
    let v = judge(
        dec!(100),
        Some(dec!(50)),
        InFlight {
            buy: Decimal::ZERO,
            sell: dec!(20),
        },
    );
    assert!(
        matches!(v, Verdict::Unattributed { delta } if delta == dec!(-50)),
        "an order for 20 does not explain 50 going missing: {v:?}"
    );
}

#[test]
fn an_incomplete_read_is_unknown_never_agreed() {
    // Invariant 47. A reconciler that failed to read a balance must say "the check
    // could not be performed". A zero in place of an answer would turn it into a
    // divergence, and `Agreed` into a successful check that never happened.
    assert_eq!(
        judge(dec!(100), None, InFlight::default()),
        Verdict::Unknown
    );
    assert_ne!(judge(dec!(100), None, InFlight::default()), Verdict::Agreed);
}

#[test]
fn the_tolerance_grows_with_the_position() {
    // Invariant 48. An absolute tolerance on a position of a hundred thousand shares
    // is zero; on a position of three shares it is everything.
    assert_eq!(
        tolerance_for(dec!(3)),
        dec!(0.01),
        "a small position is held by the minimum"
    );
    assert_eq!(
        tolerance_for(dec!(100000)),
        dec!(100),
        "a large one by a share of its size"
    );
    assert!(tolerance_for(dec!(100000)) > tolerance_for(dec!(1000)));

    // And the tolerance really works: a hundredth on a large position is not news.
    assert_eq!(
        judge(dec!(100000), Some(dec!(99950)), InFlight::default()),
        Verdict::Agreed
    );
    // While on a small one the same absolute difference is news.
    assert!(matches!(
        judge(dec!(100), Some(dec!(50)), InFlight::default()),
        Verdict::Unattributed { .. }
    ));
}

// ---------------------------------------------------------------------------
// A full pass
// ---------------------------------------------------------------------------

async fn open_position(db: &Db, token: &str, size: Decimal) {
    db.positions()
        .apply_buy(
            "0xrec",
            token,
            Mode::Live,
            size,
            size / dec!(2),
            Decimal::ZERO,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn an_order_in_flight_keeps_the_reconcile_quiet() {
    let db = db("rec_flight").await;
    open_position(&db, "tok_a", dec!(100)).await;
    // An order to sell 20 shares at 0.50 — in flight, outcome unknown.
    db.signals()
        .record_order(
            None,
            "tok_a",
            Mode::Live,
            "sell",
            dec!(0.50),
            dec!(10),
            "unknown",
            None,
            1,
        )
        .await
        .unwrap();

    let chain = StubChain(HashMap::from([("tok_a".to_string(), dec!(80))]));
    let alerts = SpyAlerts::default();
    let r = reconcile_once(&db, &chain, &NoMeta, &alerts, WINDOW)
        .await
        .unwrap();

    assert_eq!(r.explained, 1, "the divergence is explained by an order");
    assert!(r.divergences.is_empty(), "and is not an alarm");
    assert_eq!(alerts.published("alert.reconcile_divergence"), 0);
}

#[tokio::test]
async fn an_expired_order_stops_explaining_anything() {
    // Nobody ever changes the `unknown` status. Without an expiry such a row would
    // explain a genuine loss of tokens forever.
    let db = db("rec_expired").await;
    open_position(&db, "tok_a", dec!(100)).await;
    let o = db
        .signals()
        .record_order(
            None,
            "tok_a",
            Mode::Live,
            "sell",
            dec!(0.50),
            dec!(10),
            "unknown",
            None,
            1,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE orders SET ts_submitted = now() - interval '2 hours' WHERE id = $1")
        .bind(o.id)
        .execute(db.pool())
        .await
        .unwrap();

    let chain = StubChain(HashMap::from([("tok_a".to_string(), dec!(80))]));
    let alerts = SpyAlerts::default();
    let r = reconcile_once(&db, &chain, &NoMeta, &alerts, WINDOW)
        .await
        .unwrap();

    assert_eq!(r.explained, 0, "an expired order explains nothing");
    assert_eq!(r.divergences.len(), 1, "the divergence is an alarm again");
}

#[tokio::test]
async fn a_zero_window_turns_the_explanation_off() {
    let db = db("rec_off").await;
    open_position(&db, "tok_a", dec!(100)).await;
    db.signals()
        .record_order(
            None,
            "tok_a",
            Mode::Live,
            "sell",
            dec!(0.50),
            dec!(10),
            "unknown",
            None,
            1,
        )
        .await
        .unwrap();

    let chain = StubChain(HashMap::from([("tok_a".to_string(), dec!(80))]));
    let alerts = SpyAlerts::default();
    let r = reconcile_once(&db, &chain, &NoMeta, &alerts, 0)
        .await
        .unwrap();

    assert_eq!(r.explained, 0);
    assert_eq!(
        r.divergences.len(),
        1,
        "a zero window restores the earlier behaviour"
    );
}

#[tokio::test]
async fn an_unreadable_balance_costs_only_itself() {
    // Previously `?` brought down the whole pass: one unreadable token left ALL the
    // others unchecked, and the report said nothing about it.
    let db = db("rec_dead").await;
    open_position(&db, "tok_a", dec!(100)).await;
    open_position(&db, "tok_b", dec!(50)).await;

    let alerts = SpyAlerts::default();
    let r = reconcile_once(&db, &DeadChain, &NoMeta, &alerts, WINDOW)
        .await
        .unwrap();

    assert_eq!(r.checked, 2, "the pass reached both positions");
    assert_eq!(r.unreadable.len(), 2, "both are named as unread");
    assert!(r.divergences.is_empty(), "\"not read\" is not \"diverged\"");
    assert_eq!(
        alerts.published("alert.reconcile_unreadable"),
        2,
        "staying silent about what was not read is not allowed (invariant 47)"
    );
}

#[tokio::test]
async fn a_rejected_order_explains_nothing() {
    // A rejected order is not in flight: the exchange did not accept it, and no tokens
    // moved on it in either direction.
    let db = db("rec_rejected").await;
    open_position(&db, "tok_a", dec!(100)).await;
    db.signals()
        .record_order(
            None,
            "tok_a",
            Mode::Live,
            "sell",
            dec!(0.50),
            dec!(10),
            "rejected",
            Some("no bids"),
            1,
        )
        .await
        .unwrap();

    let chain = StubChain(HashMap::from([("tok_a".to_string(), dec!(80))]));
    let alerts = SpyAlerts::default();
    let r = reconcile_once(&db, &chain, &NoMeta, &alerts, WINDOW)
        .await
        .unwrap();

    assert_eq!(r.explained, 0);
    assert_eq!(r.divergences.len(), 1);
}
