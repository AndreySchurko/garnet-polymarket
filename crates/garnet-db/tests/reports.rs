//! Summaries for readers: Telegram first, the dashboard after.
//!
//! They live in `garnet-db` because they are SQL, and only this crate knows SQL.
//! Computed **separately** per mode: mixing a paper result with a real one means losing
//! the only comparison shadow exists for.

use garnet_db::{Db, Mode};
use rust_decimal_macros::dec;

async fn db(tag: &str) -> Db {
    garnet_db::testing::isolated_db(tag).await.unwrap()
}

async fn wallet(db: &Db, addr: &str) {
    db.wallets().add(addr, Some("whale")).await.unwrap();
}

#[tokio::test]
async fn open_positions_exclude_the_settled_ones() {
    let db = db("rep_open").await;
    wallet(&db, "0xrep1").await;
    db.positions()
        .apply_buy("0xrep1", "tok_a", Mode::Live, dec!(10), dec!(4), dec!(0.1))
        .await
        .unwrap();
    let closed = db
        .positions()
        .apply_buy("0xrep1", "tok_b", Mode::Live, dec!(5), dec!(2), dec!(0.05))
        .await
        .unwrap();
    db.settlements()
        .record(closed.id, "tok_b", "Down", false, dec!(0), None)
        .await
        .unwrap();
    db.positions().close(closed.id).await.unwrap();

    let open = db.reports().open_positions().await.unwrap();
    assert_eq!(
        open.len(),
        1,
        "a closed position has no place in the list of open ones"
    );
    assert_eq!(open[0].token_id, "tok_a");
    assert_eq!(open[0].size, dec!(10));
    assert_eq!(open[0].cost_usd, dec!(4));
    assert_eq!(open[0].mode, Mode::Live);
}

#[tokio::test]
async fn realised_pnl_counts_payout_proceeds_cost_and_fees() {
    let db = db("rep_pnl").await;
    wallet(&db, "0xrep2").await;

    // A win: $4 of stake, $0.1 of fees, a $10 payout.
    let win = db
        .positions()
        .apply_buy("0xrep2", "tok_w", Mode::Live, dec!(10), dec!(4), dec!(0.1))
        .await
        .unwrap();
    db.settlements()
        .record(win.id, "tok_w", "Up", true, dec!(10), None)
        .await
        .unwrap();
    db.positions().close(win.id).await.unwrap();

    // A loss: $2 of stake, $0.05 of fees, a payout of zero.
    let lose = db
        .positions()
        .apply_buy("0xrep2", "tok_l", Mode::Live, dec!(5), dec!(2), dec!(0.05))
        .await
        .unwrap();
    db.settlements()
        .record(lose.id, "tok_l", "Down", false, dec!(0), None)
        .await
        .unwrap();
    db.positions().close(lose.id).await.unwrap();

    let pnl = db.reports().realised_pnl(None).await.unwrap();
    let live = pnl
        .iter()
        .find(|r| r.mode == Mode::Live)
        .expect("the live row");

    assert_eq!(live.closed, 2);
    assert_eq!(live.won, 1);
    // 10 − 4 − 0.1 − 2 − 0.05 = 3.85
    assert_eq!(live.pnl_usd, dec!(3.85));
}

#[tokio::test]
async fn a_sale_counts_towards_the_result() {
    // The leader exited before resolution and we followed: proceeds are just as much a
    // result as a payout, and must not be lost.
    let db = db("rep_sale").await;
    wallet(&db, "0xrep3").await;
    let p = db
        .positions()
        .apply_buy("0xrep3", "tok_s", Mode::Live, dec!(10), dec!(4), dec!(0.1))
        .await
        .unwrap();
    db.positions()
        .apply_sell("0xrep3", "tok_s", Mode::Live, dec!(10), dec!(6), dec!(0.1))
        .await
        .unwrap();
    db.positions().close(p.id).await.unwrap();

    let pnl = db.reports().realised_pnl(None).await.unwrap();
    let live = pnl.iter().find(|r| r.mode == Mode::Live).unwrap();
    // 6 − 4 − 0.2 = 1.8
    assert_eq!(live.pnl_usd, dec!(1.8));
}

