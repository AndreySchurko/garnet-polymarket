//! A baseline measurement turned into a permanent summary: the distribution of entry-queue
//! depth.
//!
//! The first revision of this query **counted refusals as observations**: the window
//! function assigned a depth to every row of `signals`, and a queue of twelve copies
//! interleaved with twelve refusals doubled every bucket. The
//! `docs/fixtures/baseline-seed.sql` fixture caught it, and its expected answer is
//! reproduced here: bucket 1 -> 5, buckets 2..12 -> 1 each.

use chrono::{TimeZone, Utc};
use garnet_db::{Db, Mode, NewLeaderTrade, Side, Source};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;

async fn db(tag: &str) -> Db {
    let db = garnet_db::testing::isolated_db(tag).await.unwrap();
    db.wallets().add("0xw1", None).await.unwrap();
    db.wallets().add("0xw2", None).await.unwrap();
    db
}

/// A signal at a given second. A leader trade of its own is created per signal:
/// `signals.leader_trade_id` is a foreign key.
async fn signal_at(db: &Db, wallet: &str, verdict: &str, secs: i64, tag: &str) {
    let t = db
        .trades()
        .record(&NewLeaderTrade {
            wallet: wallet.into(),
            tx_hash: format!("0x{tag}"),
            token_id: "tok_a".into(),
            side: Side::Buy,
            price: dec!(0.5),
            size: dec!(1),
            ts_trade: Utc::now(),
            source: Source::Rtds,
            market_text: "m".into(),
            outcome_text: "o".into(),
        })
        .await
        .unwrap()
        .fresh()
        .unwrap();
    let s = db
        .signals()
        .record(
            t.id,
            wallet,
            Mode::Live,
            verdict,
            dec!(10),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE signals SET ts_signal = $1 WHERE id = $2")
        .bind(Utc.timestamp_opt(1_789_000_000 + secs, 0).unwrap())
        .bind(s.id)
        .execute(db.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn the_baseline_fixture_answer_is_reproduced() {
    let db = db("fd_fixture").await;

    // w1: a queue of 12 copies one second apart, plus 12 refusals interleaved with them,
    // plus one late copy an hour later.
    for g in 1..=12 {
        signal_at(&db, "0xw1", "copy", g, &format!("c{g}")).await;
    }
    for g in 1..=12 {
        signal_at(&db, "0xw1", "skip:slippage_exceeded", g, &format!("s{g}")).await;
    }
    signal_at(&db, "0xw1", "copy", 3600, "late").await;
    // w2: three copies a minute apart.
    for g in 1..=3 {
        signal_at(&db, "0xw2", "copy", g * 60, &format!("w2_{g}")).await;
    }

    let rows = db.reports().fire_depth(30).await.unwrap();
    let got: HashMap<i64, i64> = rows.iter().map(|r| (r.depth, r.copies)).collect();

    assert_eq!(
        got.get(&1),
        Some(&5),
        "bucket 1: w1's first, w1's late one and three from w2"
    );
    for d in 2..=12 {
        assert_eq!(got.get(&d), Some(&1), "bucket {d}");
    }
    assert_eq!(got.len(), 12, "exactly twelve buckets");
    assert!(rows.iter().all(|r| r.mode == Mode::Live));
}

#[tokio::test]
async fn refusals_are_not_counted_as_depth() {
    // A refusal spends no capital, and its place is beside the depth distribution, not
    // inside it (invariant 33).
    let db = db("fd_skips").await;
    for g in 1..=5 {
        signal_at(&db, "0xw1", "skip:slippage_exceeded", g, &format!("s{g}")).await;
    }
    signal_at(&db, "0xw1", "copy", 6, "c1").await;

    let rows = db.reports().fire_depth(30).await.unwrap();
    assert_eq!(rows.len(), 1, "one copy, one bucket");
    assert_eq!(
        rows[0].depth, 1,
        "the five refusals before it added no depth"
    );
    assert_eq!(rows[0].copies, 1);
}

#[tokio::test]
async fn the_window_changes_the_answer_which_is_why_it_is_an_argument() {
    // The summary is looked at precisely to compare windows with each other: a threshold
    // without a window is meaningless.
    let db = db("fd_window").await;
    for g in 1..=4 {
        signal_at(&db, "0xw1", "copy", g * 20, &format!("c{g}")).await;
    }

    let narrow = db.reports().fire_depth(10).await.unwrap();
    assert!(
        narrow.iter().all(|r| r.depth == 1),
        "within a 10 s window they are one apiece"
    );

    let wide = db.reports().fire_depth(120).await.unwrap();
    let max = wide.iter().map(|r| r.depth).max().unwrap();
    assert_eq!(max, 4, "within a 120 s window this is a queue of four");
}

#[tokio::test]
async fn the_modes_are_counted_apart() {
    let db = db("fd_modes").await;
    let t = db
        .trades()
        .record(&NewLeaderTrade {
            wallet: "0xw1".into(),
            tx_hash: "0xm1".into(),
            token_id: "tok_a".into(),
            side: Side::Buy,
            price: dec!(0.5),
            size: dec!(1),
            ts_trade: Utc::now(),
            source: Source::Rtds,
            market_text: "m".into(),
            outcome_text: "o".into(),
        })
        .await
        .unwrap()
        .fresh()
        .unwrap();
    for mode in [Mode::Live, Mode::Shadow] {
        db.signals()
            .record(t.id, "0xw1", mode, "copy", Decimal::ONE, None, None, None)
            .await
            .unwrap();
    }

    let rows = db.reports().fire_depth(30).await.unwrap();
    assert_eq!(rows.len(), 2, "the modes are not added together");
    assert!(
        rows.iter().all(|r| r.depth == 1),
        "paper does not deepen the real queue"
    );
}
