use garnet_copy_engine::exit::{
    apply_exit, apply_exit_shadow, carried_fraction, carried_shares, exit_fraction, shares_to_sell,
    ExitOutcome,
};
use garnet_core::execute::{ClobExec, ExecError, OrderRequest};
use garnet_core::shadow::{Fill, FillSource};
use garnet_db::{Db, Mode, Position};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

struct StubClob {
    calls: AtomicUsize,
    rejects: bool,
    seen: Mutex<Vec<OrderRequest>>,
}

impl StubClob {
    fn ok() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            rejects: false,
            seen: Mutex::new(Vec::new()),
        }
    }
    fn always_rejects() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            rejects: true,
            seen: Mutex::new(Vec::new()),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn last(&self) -> OrderRequest {
        self.seen.lock().unwrap().last().unwrap().clone()
    }
}

impl ClobExec for StubClob {
    async fn place_ioc(&self, req: &OrderRequest) -> Result<Fill, ExecError> {
        self.seen.lock().unwrap().push(req.clone());
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.rejects {
            Err(ExecError::Rejected("no liquidity".into()))
        } else {
            Ok(Fill {
                size: req.size_shares,
                avg_price: req.limit_price,
                notional: req.size_shares * req.limit_price,
                fee_usd: Decimal::ZERO,
                source: FillSource::BookWalk,
            })
        }
    }
}

/// A Postgres schema of its own per test: settlement and reconciliation passes are
/// global in meaning, and in a shared schema a neighbouring test would change our
/// rows.
async fn db(wallet: &str) -> Db {
    let db = garnet_db::testing::isolated_db("exit").await.unwrap();
    db.wallets().add(wallet, None).await.unwrap();
    db
}

fn position(open: Decimal) -> Position {
    Position {
        id: 1,
        wallet: "0xw".into(),
        token_id: "tok_lal".into(),
        mode: Mode::Live,
        size_bought: open,
        size_sold: Decimal::ZERO,
        cost_usd: open * dec!(0.40),
        proceeds_usd: Decimal::ZERO,
        fees_usd: Decimal::ZERO,
        leader_observed_size: dec!(100),
        attempts: 0,
        last_attempt_at: None,
        opened_at: chrono::Utc::now(),
        closed_at: None,
        pending_exit_shares: Decimal::ZERO,
        pending_exit_price: Decimal::ZERO,
    }
}

#[test]
fn fraction_is_clamped_to_what_we_observed() {
    assert_eq!(exit_fraction(dec!(40), dec!(100)), dec!(0.4));
    // the leader held the lot before us: sold 300 against 100 observed
    assert_eq!(
        exit_fraction(dec!(300), dec!(100)),
        dec!(1.0),
        "a full exit, not 3x"
    );
    assert_eq!(
        exit_fraction(dec!(10), Decimal::ZERO),
        dec!(1.0),
        "we saw no buys — we exit in full"
    );
    assert_eq!(
        exit_fraction(dec!(-5), dec!(100)),
        Decimal::ZERO,
        "a negative fraction does not exist"
    );
}

#[test]
fn shares_follow_the_open_size_not_the_bought_size() {
    let mut p = position(dec!(100));
    p.size_sold = dec!(60);
    assert_eq!(
        shares_to_sell(&p, dec!(0.5)),
        dec!(20),
        "half of the remaining 40"
    );
}

#[tokio::test]
async fn sell_uses_the_mirrored_slippage_floor() {
    let clob = StubClob::ok();
    let out = apply_exit(
        &clob,
        &position(dec!(100)),
        dec!(0.4),
        dec!(0.40),
        dec!(0.15),
        false,
        None,
    )
    .await;

    match out {
        ExitOutcome::Sold(f) => assert_eq!(f.size, dec!(40)),
        other => panic!("expected a sale, got {other:?}"),
    }
    assert_eq!(
        clob.last().limit_price,
        dec!(0.34),
        "the floor = the leader's price minus slippage"
    );
}

#[tokio::test]
async fn failed_exit_holds_to_resolution_instead_of_dumping() {
    let clob = StubClob::always_rejects();
    let out = apply_exit(
        &clob,
        &position(dec!(100)),
        dec!(0.4),
        dec!(0.40),
        dec!(0.15),
        false,
        None,
    )
    .await;

    assert!(matches!(out, ExitOutcome::HeldToResolution { .. }));
    assert_eq!(clob.calls(), 2, "one retry, then we hold");
}

#[tokio::test]
async fn nothing_to_sell_is_not_an_order() {
    let clob = StubClob::ok();
    let mut p = position(dec!(100));
    p.size_sold = dec!(100);
    let out = apply_exit(&clob, &p, dec!(1), dec!(0.40), dec!(0.15), false, None).await;
    assert_eq!(out, ExitOutcome::Nothing);
    assert_eq!(clob.calls(), 0, "we do not submit an empty order");
}