#[tokio::test]
async fn the_two_modes_are_never_added_together() {
    let db = db("rep_modes").await;
    wallet(&db, "0xrep4").await;
    let l = db
        .positions()
        .apply_buy("0xrep4", "tok_x", Mode::Live, dec!(1), dec!(1), dec!(0))
        .await
        .unwrap();
    let s = db
        .positions()
        .apply_buy("0xrep4", "tok_x", Mode::Shadow, dec!(1), dec!(1), dec!(0))
        .await
        .unwrap();
    db.settlements()
        .record(l.id, "tok_x", "Up", true, dec!(3), None)
        .await
        .unwrap();
    db.settlements()
        .record(s.id, "tok_x", "Up", true, dec!(3), None)
        .await
        .unwrap();
    db.positions().close(l.id).await.unwrap();
    db.positions().close(s.id).await.unwrap();

    let pnl = db.reports().realised_pnl(None).await.unwrap();
    assert_eq!(pnl.len(), 2, "one row per mode");
    for row in &pnl {
        assert_eq!(row.pnl_usd, dec!(2), "{:?}", row.mode);
    }
}

#[tokio::test]
async fn a_period_cuts_off_what_closed_earlier() {
    let db = db("rep_period").await;
    wallet(&db, "0xrep5").await;
    let old = db
        .positions()
        .apply_buy("0xrep5", "tok_old", Mode::Live, dec!(1), dec!(1), dec!(0))
        .await
        .unwrap();
    db.settlements()
        .record(old.id, "tok_old", "Up", true, dec!(5), None)
        .await
        .unwrap();
    db.positions().close(old.id).await.unwrap();
    sqlx::query("UPDATE positions SET closed_at = now() - interval '3 days' WHERE id = $1")
        .bind(old.id)
        .execute(db.pool())
        .await
        .unwrap();

    let since = chrono::Utc::now() - chrono::Duration::hours(24);
    let day = db.reports().realised_pnl(Some(since)).await.unwrap();
    assert!(
        day.is_empty(),
        "something three days old does not fall within a day"
    );

    let all = db.reports().realised_pnl(None).await.unwrap();
    assert_eq!(all.len(), 1);
}

#[tokio::test]
async fn recent_signals_show_the_skips_with_their_reason() {
    // A skip with its reason is the main thing an operator looks for in this list: it
    // answers the question "why did the bot not copy".
    let db = db("rep_sig").await;
    wallet(&db, "0xrep6").await;
    let trade = db
        .trades()
        .record(&garnet_db::NewLeaderTrade {
            wallet: "0xrep6".into(),
            tx_hash: "0xhash1".into(),
            token_id: "tok_sig".into(),
            side: garnet_db::Side::Buy,
            price: dec!(0.4),
            size: dec!(100),
            ts_trade: chrono::Utc::now(),
            source: garnet_db::Source::Rtds,
            market_text: "Lakers vs Celtics".into(),
            outcome_text: "Lakers to win".into(),
        })
        .await
        .unwrap()
        .fresh()
        .unwrap();

    db.signals()
        .record(
            trade.id,
            "0xrep6",
            Mode::Live,
            "skip:insufficient_balance",
            dec!(25),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    db.signals()
        .record(
            trade.id,
            "0xrep6",
            Mode::Shadow,
            "copy",
            dec!(25),
            Some(dec!(0.42)),
            Some(dec!(0.40)),
            Some(dec!(0.38)),
        )
        .await
        .unwrap();

    let rows = db.reports().recent_signals(10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].verdict, "copy", "newest first");
    let skip = rows.iter().find(|r| r.verdict.starts_with("skip")).unwrap();
    assert_eq!(skip.verdict, "skip:insufficient_balance");
    assert_eq!(
        skip.outcome_text, "Lakers to win",
        "a signal without a market is unreadable"
    );
}

#[tokio::test]
async fn pnl_per_wallet_answers_which_one_pays() {
    let db = db("rep_bywallet").await;
    wallet(&db, "0xrep7").await;
    wallet(&db, "0xrep8").await;
    let a = db
        .positions()
        .apply_buy("0xrep7", "tok_1", Mode::Live, dec!(1), dec!(1), dec!(0))
        .await
        .unwrap();
    let b = db
        .positions()
        .apply_buy("0xrep8", "tok_2", Mode::Live, dec!(1), dec!(1), dec!(0))
        .await
        .unwrap();
    db.settlements()
        .record(a.id, "tok_1", "Up", true, dec!(4), None)
        .await
        .unwrap();
    db.settlements()
        .record(b.id, "tok_2", "Down", false, dec!(0), None)
        .await
        .unwrap();
    db.positions().close(a.id).await.unwrap();
    db.positions().close(b.id).await.unwrap();

    let since = chrono::Utc::now() - chrono::Duration::hours(24);
    let by = db.reports().pnl_by_wallet(Some(since)).await.unwrap();
    assert_eq!(by.get("0xrep7").copied(), Some(dec!(3)));
    assert_eq!(by.get("0xrep8").copied(), Some(dec!(-1)));
}
