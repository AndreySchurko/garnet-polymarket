//! Latency per stage of the signal path (invariant 42).
//!
//! The instrument the hot-path work is judged by. Until 19.09.2026 two of the four stages
//! were declared and never written, while the fourth was written as a constant zero — a
//! distribution of zeros is indistinguishable from an instantaneous path.

use chrono::{Duration, Utc};
use garnet_db::{Db, Mode, NewLeaderTrade, Side, Source};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

async fn db(tag: &str) -> Db {
    let db = garnet_db::testing::isolated_db(tag).await.unwrap();
    db.wallets().add("0xlat", None).await.unwrap();
    db
}

/// The full path of one trade with given latencies per stage.
async fn path(
    db: &Db,
    tag: &str,
    seen: i64,
    signal: i64,
    submit: i64,
    fill: Option<i64>,
    mode: Mode,
) {
    let base = Utc::now() - Duration::hours(1);
    let t = db
        .trades()
        .record(&NewLeaderTrade {
            wallet: "0xlat".into(),
            tx_hash: format!("0x{tag}"),
            token_id: "tok_a".into(),
            side: Side::Buy,
            price: dec!(0.5),
            size: dec!(10),
            ts_trade: base,
            source: Source::Rtds,
            market_text: "m".into(),
            outcome_text: "o".into(),
        })
        .await
        .unwrap()
        .fresh()
        .unwrap();
    sqlx::query("UPDATE leader_trades SET ts_seen = $1 WHERE id = $2")
        .bind(base + Duration::seconds(seen))
        .bind(t.id)
        .execute(db.pool())
        .await
        .unwrap();

    let s = db
        .signals()
        .record(t.id, "0xlat", mode, "copy", dec!(10), None, None, None)
        .await
        .unwrap();
    sqlx::query("UPDATE signals SET ts_signal = $1 WHERE id = $2")
        .bind(base + Duration::seconds(seen + signal))
        .bind(s.id)
        .execute(db.pool())
        .await
        .unwrap();

    let o = db
        .signals()
        .record_order(
            Some(s.id),
            "tok_a",
            mode,
            "buy",
            dec!(0.5),
            dec!(10),
            "filled",
            None,
            1,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE orders SET ts_submitted = $1, ts_filled = $2 WHERE id = $3")
        .bind(base + Duration::seconds(seen + signal + submit))
        .bind(fill.map(|f| base + Duration::seconds(seen + signal + submit + f)))
        .bind(o.id)
        .execute(db.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn every_stage_carries_a_number_not_just_a_count() {
    let db = db("lat_all").await;
    path(&db, "a", 2, 1, 3, Some(4), Mode::Live).await;

    let rows = db.reports().latency(None).await.unwrap();
    assert_eq!(
        rows.len(),
        5,
        "four stages plus the end-to-end one, all with a number"
    );

    let by = |name: &str| rows.iter().find(|r| r.stage == name).unwrap().clone();
    assert_eq!(by("trade_to_seen").median_secs, Some(dec!(2)));
    assert_eq!(by("seen_to_signal").median_secs, Some(dec!(1)));
    assert_eq!(by("signal_to_submitted").median_secs, Some(dec!(3)));
    assert_eq!(by("submitted_to_filled").median_secs, Some(dec!(4)));
    assert!(rows.iter().all(|r| r.n == 1));
}

#[tokio::test]
async fn the_stages_come_in_the_order_the_signal_walks_them() {
    // The reader wants the path, not the alphabet: a jumbled order of stages reads as a
    // jumbled path.
    let db = db("lat_order").await;
    path(&db, "a", 1, 1, 1, Some(1), Mode::Live).await;

    let stages: Vec<String> = db
        .reports()
        .latency(None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.stage)
        .collect();
    assert_eq!(
        stages,
        vec![
            "trade_to_seen",
            "seen_to_signal",
            "signal_to_submitted",
            "submitted_to_filled",
            // The end-to-end figure comes last: it is not a stage of the path but its
            // total.
            "trade_to_submitted",
        ]
    );
}

#[tokio::test]
async fn a_paper_fill_stays_out_of_the_live_latency() {
    // A paper fill is simulated against the book within the same task: its latency is
    // identically zero, and mixing it in would understate the median by shadow's share.
    // shadow.
    let db = db("lat_shadow").await;
    path(&db, "live", 1, 1, 1, Some(10), Mode::Live).await;
    path(&db, "paper", 1, 1, 1, Some(0), Mode::Shadow).await;

    let rows = db.reports().latency(None).await.unwrap();
    let fill = rows
        .iter()
        .find(|r| r.stage == "submitted_to_filled")
        .unwrap();
    assert_eq!(fill.n, 1, "only the live fill entered the measurement");
    assert_eq!(
        fill.median_secs,
        Some(dec!(10)),
        "the paper zero did not move the median"
    );
}

#[tokio::test]
async fn a_clock_that_runs_backwards_does_not_become_negative_latency() {
    // The database's clock and the exchange's drift apart. A negative latency is not
    // "faster than instantaneous" but a clock mismatch, and it has no place in the
    // distribution.
    let db = db("lat_skew").await;
    path(&db, "a", -5, 1, 1, Some(1), Mode::Live).await;

    let rows = db.reports().latency(None).await.unwrap();
    let seen = rows.iter().find(|r| r.stage == "trade_to_seen").unwrap();
    assert_eq!(
        seen.median_secs,
        Some(Decimal::ZERO),
        "clamped to zero, not minus five"
    );
}

#[tokio::test]
async fn a_signal_without_an_order_measures_the_stages_it_has() {
    // A refusal reaches `signals` and does not reach `orders`. It has the first two
    // stages, and they must not be lost because the last two are missing.
    let db = db("lat_skip").await;
    let base = Utc::now() - Duration::hours(1);
    let t = db
        .trades()
        .record(&NewLeaderTrade {
            wallet: "0xlat".into(),
            tx_hash: "0xskip".into(),
            token_id: "tok_a".into(),
            side: Side::Buy,
            price: dec!(0.5),
            size: dec!(10),
            ts_trade: base,
            source: Source::Rtds,
            market_text: "m".into(),
            outcome_text: "o".into(),
        })
        .await
        .unwrap()
        .fresh()
        .unwrap();
    db.signals()
        .record(
            t.id,
            "0xlat",
            Mode::Live,
            "skip:slippage_exceeded",
            Decimal::ZERO,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let rows = db.reports().latency(None).await.unwrap();
    assert_eq!(
        rows.len(),
        2,
        "two stages — the ones that happened; there is no end-to-end figure without an order"
    );
    assert!(rows.iter().any(|r| r.stage == "seen_to_signal" && r.n == 1));
}

#[tokio::test]
async fn the_end_to_end_number_is_measured_not_added_up() {
    // The median of a sum does not equal the sum of medians. The added-up number would
    // resemble the truth without being it — and it is what the target is judged by.
    let db = db("lat_e2e").await;
    path(&db, "a", 3, 1, 2, Some(9), Mode::Live).await; // 6
    path(&db, "b", 5, 1, 3, Some(9), Mode::Live).await; // 9
    path(&db, "c", 1, 9, 1, Some(9), Mode::Live).await; // 11

    let rows = db.reports().latency(None).await.unwrap();
    let e2e = rows
        .iter()
        .find(|r| r.stage == "trade_to_submitted")
        .unwrap();
    assert_eq!(
        e2e.median_secs,
        Some(dec!(9)),
        "the median over trades: 6, 9, 11"
    );

    // The sum of per-stage medians would give a different number — and that is not
    // pedantry but the difference between 9 and 7.
    let sum: Decimal = rows
        .iter()
        .filter(|r| r.stage != "trade_to_submitted" && r.stage != "submitted_to_filled")
        .filter_map(|r| r.median_secs)
        .sum();
    assert_ne!(sum, dec!(9), "adding medians answers the wrong question");
}

#[tokio::test]
async fn the_fill_stage_is_not_part_of_the_way_to_submission() {
    // Submission ends with submission. Including execution in "from trade to order"
    // means reporting a latency that was not on that path.
    let db = db("lat_scope").await;
    path(&db, "a", 1, 1, 1, Some(100), Mode::Live).await;

    let rows = db.reports().latency(None).await.unwrap();
    let e2e = rows
        .iter()
        .find(|r| r.stage == "trade_to_submitted")
        .unwrap();
    assert_eq!(e2e.median_secs, Some(dec!(3)), "3 s, not 103");
}