#[tokio::test]
async fn switching_mode_does_not_merge_paper_and_real_lots() {
    let db = db("0xmodesplit").await;
    let p = db.positions();
    p.apply_buy(
        "0xmodesplit",
        "tok",
        Mode::Shadow,
        dec!(50),
        dec!(15),
        Decimal::ZERO,
    )
    .await
    .unwrap();
    p.apply_buy(
        "0xmodesplit",
        "tok",
        Mode::Live,
        dec!(50),
        dec!(15.5),
        Decimal::ZERO,
    )
    .await
    .unwrap();

    let rows = p.for_wallet("0xmodesplit").await.unwrap();
    assert_eq!(
        rows.len(),
        2,
        "the key (wallet, token_id, mode) keeps them apart"
    );
    assert_eq!(rows.iter().filter(|r| r.mode == Mode::Live).count(), 1);
}

#[tokio::test]
async fn buys_accumulate_because_stake_is_per_signal() {
    let db = db("0xaverage").await;
    let p = db.positions();
    p.apply_buy(
        "0xaverage",
        "tok",
        Mode::Live,
        dec!(50),
        dec!(15),
        dec!(0.1),
    )
    .await
    .unwrap();
    let after = p
        .apply_buy(
            "0xaverage",
            "tok",
            Mode::Live,
            dec!(40),
            dec!(14),
            dec!(0.1),
        )
        .await
        .unwrap();

    assert_eq!(
        after.size_bought,
        dec!(90),
        "the leader's averaging is reproduced"
    );
    assert_eq!(after.cost_usd, dec!(29));
    assert_eq!(after.fees_usd, dec!(0.2));
}

#[tokio::test]
async fn selling_everything_closes_the_position() {
    let db = db("0xclose").await;
    let p = db.positions();
    p.apply_buy(
        "0xclose",
        "tok",
        Mode::Live,
        dec!(50),
        dec!(20),
        Decimal::ZERO,
    )
    .await
    .unwrap();
    let after = p
        .apply_sell("0xclose", "tok", Mode::Live, dec!(50), dec!(25), dec!(0.3))
        .await
        .unwrap();

    assert_eq!(after.open_size(), Decimal::ZERO);
    assert!(after.closed_at.is_some(), "a zero position must close");
    assert_eq!(after.proceeds_usd, dec!(25));
}

#[tokio::test]
async fn settlement_queue_walks_oldest_first() {
    let db = db("0xqueue").await;
    let p = db.positions();
    for (tok, age) in [
        ("tok_new", "1 hour"),
        ("tok_old", "9 days"),
        ("tok_mid", "3 days"),
    ] {
        p.apply_buy("0xqueue", tok, Mode::Live, dec!(10), dec!(4), Decimal::ZERO)
            .await
            .unwrap();
        sqlx::query("UPDATE positions SET opened_at = now() - $2::interval WHERE token_id = $1")
            .bind(tok)
            .bind(age)
            .execute(db.pool())
            .await
            .unwrap();
    }

    // The queue is shared across all wallets — in the test we look only at ours.
    let queue = p.open_for_settlement(100).await.unwrap();
    let order: Vec<&str> = queue
        .iter()
        .filter(|q| q.wallet == "0xqueue")
        .map(|q| q.token_id.as_str())
        .collect();
    assert_eq!(
        order,
        vec!["tok_old", "tok_mid", "tok_new"],
        "newest-first jammed the queue in the predecessor"
    );
}

#[tokio::test]
async fn the_sell_floor_is_snapped_up_to_the_tick() {
    // 0.40 * 0.85 = 0.34 exactly, while 0.417 * 0.85 = 0.35445 — off the 0.01 grid.
    // Upwards: a floor rounded down would let the sale through below the permitted
    // slippage, and an off-grid price is rejected locally by the SDK's builder.
    let clob = StubClob::ok();
    apply_exit(
        &clob,
        &position(dec!(100)),
        dec!(1),
        dec!(0.417),
        dec!(0.15),
        false,
        Some(dec!(0.01)),
    )
    .await;
    assert_eq!(clob.last().limit_price, dec!(0.36));
}

#[tokio::test]
async fn a_slice_below_the_exchange_minimum_is_not_sent() {
    // Invariant 15: the minimum order is $1 in money. The leader trims a position
    // by a percentage, and our share comes out in cents: on 05.09.2026 shadow booked
    // such exits for $0.44, $0.06 and $0.02 — a live wallet cannot make them.
    let clob = StubClob::ok();
    let out = apply_exit(
        &clob,
        &position(dec!(100)),
        dec!(0.005), // 0.5 shares at 0.40 — forty cents
        dec!(0.40),
        dec!(0.15),
        false,
        Some(dec!(0.01)),
    )
    .await;

    assert_eq!(
        clob.calls(),
        0,
        "the exchange would reject an order below the minimum — we do not send it"
    );
    let ExitOutcome::HeldToResolution { reason } = out else {
        panic!("expected a refusal with a reason, got {out:?}");
    };
    assert!(
        reason.contains("minimum"),
        "the reason must name the minimum: {reason}"
    );
}

