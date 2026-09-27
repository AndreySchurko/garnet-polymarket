//! The quality of copying against the quality of the leader (invariants 49 and 50).
//!
//! These tests were written before the module. Each of them is a claim from the
//! specification rather than a check that the code does what it does.

use chrono::{Duration, Utc};
use garnet_db::{Db, Mode, NewLeaderTrade, Source};
use garnet_types::Side;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

async fn db(tag: &str) -> Db {
    garnet_db::testing::isolated_db(tag).await.unwrap()
}

async fn wallet(db: &Db, addr: &str) {
    db.wallets().add(addr, Some("whale")).await.unwrap();
}

/// A leader trade. `ago_secs` counts back from "now": the wallet was assigned at
/// `created_at`, and the tests about invariant 25 have to be able to place a trade on
/// either side of it.
#[allow(clippy::too_many_arguments)]
async fn his_trade(
    db: &Db,
    wallet: &str,
    token: &str,
    side: Side,
    price: Decimal,
    size: Decimal,
    ago_secs: i64,
    tag: &str,
) {
    db.trades()
        .record(&NewLeaderTrade {
            wallet: wallet.into(),
            tx_hash: format!("0xtx_{tag}"),
            token_id: token.into(),
            side,
            price,
            size,
            ts_trade: Utc::now() - Duration::seconds(ago_secs),
            source: Source::Rtds,
            market_text: "Who will win".into(),
            outcome_text: "Up".into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn the_gap_is_positive_when_he_is_ahead() {
    let db = db("mu_ahead").await;
    wallet(&db, "0xm1").await;

    // Their half: 100 shares at $0.40, the market resolved in their favour.
    // (100 − 40) / 40 = +150%.
    his_trade(
        &db,
        "0xm1",
        "tok_a",
        Side::Buy,
        dec!(0.40),
        dec!(100),
        0,
        "a1",
    )
    .await;

    // Ours: 10 shares for $5 (that is, at $0.50), payout $10 -> (10 - 5) / 5 = +100%.
    let p = db
        .positions()
        .apply_buy("0xm1", "tok_a", Mode::Live, dec!(10), dec!(5), dec!(0))
        .await
        .unwrap();
    db.settlements()
        .record(p.id, "tok_a", "Up", true, dec!(10), None)
        .await
        .unwrap();
    db.positions().close(p.id).await.unwrap();

    let rep = db.matchup().report(Mode::Live, None).await.unwrap();
    let row = rep
        .rows
        .iter()
        .find(|r| r.token_id == "tok_a")
        .expect("the row is there");

    assert_eq!(row.his_pct, Some(dec!(150)), "their half");
    assert_eq!(row.our_pct, Some(dec!(100)), "our half");
    assert_eq!(row.gap(), Some(dec!(50)), "they are 50 pp ahead");
    // We entered at 0.50 against their 0.40 — ten cents against us.
    assert_eq!(row.entry_diff_c(), Some(dec!(10)), "signed against us");
}

#[tokio::test]
async fn the_gap_is_negative_when_we_are_ahead() {
    let db = db("mu_we").await;
    wallet(&db, "0xm2").await;

    his_trade(
        &db,
        "0xm2",
        "tok_b",
        Side::Buy,
        dec!(0.50),
        dec!(100),
        0,
        "b1",
    )
    .await;

    // We entered cheaper: 10 shares for $3 -> at $0.30.
    let p = db
        .positions()
        .apply_buy("0xm2", "tok_b", Mode::Live, dec!(10), dec!(3), dec!(0))
        .await
        .unwrap();
    db.settlements()
        .record(p.id, "tok_b", "Up", true, dec!(10), None)
        .await
        .unwrap();
    db.positions().close(p.id).await.unwrap();

    let rep = db.matchup().report(Mode::Live, None).await.unwrap();
    let row = rep.rows.iter().find(|r| r.token_id == "tok_b").unwrap();

    // Theirs: (100 - 50)/50 = +100%. Ours: (10 - 3)/3 = +233.33%.
    assert_eq!(row.his_pct, Some(dec!(100)));
    assert!(
        row.gap().unwrap() < Decimal::ZERO,
        "we are ahead — the gap is negative"
    );
    assert_eq!(
        row.entry_diff_c(),
        Some(dec!(-20)),
        "we entered 20 cents better"
    );
}

#[tokio::test]
async fn an_absent_price_is_none_and_never_zero() {
    // Zero reads as "copied perfectly". Nothing to measure with is None.
    let db = db("mu_none").await;
    wallet(&db, "0xm3").await;

    // They entered, we opened no position at all: our price does not exist.
    his_trade(
        &db,
        "0xm3",
        "tok_c",
        Side::Buy,
        dec!(0.40),
        dec!(100),
        0,
        "c1",
    )
    .await;

    let rep = db.matchup().report(Mode::Live, None).await.unwrap();
    let row = rep.rows.iter().find(|r| r.token_id == "tok_c").unwrap();

    assert_eq!(row.our_avg, Decimal::ZERO, "there is no price of ours");
    assert_eq!(row.entry_diff_c(), None, "None, not zero");
    assert_eq!(row.our_pct, None);
    assert_eq!(
        row.gap(),
        None,
        "half of the gap is missing — there is no gap"
    );
}

#[tokio::test]
async fn the_denominator_starts_at_assignment_not_at_his_first_trade_ever() {
    // Invariant 25: comparing ourselves against entries we could not by construction have
    // seen means measuring the moment the wallet was assigned, not execution.
    let db = db("mu_since").await;
    wallet(&db, "0xm4").await;

    // Before the assignment — a cheap buy we never saw.
    his_trade(
        &db,
        "0xm4",
        "tok_d",
        Side::Buy,
        dec!(0.10),
        dec!(100),
        3600,
        "d_old",
    )
    .await;
    // After the assignment — at 0.40.
    his_trade(
        &db,
        "0xm4",
        "tok_d",
        Side::Buy,
        dec!(0.40),
        dec!(100),
        0,
        "d_new",
    )
    .await;

    let p = db
        .positions()
        .apply_buy("0xm4", "tok_d", Mode::Live, dec!(10), dec!(4), dec!(0))
        .await
        .unwrap();
    db.settlements()
        .record(p.id, "tok_d", "Up", true, dec!(10), None)
        .await
        .unwrap();
    db.positions().close(p.id).await.unwrap();

    let rep = db.matchup().report(Mode::Live, None).await.unwrap();
    let row = rep.rows.iter().find(|r| r.token_id == "tok_d").unwrap();

    assert_eq!(
        row.his_avg_since,
        dec!(0.40),
        "the average since assignment, not 0.25"
    );
    assert_eq!(
        row.his_size,
        dec!(100),
        "their volume is also since the assignment"
    );
    assert_eq!(
        row.entry_diff_c(),
        Some(dec!(0)),
        "we entered at their price"
    );
}

#[tokio::test]
async fn our_half_includes_the_fee() {
    // Invariant 4: on a market with a fee rate, a gap without the fee is our own
    // undercount, not an execution gap.
    let db = db("mu_fee").await;
    wallet(&db, "0xm5").await;

    his_trade(
        &db,
        "0xm5",
        "tok_e",
        Side::Buy,
        dec!(0.50),
        dec!(100),
        0,
        "e1",
    )
    .await;

    // 10 shares for $5 plus $0.50 in fees -> an effective entry price of $0.55.
    let p = db
        .positions()
        .apply_buy("0xm5", "tok_e", Mode::Live, dec!(10), dec!(5), dec!(0.5))
        .await
        .unwrap();
    db.settlements()
        .record(p.id, "tok_e", "Up", true, dec!(10), None)
        .await
        .unwrap();
    db.positions().close(p.id).await.unwrap();

    let rep = db.matchup().report(Mode::Live, None).await.unwrap();
    let row = rep.rows.iter().find(|r| r.token_id == "tok_e").unwrap();

    assert_eq!(row.our_avg, dec!(0.55), "the fee is part of our price");
    assert_eq!(
        row.entry_diff_c(),
        Some(dec!(5)),
        "without the fee this would be zero"
    );
}

#[tokio::test]
async fn a_skip_is_a_row_of_its_own_and_stays_out_of_the_average() {
    // Invariant 50: the average gap over the positions taken flatters us by exactly the
    // discarded tail.
    let db = db("mu_skip").await;
    wallet(&db, "0xm6").await;

    // A position taken: we entered 10 cents worse.
    his_trade(
        &db,
        "0xm6",
        "tok_f",
        Side::Buy,
        dec!(0.40),
        dec!(100),
        0,
        "f1",
    )
    .await;
    db.positions()
        .apply_buy("0xm6", "tok_f", Mode::Live, dec!(10), dec!(5), dec!(0))
        .await
        .unwrap();

    // A skipped one: they entered, we refused on slippage.
    his_trade(
        &db,
        "0xm6",
        "tok_g",
        Side::Buy,
        dec!(0.30),
        dec!(100),
        0,
        "g1",
    )
    .await;
    let t = db.trades().count_for_wallet("0xm6").await.unwrap();
    assert_eq!(t, 2, "both leader trades are recorded");
    let trade_g: i64 = sqlx::query_scalar(
        "SELECT id FROM leader_trades WHERE token_id = 'tok_g' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    db.signals()
        .record(
            trade_g,
            "0xm6",
            Mode::Live,
            "skip:slippage_exceeded",
            dec!(0),
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let rep = db.matchup().report(Mode::Live, None).await.unwrap();

    assert_eq!(rep.n_skipped, 1, "the skip is shown as a separate line");
    assert_eq!(rep.n_measurable, 1, "only the one taken is measurable");
    let skipped = rep.rows.iter().find(|r| r.token_id == "tok_g").unwrap();
    assert!(
        matches!(&skipped.status, garnet_db::MatchupStatus::Skipped { reason }
                 if reason == "skip:slippage_exceeded"),
        "the reason for the refusal is preserved"
    );
    // The average is computed over the measurable row only: the skip's -10 cents do not
    // enter it, because they do not exist.
    assert_eq!(rep.avg_entry_diff_c, Some(dec!(10)));
}

#[tokio::test]
async fn the_two_modes_are_never_added_together() {
    let db = db("mu_modes").await;
    wallet(&db, "0xm7").await;
    his_trade(
        &db,
        "0xm7",
        "tok_h",
        Side::Buy,
        dec!(0.50),
        dec!(100),
        0,
        "h1",
    )
    .await;

    // The live one entered at 0.60, the paper one at 0.50.
    db.positions()
        .apply_buy("0xm7", "tok_h", Mode::Live, dec!(10), dec!(6), dec!(0))
        .await
        .unwrap();
    db.positions()
        .apply_buy("0xm7", "tok_h", Mode::Shadow, dec!(10), dec!(5), dec!(0))
        .await
        .unwrap();

    let live = db.matchup().report(Mode::Live, None).await.unwrap();
    let shadow = db.matchup().report(Mode::Shadow, None).await.unwrap();

    assert_eq!(live.rows.len(), 1);
    assert_eq!(shadow.rows.len(), 1);
    assert_eq!(live.rows[0].our_avg, dec!(0.60));
    assert_eq!(shadow.rows[0].our_avg, dec!(0.50));
    assert_eq!(live.avg_entry_diff_c, Some(dec!(10)));
    assert_eq!(shadow.avg_entry_diff_c, Some(dec!(0)));
}

#[tokio::test]
async fn a_trade_never_weighed_in_this_mode_is_not_a_skip() {
    // A skip is our own refusal, and it measures the truncated tail of copying (invariant
    // 50). A leader trade on a wallet that was running in another mode contains no
    // decision of ours at all. Adding them together would inflate the tail by exactly
    // somebody else's mode — the same conflation of two outcomes as a zero in place of
    // `None`.
    let db = db("mu_uneval").await;
    wallet(&db, "0xm8").await;

    // They entered twice. The live mode weighed one and rejected it, and took the other.
    his_trade(
        &db,
        "0xm8",
        "tok_i",
        Side::Buy,
        dec!(0.30),
        dec!(100),
        0,
        "i1",
    )
    .await;
    his_trade(
        &db,
        "0xm8",
        "tok_j",
        Side::Buy,
        dec!(0.40),
        dec!(100),
        0,
        "j1",
    )
    .await;

    let t_i: i64 = sqlx::query_scalar(
        "SELECT id FROM leader_trades WHERE token_id = 'tok_i' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    db.signals()
        .record(
            t_i,
            "0xm8",
            Mode::Live,
            "skip:slippage_exceeded",
            dec!(0),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    db.positions()
        .apply_buy("0xm8", "tok_j", Mode::Live, dec!(10), dec!(4), dec!(0))
        .await
        .unwrap();

    let live = db.matchup().report(Mode::Live, None).await.unwrap();
    assert_eq!(live.n_skipped, 1, "in live mode exactly one was rejected");
    assert_eq!(live.n_unevaluated, 0, "the live mode saw both trades");

    // In paper mode not a single decision was taken: zero skips, not two.
    let shadow = db.matchup().report(Mode::Shadow, None).await.unwrap();
    assert_eq!(
        shadow.n_skipped, 0,
        "somebody else's mode does not count as skips"
    );
    assert_eq!(shadow.n_unevaluated, 2);
    assert!(shadow
        .rows
        .iter()
        .all(|r| r.status == garnet_db::MatchupStatus::NotEvaluated));
}

#[tokio::test]
async fn a_copy_that_never_became_a_position_is_not_a_slippage_skip() {
    // We decided to copy and there is no position — the exchange refused, or the fill
    // never arrived. An exchange refusal hidden inside the statistics of our own
    // thresholds would make us turn a threshold where the threshold is not the issue.
    let db = db("mu_nofill").await;
    wallet(&db, "0xm9").await;
    his_trade(
        &db,
        "0xm9",
        "tok_k",
        Side::Buy,
        dec!(0.40),
        dec!(100),
        0,
        "k1",
    )
    .await;

    let t: i64 = sqlx::query_scalar("SELECT id FROM leader_trades WHERE token_id = 'tok_k'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    db.signals()
        .record(
            t,
            "0xm9",
            Mode::Live,
            "copy",
            dec!(10),
            Some(dec!(0.46)),
            None,
            None,
        )
        .await
        .unwrap();

    let rep = db.matchup().report(Mode::Live, None).await.unwrap();
    assert_eq!(rep.n_skipped, 1);
    let row = &rep.rows[0];
    assert!(
        matches!(&row.status, garnet_db::MatchupStatus::Skipped { reason }
                 if reason.contains("there is no position")),
        "the reason must name the copy that never happened, not slippage: {:?}",
        row.status
    );
}
