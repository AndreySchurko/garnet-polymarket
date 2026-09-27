//! A deferred exit has to remember what to aim at.
//!
//! Shares without a price are half a decision: the floor was computed from the leader's
//! sale price, and that lived in the trade frame and was stored nowhere. A retry picking
//! up such a row would not know its own floor and would sell at any price — that is, it
//! would decide afresh what the leader had already decided.

use garnet_db::{Db, Mode};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

async fn db(tag: &str) -> Db {
    garnet_db::testing::isolated_db(tag).await.unwrap()
}

async fn position(db: &Db, wallet: &str, token: &str) -> garnet_db::Position {
    db.positions()
        .apply_buy(wallet, token, Mode::Shadow, dec!(50), dec!(15), dec!(0.1))
        .await
        .unwrap()
}

#[tokio::test]
async fn pending_exit_keeps_the_price_it_was_priced_at() {
    let db = db("pos_pend_price").await;
    db.wallets().add("0xpend1", None).await.unwrap();
    let p = position(&db, "0xpend1", "tok_a").await;

    db.positions()
        .set_pending_exit(p.id, dec!(12.5), dec!(0.42))
        .await
        .unwrap();

    let got = db
        .positions()
        .get("0xpend1", "tok_a", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.pending_exit_shares, dec!(12.5));
    assert_eq!(got.pending_exit_price, dec!(0.42));
}

#[tokio::test]
async fn clearing_the_shares_clears_the_price() {
    let db = db("pos_pend_clear").await;
    db.wallets().add("0xpend2", None).await.unwrap();
    let p = position(&db, "0xpend2", "tok_a").await;
    db.positions()
        .set_pending_exit(p.id, dec!(12.5), dec!(0.42))
        .await
        .unwrap();

    db.positions()
        .set_pending_exit(p.id, Decimal::ZERO, dec!(0.42))
        .await
        .unwrap();

    let got = db
        .positions()
        .get("0xpend2", "tok_a", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.pending_exit_shares, Decimal::ZERO);
    // A floor without shares is junk that a retry would mistake for work.
    assert_eq!(got.pending_exit_price, Decimal::ZERO);
}

#[tokio::test]
async fn pending_open_lists_only_what_can_be_retried() {
    let db = db("pos_pend_list").await;
    db.wallets().add("0xpend3", None).await.unwrap();
    let with_price = position(&db, "0xpend3", "tok_a").await;
    let no_price = position(&db, "0xpend3", "tok_b").await;
    let nothing_owed = position(&db, "0xpend3", "tok_c").await;

    db.positions()
        .set_pending_exit(with_price.id, dec!(5), dec!(0.42))
        .await
        .unwrap();
    // A row from before migration 8: shares present, no floor. Nothing to aim at — we skip it.
    sqlx_set_pending_without_price(&db, no_price.id).await;
    let _ = nothing_owed;

    let rows = db.positions().pending_open(50).await.unwrap();

    let ids: Vec<i64> = rows.iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![with_price.id]);
}

#[tokio::test]
async fn a_closed_position_is_not_retried() {
    let db = db("pos_pend_closed").await;
    db.wallets().add("0xpend4", None).await.unwrap();
    let p = position(&db, "0xpend4", "tok_a").await;
    db.positions()
        .set_pending_exit(p.id, dec!(5), dec!(0.42))
        .await
        .unwrap();
    db.positions().close(p.id).await.unwrap();

    assert!(db.positions().pending_open(50).await.unwrap().is_empty());
}

/// Writes deferred shares bypassing the repository, without a price.
///
/// This is what rows left over from migration 7 look like: the price column did not exist
/// then, and they cannot be zeroed out — we still owe the leader that fraction.
async fn sqlx_set_pending_without_price(db: &Db, id: i64) {
    sqlx::query(
        "UPDATE positions SET pending_exit_shares = 5, pending_exit_price = 0 WHERE id = $1",
    )
    .bind(id)
    .execute(db.pool())
    .await
    .unwrap();
}
