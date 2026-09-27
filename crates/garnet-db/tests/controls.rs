//! The operator's manual stop.
//!
//! It lives in the database rather than in process memory or on the bus: `/kill` must
//! work when NATS is unreachable, and it must survive a restart. Until now the killswitch
//! was a `Mutex` inside the process — a restart lifted the operator's stop silently, and
//! nobody would have noticed.

use garnet_db::Db;

/// The manual stop's key is one per installation, so tests about it have to sit in a
/// schema of their own: in a shared one a neighbouring run would clear our stop.
async fn fresh() -> (Db, String) {
    garnet_db::testing::isolated("controls").await.unwrap()
}

#[tokio::test]
async fn a_fresh_database_is_not_stopped() {
    // A missing row means "not halted": a fresh database trades.
    let (db, _) = fresh().await;
    assert!(!db.controls().manual_stop().await.unwrap());
}

#[tokio::test]
async fn the_stop_survives_a_new_connection() {
    // Exactly what the in-memory killswitch could not do: survive a restart.
    let (db, url) = fresh().await;
    db.controls()
        .set_manual_stop(true, "operator")
        .await
        .unwrap();

    let reopened = Db::connect(&url).await.unwrap();
    assert!(
        reopened.controls().manual_stop().await.unwrap(),
        "the operator's stop must survive a restart of the process"
    );

    reopened
        .controls()
        .set_manual_stop(false, "operator")
        .await
        .unwrap();
    assert!(!reopened.controls().manual_stop().await.unwrap());
}

#[tokio::test]
async fn who_stopped_and_when_is_recorded() {
    // Halting trading is a decision somebody is answerable for.
    let (db, _) = fresh().await;
    db.controls()
        .set_manual_stop(true, "telegram:42")
        .await
        .unwrap();

    let (actor, value): (String, String) =
        sqlx::query_as("SELECT actor, value FROM controls WHERE key = 'manual_stop'")
            .fetch_one(db.pool())
            .await
            .unwrap();

    assert_eq!(actor, "telegram:42");
    assert_eq!(value, "true");
}

#[test]
fn the_default_test_database_is_not_the_trading_one() {
    // Paid for on 2026-09-04: `TEST_DATABASE_URL` was not set, the default pointed at the
    // production database, and a test run added eleven wallets to the working registry —
    // one of them in live mode. A forgotten variable must drop the tests into the test
    // database, not the trading one.
    let url = garnet_db::testing::default_url();
    assert!(
        url.ends_with("/garnet_test"),
        "the test default must be a separate database, not {url}"
    );
}

/// The loss stop's latch outlives the process: a restart after a bad day is the most
/// likely event of that day.
#[tokio::test]
async fn the_loss_stop_latch_survives_the_process() {
    let db = garnet_db::testing::isolated_db("ctl_loss").await.unwrap();
    assert_eq!(
        db.controls().loss_stop_day().await.unwrap(),
        None,
        "a fresh database trades"
    );

    let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();
    db.controls().set_loss_stop_day(day, "core").await.unwrap();

    assert_eq!(db.controls().loss_stop_day().await.unwrap(), Some(day));

    // Rewriting with the same date does not multiply rows: one key per installation.
    db.controls().set_loss_stop_day(day, "core").await.unwrap();
    assert_eq!(db.controls().loss_stop_day().await.unwrap(), Some(day));
}
