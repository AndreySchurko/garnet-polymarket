//! The market registry outlives the process.
//!
//! Until 06.09.2026 the `markets` table stood empty: the metadata lived only in the
//! process cache with a ten-minute lifetime. The cost of that is known — a resolved
//! market loses its order book (invariant 18), and the token -> condition path was
//! exactly what we used to fetch from the book. After a restart the only road to the
//! condition is Gamma, and when that is gone settlement has nothing to stand on.

use garnet_db::repos::markets::NewMarket;
use garnet_db::Db;
use rust_decimal_macros::dec;

async fn db(tag: &str) -> Db {
    garnet_db::testing::isolated_db(tag).await.unwrap()
}

fn market(token: &str) -> NewMarket {
    NewMarket {
        token_id: token.into(),
        condition_id: "0xcid".into(),
        question: "Lakers vs Celtics".into(),
        // The outcome label is not Yes/No: crypto pairs use Up/Down, totals use
        // Over/Under, sports use team names.
        outcome_label: "Los Angeles Lakers".into(),
        category: Some("Sports".into()),
        game_start_time: None,
        end_date: None,
        neg_risk: false,
        fee_rate: dec!(0.05),
        fee_exponent: dec!(1),
        fee_taker_only: true,
        resolved_outcome: None,
        closed: false,
    }
}

#[tokio::test]
async fn a_market_is_stored_and_read_back_by_token() {
    let db = db("mkt_rt").await;
    db.markets().upsert(&market("tok_a")).await.unwrap();

    let m = db
        .markets()
        .get("tok_a")
        .await
        .unwrap()
        .expect("the market is recorded");
    assert_eq!(m.condition_id, "0xcid");
    assert_eq!(m.outcome_label, "Los Angeles Lakers");
    assert_eq!(m.fee_rate, dec!(0.05));
}

#[tokio::test]
async fn a_second_sighting_updates_rather_than_duplicates() {
    let db = db("mkt_up").await;
    db.markets().upsert(&market("tok_b")).await.unwrap();

    let mut later = market("tok_b");
    later.closed = true;
    later.resolved_outcome = Some("Los Angeles Lakers".into());
    db.markets().upsert(&later).await.unwrap();

    let m = db.markets().get("tok_b").await.unwrap().unwrap();
    assert!(m.closed, "the market closed — the record must know it");
    assert_eq!(m.resolved_outcome.as_deref(), Some("Los Angeles Lakers"));
    assert_eq!(
        db.markets().count().await.unwrap(),
        1,
        "the key is token_id, duplicates cannot exist"
    );
}

/// The condition for a token — the reason the table exists: once a market has resolved the
/// book answers 404, and there is nowhere left to ask for the condition.
#[tokio::test]
async fn the_condition_survives_the_book_disappearing() {
    let db = db("mkt_cond").await;
    db.markets().upsert(&market("tok_c")).await.unwrap();

    assert_eq!(
        db.markets().condition_of("tok_c").await.unwrap().as_deref(),
        Some("0xcid")
    );
    assert_eq!(
        db.markets().condition_of("tok_missing").await.unwrap(),
        None
    );
}
