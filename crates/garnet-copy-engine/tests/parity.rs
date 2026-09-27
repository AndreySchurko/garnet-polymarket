//! Parity between the modes.
//!
//! Live and shadow must differ by exactly one thing: how the fill is obtained.
//! Everything else — size, cost, fee, position — is computed by one body of code.
//! Otherwise a comparison between the modes measures not the quality of execution
//! but the difference between two implementations of accounting.

use garnet_copy_engine::exit::{apply_exit, apply_exit_shadow, ExitOutcome};
use garnet_core::book::Book;
use garnet_core::execute::{submit_ioc, ClobExec, ExecError, OrderOutcome, OrderRequest};
use garnet_core::market_meta::FeeSchedule;
use garnet_core::shadow::{simulate_exit, simulate_fill, Fill, FillSource};
use garnet_db::{Db, Mode, Position, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn book() -> Book {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/book_thin.json");
    let raw = std::fs::read_to_string(path).unwrap();
    Book::from_clob(&serde_json::from_str(&raw).unwrap()).unwrap()
}

fn fee() -> FeeSchedule {
    FeeSchedule {
        rate: dec!(0.05),
        exponent: Decimal::ONE,
        taker_only: true,
    }
}

/// An exchange that returns exactly what a walk down the book would give.
struct HonestClob;

impl ClobExec for HonestClob {
    async fn place_ioc(&self, req: &OrderRequest) -> Result<Fill, ExecError> {
        Ok(simulate_fill(
            &book(),
            req.size_shares * req.limit_price,
            req.limit_price,
            &fee(),
        ))
    }
}

/// A Postgres schema of its own per test: settlement and reconciliation passes are
/// global in meaning, and in a shared schema a neighbouring test would change our
/// rows.
async fn db(wallet: &str) -> Db {
    let db = garnet_db::testing::isolated_db("parity").await.unwrap();
    db.wallets().add(wallet, None).await.unwrap();
    db
}

/// Runs one and the same signal in the given mode and returns the position.
async fn run_signal(db: &Db, wallet: &str, mode: Mode) -> (Position, Fill) {
    let size_usd = dec!(25);
    let limit = dec!(0.45);

    let fill = match mode {
        Mode::Shadow => simulate_fill(&book(), size_usd, limit, &fee()),
        Mode::Live => {
            let req = OrderRequest {
                token_id: "tok_lal".into(),
                side: Side::Buy,
                limit_price: limit,
                size_shares: size_usd / limit,
                neg_risk: false,
            };
            match submit_ioc(&HonestClob, &req).await {
                OrderOutcome::Filled(f) | OrderOutcome::Partial(f) => f,
                OrderOutcome::Rejected(e) | OrderOutcome::Unknown(e) => {
                    panic!("a refusal inside the parity test: {e}")
                }
            }
        }
    };

    let pos = db
        .positions()
        .apply_buy(
            wallet,
            "tok_lal",
            mode,
            fill.size,
            fill.notional,
            fill.fee_usd,
        )
        .await
        .unwrap();
    (pos, fill)
}

#[tokio::test]
async fn same_signal_produces_identical_accounting_in_both_modes() {
    let db = db("0xparity").await;

    let (live_pos, live_fill) = run_signal(&db, "0xparity", Mode::Live).await;
    let (shadow_pos, shadow_fill) = run_signal(&db, "0xparity", Mode::Shadow).await;

    assert_eq!(
        live_pos.size_bought, shadow_pos.size_bought,
        "the lots must match"
    );
    assert_eq!(
        live_pos.cost_usd, shadow_pos.cost_usd,
        "the cost must match"
    );
    assert_eq!(live_pos.fees_usd, shadow_pos.fees_usd, "the fee must match");
    assert!(
        live_pos.fees_usd > Decimal::ZERO,
        "the fee is charged in both modes"
    );

    // The only difference is how the fill was obtained.
    assert_eq!(live_fill.source, FillSource::Clob);
    assert_eq!(shadow_fill.source, FillSource::BookWalk);
    assert_eq!(live_fill.size, shadow_fill.size);
    assert_eq!(live_fill.avg_price, shadow_fill.avg_price);
}

#[tokio::test]
async fn the_two_modes_are_separate_rows_not_one() {
    let db = db("0xparity2").await;
    run_signal(&db, "0xparity2", Mode::Live).await;
    run_signal(&db, "0xparity2", Mode::Shadow).await;

    let rows = db.positions().for_wallet("0xparity2").await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].token_id, rows[1].token_id,
        "one and the same market"
    );
    assert_ne!(rows[0].mode, rows[1].mode, "different modes");
}

