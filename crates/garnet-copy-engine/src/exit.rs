//! Exits.
//!
//! The leader sold a fraction of their position — we sell the same fraction of
//! ours. The fraction is computed against the leader's position **as observed by
//! us**: before the wallet was added their buys were invisible to us, so a sale of
//! 40% of theirs may amount to 300% of what we saw. Anything above what we
//! observed is a full exit.

use crate::sell_limit;
use garnet_core::book::Book;
use garnet_core::execute::{
    submit_ioc, ClobExec, OrderOutcome, OrderRequest, MIN_ORDER_NOTIONAL_USD,
};
use garnet_core::market_meta::FeeSchedule;
use garnet_core::shadow::{simulate_exit, Fill};
use garnet_db::{Position, Side};
use rust_decimal::Decimal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitOutcome {
    Sold(Fill),
    /// The sale did not go through — the position stays and lives to resolution.
    /// An exit does not turn into a market order at any price: that is no longer
    /// copying.
    HeldToResolution {
        reason: String,
    },
    /// The exchange accepted the order, but the outcome is unknown. We neither
    /// touch the position nor retry: reconciliation resolves the divergence, while
    /// a retry would sell twice.
    ///
    /// **The order's size and price are mandatory here.** The reconciler explains
    /// a divergence by an order in flight (invariant 48), and an order about whose
    /// size nothing was recorded explains nothing: until 19.09.2026 these three
    /// fields were written as zeros, and a sale in flight was indistinguishable
    /// from a loss of tokens. A row that exists for the sake of future diagnostics
    /// is obliged to carry the number those diagnostics are made from.
    Unknown {
        reason: String,
        size_shares: Decimal,
        limit_price: Decimal,
    },
    /// There is nothing to sell.
    Nothing,
}

/// What fraction of our own position we are closing.
///
/// Clamped to 0..=1. If the leader's observed position is zero (we saw only their
/// sale), we exit in full: holding a tail whose origin nothing is known about is
/// worse than closing.
pub fn exit_fraction(leader_sold: Decimal, leader_observed: Decimal) -> Decimal {
    if leader_observed <= Decimal::ZERO {
        return Decimal::ONE;
    }
    (leader_sold / leader_observed)
        .min(Decimal::ONE)
        .max(Decimal::ZERO)
}

/// How many shares we are selling.
pub fn shares_to_sell(pos: &Position, fraction: Decimal) -> Decimal {
    (pos.open_size() * fraction).max(Decimal::ZERO)
}

/// How many shares we want to sell now, including what was deferred earlier.
///
/// The leader trims a position by percentages, and our share comes out in cents:
/// at a $25 stake, sixty-three percent of exit attempts hit the $1 minimum order
/// size (invariant 15). A refusal on every such sale means we copy the leader's
/// entry and do not copy their exit — that is, we trade a different strategy from
/// the one we measure.
///
/// Accumulated **in shares**: a fraction is computed against the current position
/// size, that size changes with additional buys, and a deferred "five percent"
/// would mean a different number of shares an hour later. More than we hold can
/// never accumulate — otherwise we would sell more than we bought.
#[must_use]
pub fn carried_shares(pos: &Position, fraction: Decimal, carried: Decimal) -> Decimal {
    (shares_to_sell(pos, fraction) + carried.max(Decimal::ZERO))
        .min(pos.open_size())
        .max(Decimal::ZERO)
}

/// Accumulated shares back into a fraction of the position: the exit path works in
/// fractions.
///
/// An empty position yields zero rather than a division by zero: there is nothing
/// to sell, and that is an ordinary state, not an error.
#[must_use]
pub fn carried_fraction(pos: &Position, shares: Decimal) -> Decimal {
    let held = pos.open_size();
    if held <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    (shares / held).min(Decimal::ONE).max(Decimal::ZERO)
}

/// The shape of an exit: how many shares, and below what price we do not part with
/// them.
///
/// One shape for both modes, deliberately. Were it to diverge, a paper exit would
/// be measuring something other than what a live one would do, and the comparison
/// between the modes would lose its meaning.
/// What to do about an exit: an order, "nothing", or "the exchange will not accept
/// that much".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitShape {
    Order(Box<OrderRequest>),
    /// There is nothing to sell: the leader's fraction fell on an empty position.
    Nothing,
    /// The fraction, in money, is below the exchange's minimum order size. It
    /// differs from "nothing" in that we do have a position — and that has to be
    /// visible as a row, not as silence.
    BelowMinimum {
        notional: Decimal,
    },
}

