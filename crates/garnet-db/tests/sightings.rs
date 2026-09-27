//! The race between delivery circuits (invariant 43).
//!
//! The dedup discards the second copy of a trade correctly, but the only evidence about a
//! circuit's speed was discarded along with it: `insert_new` returned `None`, and the
//! losing delivery vanished without trace. A circuit whose value cannot be measured can
//! neither be switched off nor defended.

use chrono::{Duration, Utc};
use garnet_db::{Db, NewLeaderTrade, Seen, Source};
use garnet_types::Side;
use rust_decimal_macros::dec;

async fn db(tag: &str) -> Db {
    let db = garnet_db::testing::isolated_db(tag).await.unwrap();
    db.wallets().add("0xrace", Some("whale")).await.unwrap();
    db
}

fn trade(tx: &str, source: Source) -> NewLeaderTrade {
    NewLeaderTrade {
        wallet: "0xrace".into(),
        tx_hash: tx.into(),
        token_id: "tok_a".into(),
        side: Side::Buy,
        price: dec!(0.42),
        size: dec!(100),
        ts_trade: Utc::now(),
        source,
        market_text: "Lakers vs Celtics".into(),
        outcome_text: "Lakers".into(),
    }
}

async fn sightings_of(db: &Db, trade_id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM trade_sightings WHERE leader_trade_id = $1")
        .bind(trade_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn two_deliveries_are_one_trade_and_two_sightings() {
    let db = db("sight_two").await;

    let first = db
        .trades()
        .record(&trade("0xtx1", Source::Rtds))
        .await
        .unwrap();
    let id = match first {
        Seen::First(t) => t.id,
        other => panic!("the first delivery must be the first: {other:?}"),
    };

    let second = db
        .trades()
        .record(&trade("0xtx1", Source::Poll))
        .await
        .unwrap();
    match second {
        Seen::Again(t) => assert_eq!(t.id, id, "the repeat is attached to the same trade"),
        other => panic!("the second delivery is not a trade but a sighting: {other:?}"),
    }

    let trades: i64 = sqlx::query_scalar("SELECT count(*) FROM leader_trades")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(trades, 1, "one on-chain trade, one row");
    assert_eq!(sightings_of(&db, id).await, 2, "but two sightings");
}

#[tokio::test]
async fn a_repeat_from_the_same_source_adds_no_sighting() {
    // The poll runs every few seconds and brings the same trade back twenty times in a
    // row. Counting that as twenty sightings would declare it twenty times more useful
    // than it is.
    let db = db("sight_repeat").await;
    let id = db
        .trades()
        .record(&trade("0xtx2", Source::Poll))
        .await
        .unwrap()
        .fresh()
        .unwrap()
        .id;

    for _ in 0..5 {
        db.trades()
            .record(&trade("0xtx2", Source::Poll))
            .await
            .unwrap();
    }

    assert_eq!(
        sightings_of(&db, id).await,
        1,
        "the same circuit, one sighting"
    );
}

#[tokio::test]
async fn the_lag_is_measured_from_the_first_sighting_not_from_the_insert() {
    // Lag is the difference between circuits, not the age of a row. Measuring it from the
    // insert means measuring when we wrote it, not when we saw it.
    let db = db("sight_lag").await;
    let id = db
        .trades()
        .record(&trade("0xtx3", Source::Rtds))
        .await
        .unwrap()
        .fresh()
        .unwrap()
        .id;
    db.trades()
        .record(&trade("0xtx3", Source::Poll))
        .await
        .unwrap();

    // We shift the sightings into the past: the socket saw it 10 seconds before the poll,
    // while both rows were written just now.
    let base = Utc::now() - Duration::hours(2);
    sqlx::query("UPDATE trade_sightings SET ts_seen = $1 WHERE source = 'rtds'")
        .bind(base)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE trade_sightings SET ts_seen = $1 WHERE source = 'poll'")
        .bind(base + Duration::seconds(10))
        .execute(db.pool())
        .await
        .unwrap();

    let race = db.reports().source_race(None).await.unwrap();
    let rtds = race.iter().find(|r| r.source == "rtds").unwrap();
    let poll = race.iter().find(|r| r.source == "poll").unwrap();

    assert_eq!(rtds.wins, 1, "the socket saw it first");
    assert_eq!(rtds.confirmations, 0);
    assert_eq!(
        rtds.median_lag_secs,
        Some(dec!(0)),
        "the first one has no lag"
    );

    assert_eq!(poll.wins, 0);
    assert_eq!(poll.confirmations, 1, "the poll merely confirmed");
    assert_eq!(
        poll.median_lag_secs,
        Some(dec!(10)),
        "lag behind the first, not behind the insert"
    );
    let _ = id;
}

#[tokio::test]
async fn a_circuit_that_brings_nothing_of_its_own_says_so() {
    // The very question the report exists for: does the poll pay for itself. Zero unique
    // trades means it is paying in backfill duplicates for nothing.
    let db = db("sight_unique").await;

    // Two trades were brought by the socket, and the poll merely duplicated them.
    for tx in ["0xa1", "0xa2"] {
        db.trades().record(&trade(tx, Source::Rtds)).await.unwrap();
        db.trades().record(&trade(tx, Source::Poll)).await.unwrap();
    }
    // The third was brought by nobody but the poll.
    db.trades()
        .record(&trade("0xa3", Source::Poll))
        .await
        .unwrap();

    let race = db.reports().source_race(None).await.unwrap();
    let rtds = race.iter().find(|r| r.source == "rtds").unwrap();
    let poll = race.iter().find(|r| r.source == "poll").unwrap();

    assert_eq!(
        rtds.only_source, 0,
        "everything the socket brought, the poll brought too"
    );
    assert_eq!(rtds.sightings, 2);
    assert_eq!(
        poll.only_source, 1,
        "one trade was brought by the poll alone"
    );
    assert_eq!(poll.sightings, 3);
    assert_eq!(poll.wins, 1, "and on that one it is first");
}

#[tokio::test]
async fn nothing_seen_is_an_empty_report_not_a_zero_row() {
    let db = db("sight_empty").await;
    assert!(db.reports().source_race(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_chain_is_a_third_delivery_not_a_third_truth() {
    // Invariant 41. The log carries the same `transactionHash` as the socket frame and
    // collapses on the same key. The circuit is not a third source of truth — it is a
    // third delivery of one truth.
    let db = db("sight_chain").await;

    let first = db
        .trades()
        .record(&trade("0xtx_c", Source::Rtds))
        .await
        .unwrap();
    let id = match first {
        Seen::First(t) => t.id,
        other => panic!("{other:?}"),
    };
    let again = db
        .trades()
        .record(&trade("0xtx_c", Source::Chain))
        .await
        .unwrap();
    assert!(
        matches!(again, Seen::Again(t) if t.id == id),
        "the dedup collapsed them"
    );

    let trades: i64 = sqlx::query_scalar("SELECT count(*) FROM leader_trades")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(trades, 1, "one on-chain trade, one row");
    assert_eq!(sightings_of(&db, id).await, 2, "but two sightings");

    let race = db.reports().source_race(None).await.unwrap();
    assert!(
        race.iter()
            .any(|r| r.source == "chain" && r.confirmations == 1),
        "the chain circuit appears in the circuit race as a row of its own"
    );
}