/// An exchange that returns, on a sale, exactly what a walk down the bids would give.
struct HonestSellClob;

impl ClobExec for HonestSellClob {
    async fn place_ioc(&self, req: &OrderRequest) -> Result<Fill, ExecError> {
        let mut f = simulate_exit(&book(), req.size_shares, req.limit_price, &fee());
        f.source = FillSource::Clob;
        Ok(f)
    }
}

fn held_position(open: Decimal) -> Position {
    Position {
        id: 1,
        wallet: "0xparity3".into(),
        token_id: "tok_lal".into(),
        mode: Mode::Live,
        size_bought: open,
        size_sold: Decimal::ZERO,
        cost_usd: open * dec!(0.40),
        proceeds_usd: Decimal::ZERO,
        fees_usd: Decimal::ZERO,
        leader_observed_size: dec!(120),
        attempts: 0,
        last_attempt_at: None,
        opened_at: chrono::Utc::now(),
        closed_at: None,
        pending_exit_shares: Decimal::ZERO,
        pending_exit_price: Decimal::ZERO,
    }
}

#[tokio::test]
async fn an_exit_is_identical_in_both_modes_too() {
    // Parity on entry was checked from the start, parity on exit was not, and the
    // defect settled exactly there: until 05.09.2026 a paper exit went to the live
    // exchange.
    let pos = held_position(dec!(66.5));

    let live = apply_exit(
        &HonestSellClob,
        &pos,
        dec!(1),
        dec!(0.42),
        dec!(0.40),
        false,
        Some(dec!(0.01)),
    )
    .await;
    let shadow = apply_exit_shadow(
        &book(),
        &pos,
        dec!(1),
        dec!(0.42),
        dec!(0.40),
        Some(dec!(0.01)),
        &fee(),
    );

    let (ExitOutcome::Sold(lf), ExitOutcome::Sold(sf)) = (&live, &shadow) else {
        panic!("both modes must sell: live={live:?} shadow={shadow:?}");
    };
    assert_eq!(lf.size, sf.size, "the lots must match");
    assert_eq!(lf.avg_price, sf.avg_price, "the price must match");
    assert_eq!(lf.notional, sf.notional);
    assert_eq!(lf.fee_usd, sf.fee_usd, "the fee is charged in both modes");
    assert!(sf.fee_usd > Decimal::ZERO);

    // The only difference is how the fill was obtained.
    assert_eq!(lf.source, FillSource::Clob);
    assert_eq!(sf.source, FillSource::BookWalk);
}

#[tokio::test]
async fn a_slice_below_the_minimum_is_refused_in_both_modes() {
    // Otherwise shadow measures exits that live cannot make, and the difference
    // between the modes stops being a difference of execution.
    let pos = held_position(dec!(66.5));

    let live = apply_exit(
        &HonestSellClob,
        &pos,
        dec!(0.005),
        dec!(0.42),
        dec!(0.40),
        false,
        Some(dec!(0.01)),
    )
    .await;
    let shadow = apply_exit_shadow(
        &book(),
        &pos,
        dec!(0.005),
        dec!(0.42),
        dec!(0.40),
        Some(dec!(0.01)),
        &fee(),
    );

    assert!(
        matches!(live, ExitOutcome::HeldToResolution { .. }),
        "live: {live:?}"
    );
    assert!(
        matches!(shadow, ExitOutcome::HeldToResolution { .. }),
        "shadow: {shadow:?}"
    );
}