pub fn exit_request(
    pos: &Position,
    fraction: Decimal,
    leader_price: Decimal,
    max_slippage_pct: Decimal,
    neg_risk: bool,
    // The market's tick size. The SDK's builder rejects an off-grid limit locally.
    tick: Option<Decimal>,
) -> ExitShape {
    let size = garnet_core::market_meta::snap_size_down(shares_to_sell(pos, fraction));
    if size <= Decimal::ZERO {
        return ExitShape::Nothing;
    }

    // Upwards: for a sale the limit is a floor, and rounding down would let it
    // through below the slippage the operator permitted.
    let limit_price =
        garnet_core::market_meta::snap_price_up(sell_limit(leader_price, max_slippage_pct), tick);

    // Invariant 15: the minimum order is $1 in money. The leader trims a position
    // by a percentage, and our share comes out in cents; raising it the way a buy
    // is raised is not allowed — that would be selling more than the leader sold.
    let notional = size * limit_price;
    if notional < MIN_ORDER_NOTIONAL_USD {
        return ExitShape::BelowMinimum { notional };
    }

    ExitShape::Order(Box::new(OrderRequest {
        token_id: pos.token_id.clone(),
        side: Side::Sell,
        limit_price,
        size_shares: size,
        neg_risk,
    }))
}

fn below_minimum(notional: Decimal) -> ExitOutcome {
    ExitOutcome::HeldToResolution {
        reason: format!(
            "the leader's fraction is ${}, minimum order ${MIN_ORDER_NOTIONAL_USD}",
            notional.round_dp(2)
        ),
    }
}

/// Executes a live exit. On a refusal, `submit_ioc` does the retry — exactly one.
pub async fn apply_exit<C: ClobExec>(
    clob: &C,
    pos: &Position,
    fraction: Decimal,
    leader_price: Decimal,
    max_slippage_pct: Decimal,
    neg_risk: bool,
    tick: Option<Decimal>,
) -> ExitOutcome {
    let req = match exit_request(
        pos,
        fraction,
        leader_price,
        max_slippage_pct,
        neg_risk,
        tick,
    ) {
        ExitShape::Order(req) => req,
        ExitShape::Nothing => return ExitOutcome::Nothing,
        ExitShape::BelowMinimum { notional } => return below_minimum(notional),
    };

    match submit_ioc(clob, &req).await {
        OrderOutcome::Filled(f) | OrderOutcome::Partial(f) => ExitOutcome::Sold(f),
        OrderOutcome::Rejected(reason) => ExitOutcome::HeldToResolution { reason },
        OrderOutcome::Unknown(reason) => ExitOutcome::Unknown {
            reason,
            size_shares: req.size_shares,
            limit_price: req.limit_price,
        },
    }
}

/// A paper exit: the same order, but filled against the bids of the book.
///
/// It does not touch the exchange at all. On 05.09.2026 twelve paper wallets did
/// touch it: the exit path did not branch on mode and sent 81 signed sell orders
/// from the trading key.
pub fn apply_exit_shadow(
    book: &Book,
    pos: &Position,
    fraction: Decimal,
    leader_price: Decimal,
    max_slippage_pct: Decimal,
    tick: Option<Decimal>,
    fee: &FeeSchedule,
) -> ExitOutcome {
    let req = match exit_request(pos, fraction, leader_price, max_slippage_pct, false, tick) {
        ExitShape::Order(req) => req,
        ExitShape::Nothing => return ExitOutcome::Nothing,
        ExitShape::BelowMinimum { notional } => return below_minimum(notional),
    };

    let fill = simulate_exit(book, req.size_shares, req.limit_price, fee);
    if fill.is_empty() {
        // The position lives to resolution: an exit does not turn into a market
        // order at any price. There are two reasons and they are different — an
        // empty book says something about the market, a bid below the floor says
        // something about the price. With one line for both (until 06.09.2026) they
        // piled into a heap from which neither a slippage threshold nor a
        // conclusion about liquidity can be drawn: exactly the same mistake as the
        // one on the buy side.
        return ExitOutcome::HeldToResolution {
            reason: match book.best_bid() {
                None => "there are no bids in the book".to_string(),
                Some(bid) => format!("the best bid {bid} is below the floor {}", req.limit_price),
            },
        };
    }
    ExitOutcome::Sold(fill)
}
