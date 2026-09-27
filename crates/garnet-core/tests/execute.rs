use garnet_core::execute::{
    order_size, submit_ioc, ClobExec, ExecError, OrderOutcome, OrderRequest,
};
use garnet_core::shadow::{Fill, FillSource};
use garnet_db::Side;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

struct StubClob {
    calls: AtomicUsize,
    behaviour: Behaviour,
    seen: Mutex<Vec<OrderRequest>>,
}

enum Behaviour {
    Fills(Decimal),
    AlwaysRejects(&'static str),
    RejectsThenFills(Decimal),
    /// The order was accepted, but the fill is unconfirmed.
    Unknown(&'static str),
}

impl StubClob {
    fn new(b: Behaviour) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            behaviour: b,
            seen: Mutex::new(Vec::new()),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn unknown(msg: &'static str) -> Self {
        Self::new(Behaviour::Unknown(msg))
    }
    fn last(&self) -> OrderRequest {
        self.seen.lock().unwrap().last().unwrap().clone()
    }
}

fn fill(size: Decimal, price: Decimal) -> Fill {
    Fill {
        size,
        avg_price: price,
        notional: size * price,
        fee_usd: Decimal::ZERO,
        source: FillSource::BookWalk, // the executor must override this to Clob
    }
}

impl ClobExec for StubClob {
    async fn place_ioc(&self, req: &OrderRequest) -> Result<Fill, ExecError> {
        self.seen.lock().unwrap().push(req.clone());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        match self.behaviour {
            Behaviour::Fills(size) => Ok(fill(size, req.limit_price)),
            Behaviour::AlwaysRejects(msg) => Err(ExecError::Rejected(msg.into())),
            Behaviour::Unknown(msg) => Err(ExecError::Unknown(msg.into())),
            Behaviour::RejectsThenFills(size) => {
                if n == 0 {
                    Err(ExecError::Rejected("price moved".into()))
                } else {
                    Ok(fill(size, req.limit_price))
                }
            }
        }
    }
}

fn req(size: Decimal, neg_risk: bool) -> OrderRequest {
    OrderRequest {
        token_id: "tok_lal".into(),
        side: Side::Buy,
        limit_price: dec!(0.345),
        size_shares: size,
        neg_risk,
    }
}

#[tokio::test]
async fn partial_fill_is_kept_and_not_chased() {
    let clob = StubClob::new(Behaviour::Fills(dec!(40)));
    let out = submit_ioc(&clob, &req(dec!(80), false)).await;

    match out {
        OrderOutcome::Partial(f) => {
            assert_eq!(f.size, dec!(40));
            assert_eq!(
                f.source,
                FillSource::Clob,
                "the fill came from the exchange"
            );
        }
        other => panic!("expected Partial, got {other:?}"),
    }
    assert_eq!(clob.calls(), 1, "we do not chase the remainder");
}

#[tokio::test]
async fn full_fill_is_filled() {
    let clob = StubClob::new(Behaviour::Fills(dec!(80)));
    assert!(matches!(
        submit_ioc(&clob, &req(dec!(80), false)).await,
        OrderOutcome::Filled(_)
    ));
}

#[tokio::test]
async fn rejection_retries_exactly_once_then_gives_up() {
    let clob = StubClob::new(Behaviour::AlwaysRejects("price moved"));
    let out = submit_ioc(&clob, &req(dec!(80), false)).await;

    match out {
        OrderOutcome::Rejected(msg) => {
            assert!(
                msg.contains("price moved"),
                "the reason for refusal must be preserved: {msg}"
            );
            assert!(
                msg.contains("retry"),
                "the record shows there was a second attempt"
            );
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert_eq!(clob.calls(), 2, "exactly one retry");
}

#[tokio::test]
async fn second_attempt_can_succeed() {
    let clob = StubClob::new(Behaviour::RejectsThenFills(dec!(80)));
    assert!(matches!(
        submit_ioc(&clob, &req(dec!(80), false)).await,
        OrderOutcome::Filled(_)
    ));
    assert_eq!(clob.calls(), 2);
}

#[tokio::test]
async fn empty_fill_is_a_rejection_not_a_zero_position() {
    let clob = StubClob::new(Behaviour::Fills(Decimal::ZERO));
    assert!(matches!(
        submit_ioc(&clob, &req(dec!(80), false)).await,
        OrderOutcome::Rejected(_)
    ));
}

#[tokio::test]
async fn neg_risk_flag_reaches_the_adapter() {
    let clob = StubClob::new(Behaviour::Fills(dec!(80)));
    submit_ioc(&clob, &req(dec!(80), true)).await;
    assert!(
        clob.last().neg_risk,
        "the adapter is chosen by the market flag"
    );
}

// ---------------------------------------------------------------------------
// The shape of an order: lot, minimum notional and the grid of cents
// ---------------------------------------------------------------------------
//
// Three live refusals from the exchange on 2026-09-04, each one after fixing the last:
//   1. «Size 29.411764705882352941176470588 has 27 decimal places. Maximum lot
//      size is 2»;
//   2. «invalid amount for a marketable BUY order ($0.99975), min size: 1»;
//   3. «the market buy orders maker amount supports a max accuracy of 2
//      decimals, taker amount a max of 5 decimals».
//
// The third is the main one: the notional in dollars must land on the grid of cents.
// That, not the length of the size, is what sets the permitted sizes: the step
// depends on the price.

#[test]
fn the_dollar_amount_always_lands_on_the_cent_grid() {
    for (stake, price) in [
        (dec!(25), dec!(0.48)),
        (dec!(1), dec!(0.021)),
        (dec!(3), dec!(0.07)),
        (dec!(10), dec!(0.999)),
        (dec!(5), dec!(0.5)),
    ] {
        let size = order_size(stake, price, None).unwrap();
        let amount = size * price;
        assert_eq!(
            amount,
            amount.round_dp(2),
            "the notional {amount} is longer than two decimals"
        );
        assert_eq!(
            size,
            size.round_dp(5),
            "the size {size} is longer than five decimals"
        );
    }
}

#[test]
fn a_stake_is_spent_as_fully_as_the_grid_allows() {
    // 25 / 0.48 = 52.0833..., and the grid of cents is held by a step of 0.0625.
    assert_eq!(
        order_size(dec!(25), dec!(0.48), None).unwrap(),
        dec!(52.0625)
    );
}

#[test]
fn an_order_below_the_exchange_minimum_is_raised_to_the_next_valid_size() {
    // 1 / 0.021 = 47.6 shares, but at this price the grid of cents is held only by a
    // step of 10 shares: 40 shares give $0.84 — below the exchange's $1 minimum, so we
    // take 50.
    let size = order_size(dec!(1), dec!(0.021), None).unwrap();
    assert_eq!(size, dec!(50));
    assert!(size * dec!(0.021) >= dec!(1));
}

#[test]
fn the_markets_declared_share_minimum_does_not_bind() {
    // `minimum_order_size` in the CLOB response equals 5 for every market, but the
    // exchange measures the minimum **in money**: the refusal on 2026-09-04 read
    // "invalid amount for a marketable BUY order ($0.99975), min size: 1". Taking 5
    // shares as binding, we would be refusing the expensive sides: $1 at 0.82 is one
    // and a half shares, and such an order is legitimate.
    let size = order_size(dec!(1), dec!(0.82), Some(dec!(5))).unwrap();
    assert!(
        size < dec!(5),
        "the market's share-count minimum is not binding: {size}"
    );
    assert!(size * dec!(0.82) >= dec!(1), "whereas the money one is");
}

#[test]
fn a_stake_below_the_exchange_minimum_still_buys_the_minimum() {
    // A stake under a dollar does not yield an order cheaper than a dollar: the
    // exchange will not accept one. We take the minimum legitimate order, no more.
    let size = order_size(dec!(0.5), dec!(0.82), None).unwrap();
    assert_eq!(size * dec!(0.82), dec!(1.23));
}

// ---------------------------------------------------------------------------
// An unknown outcome versus a refusal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_outcome_is_never_retried() {
    // Measured 2026-09-04, smoke test 2: the exchange accepted the order, but
    // `get_order` still showed zero filled. We took that for a refusal, retried — and
    // bought twice over: two trades of 7.142856 shares in the same second, $2.00
    // instead of $1.00. A retry is permitted only where the exchange **did not**
    // accept the order.
    let clob = StubClob::unknown("the filled size is unknown");
    let out = submit_ioc(&clob, &req(dec!(50), false)).await;

    assert!(matches!(out, OrderOutcome::Unknown(_)), "got: {out:?}");
    assert_eq!(clob.calls(), 1, "an unknown outcome is not retried");
}
