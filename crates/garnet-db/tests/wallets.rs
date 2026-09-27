use garnet_db::{Db, Mode};
use rust_decimal_macros::dec;

/// The tests run in parallel, so each works on its own address and cleans up only that
/// one: a shared TRUNCATE would race with a neighbouring test.
async fn fresh(addr: &str) -> Db {
    let db = Db::connect(&garnet_db::testing::default_url())
        .await
        .unwrap();
    db.migrate().await.unwrap();
    sqlx::query("DELETE FROM wallets WHERE address = $1")
        .bind(addr)
        .execute(db.pool())
        .await
        .unwrap();
    db
}

#[tokio::test]
async fn add_creates_shadow_and_disabled() {
    let db = fresh("0xadd").await;
    let w = db.wallets().add("0xadd", Some("whale-1")).await.unwrap();
    assert_eq!(w.mode, Mode::Shadow, "a new wallet must be in shadow");
    assert!(!w.enabled, "a new wallet must be disabled");
    assert_eq!(w.stake_usd, dec!(0));
}

#[tokio::test]
async fn every_change_is_audited() {
    let db = fresh("0xaud").await;
    db.wallets().add("0xaud", None).await.unwrap();
    db.wallets().set_stake("0xaud", dec!(25)).await.unwrap();
    db.wallets().set_mode("0xaud", Mode::Live).await.unwrap();

    let events: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT field, old_value, new_value FROM wallet_events WHERE wallet = $1 ORDER BY id",
    )
    .bind("0xaud")
    .fetch_all(db.pool())
    .await
    .unwrap();

    assert_eq!(events.len(), 2, "every change must leave a trace");
    assert_eq!(events[0].0, "stake_usd");
    assert_eq!(
        events[1],
        ("mode".to_string(), "shadow".to_string(), "live".to_string())
    );
}

#[tokio::test]
async fn renaming_leaves_a_trace_naming_who_did_it() {
    // The nickname is the only thing the dashboard changes, and it is a change too:
    // "who called this wallet a whale" must have an answer.
    let db = fresh("0xnick").await;
    db.wallets().add("0xnick", Some("old")).await.unwrap();
    db.wallets()
        .set_nickname("0xnick", "whale-1", "dashboard:42")
        .await
        .unwrap();

    assert_eq!(
        db.wallets()
            .get("0xnick")
            .await
            .unwrap()
            .unwrap()
            .nickname
            .as_deref(),
        Some("whale-1")
    );
    let (field, old, new, actor): (String, String, String, String) = sqlx::query_as(
        "SELECT field, old_value, new_value, actor FROM wallet_events
         WHERE wallet = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind("0xnick")
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        (field.as_str(), old.as_str(), new.as_str(), actor.as_str()),
        ("nickname", "old", "whale-1", "dashboard:42")
    );
}