#[tokio::test]
async fn a_slice_above_the_exchange_minimum_is_sent() {
    let clob = StubClob::ok();
    let out = apply_exit(
        &clob,
        &position(dec!(100)),
        dec!(0.5), // 50 shares at 0.34 — seventeen dollars
        dec!(0.40),
        dec!(0.15),
        false,
        Some(dec!(0.01)),
    )
    .await;

    assert_eq!(clob.calls(), 1);
    assert!(matches!(out, ExitOutcome::Sold(_)));
}

/// Deferred fractions add up until they reach the exchange minimum.
///
/// The leader trims a position by percentages, our share comes out in cents, and
/// every such sale was refused by the $1 minimum. That means we copy the leader's
/// entry and do not copy their exit — we trade a different strategy from the one we
/// measure.
#[test]
fn deferred_fractions_add_up() {
    let pos = position(dec!(100));
    // The first fraction, 2% = 2 shares at 0.30 = $0.60, below the minimum.
    let first = carried_shares(&pos, dec!(0.02), Decimal::ZERO);
    assert_eq!(first, dec!(2));
    // A second one the same: 4 shares have accumulated = $1.20, which passes.
    let second = carried_shares(&pos, dec!(0.02), first);
    assert_eq!(second, dec!(4));
}

/// Accumulated in shares, not in fractions: a fraction is computed against the
/// current size, and that changes with additional buys.
#[test]
fn the_carry_is_shares_not_a_fraction() {
    let small = position(dec!(100));
    let grown = position(dec!(400));
    // The same 5 deferred shares stay five, however much the position grows.
    assert_eq!(carried_shares(&small, Decimal::ZERO, dec!(5)), dec!(5));
    assert_eq!(carried_shares(&grown, Decimal::ZERO, dec!(5)), dec!(5));
}

/// The carry cannot exceed what we hold: otherwise we would sell more than we
/// bought.
#[test]
fn the_carry_never_exceeds_the_position() {
    let pos = position(dec!(10));
    assert_eq!(carried_shares(&pos, dec!(1), dec!(999)), dec!(10));
}

/// Converting back into a fraction: the exit path works in fractions, and the carry
/// has to return to the same units without losing anything to rounding.
#[test]
fn shares_convert_back_to_a_fraction_of_what_is_held() {
    let pos = position(dec!(40));
    assert_eq!(carried_fraction(&pos, dec!(10)), dec!(0.25));
    assert_eq!(carried_fraction(&pos, dec!(40)), Decimal::ONE);
    // An empty position: there is nothing to divide by, and the fraction is zero
    // rather than a panic.
    let mut empty = position(dec!(5));
    empty.size_sold = dec!(5);
    assert_eq!(carried_fraction(&empty, dec!(3)), Decimal::ZERO);
}

// --- A refused paper exit: two different reasons -------------------------
//
// Until 06.09.2026 both were reported by the single line "the book offers no price
// above the floor". Over seven hours of running it accumulated 31 records, from
// which neither a slippage threshold nor a conclusion about liquidity can be drawn:
// an empty book is about the market, a bid below the floor is about the price.

fn book_with_bids(bids: serde_json::Value) -> garnet_core::book::Book {
    garnet_core::book::Book::from_clob(&serde_json::json!({
        "asks": [{ "price": "0.50", "size": "100" }],
        "bids": bids,
        "tick_size": "0.01",
        "min_order_size": "5",
    }))
    .unwrap()
}

fn schedule() -> garnet_core::market_meta::FeeSchedule {
    garnet_core::market_meta::FeeSchedule::free()
}

#[test]
fn shadow_exit_says_the_book_has_no_bids() {
    let pos = position(dec!(100));
    let book = book_with_bids(serde_json::json!([]));

    let out = apply_exit_shadow(
        &book,
        &pos,
        Decimal::ONE,
        dec!(0.40),
        dec!(0.15),
        Some(dec!(0.01)),
        &schedule(),
    );

    match out {
        ExitOutcome::HeldToResolution { reason } => {
            assert_eq!(reason, "there are no bids in the book")
        }
        other => panic!("expected a refusal on an empty book, got {other:?}"),
    }
}

#[test]
fn shadow_exit_names_the_bid_and_the_floor() {
    let pos = position(dec!(100));
    // The floor = 0.40 - 15% = 0.34, the best bid is 0.20: not enough.
    let book = book_with_bids(serde_json::json!([{ "price": "0.20", "size": "500" }]));

    let out = apply_exit_shadow(
        &book,
        &pos,
        Decimal::ONE,
        dec!(0.40),
        dec!(0.15),
        Some(dec!(0.01)),
        &schedule(),
    );

    match out {
        ExitOutcome::HeldToResolution { reason } => {
            assert!(
                reason.contains("0.20"),
                "the reason does not name the bid: {reason}"
            );
            assert!(
                reason.contains("0.34"),
                "the reason does not name the floor: {reason}"
            );
        }
        other => panic!("expected a refusal on price, got {other:?}"),
    }
}
