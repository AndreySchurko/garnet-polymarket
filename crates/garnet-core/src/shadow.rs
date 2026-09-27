//! Shadow execution: a hypothetical fill against the real order book.
//!
//! The path is the same as live's and diverges at exactly one point — instead of
//! submitting an order we walk the ladder of asks. The fee is charged by the same
//! formula: a free shadow would be systematically better than live by exactly its
//! amount, and the comparison between the modes would lose its meaning.

use crate::book::Book;
use crate::market_meta::{taker_fee, FeeSchedule};
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillSource {
    Clob,
    BookWalk,
}

impl FillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FillSource::Clob => "clob",
            FillSource::BookWalk => "book_walk",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    pub size: Decimal,
    pub avg_price: Decimal,
    pub notional: Decimal,
    pub fee_usd: Decimal,
    pub source: FillSource,
}

impl Fill {
    pub fn is_empty(&self) -> bool {
        self.size <= Decimal::ZERO
    }
}

/// Walks the book for `size_usd`, never going above `limit`.
///
/// The fee is computed from the average fill price — the same way the exchange
/// computes it for a real taker.
pub fn simulate_fill(book: &Book, size_usd: Decimal, limit: Decimal, fee: &FeeSchedule) -> Fill {
    let mut spent = Decimal::ZERO;
    let mut qty = Decimal::ZERO;

    for lvl in book.asks.iter().filter(|l| l.price <= limit) {
        let remaining = size_usd - spent;
        if remaining <= Decimal::ZERO {
            break;
        }
        let take = (remaining / lvl.price).min(lvl.size);
        if take <= Decimal::ZERO {
            break;
        }
        spent += take * lvl.price;
        qty += take;
    }

    let avg_price = if qty.is_zero() {
        Decimal::ZERO
    } else {
        spent / qty
    };

    Fill {
        size: qty,
        avg_price,
        notional: spent,
        fee_usd: taker_fee(fee, avg_price, qty),
        source: FillSource::BookWalk,
    }
}

/// Walks the book to sell `size_shares`, never descending below `floor`.
///
/// The mirror image of `simulate_fill`: a buy goes up the asks from the best, a
/// sale goes down the bids. The bottom of the walk is the price floor, not any
/// price: an exit does not turn into a market order, that is no longer copying. The
/// fee is the same taker fee (invariant 5) — otherwise a paper exit is
/// systematically better than a live one.
pub fn simulate_exit(book: &Book, size_shares: Decimal, floor: Decimal, fee: &FeeSchedule) -> Fill {
    let mut got = Decimal::ZERO;
    let mut qty = Decimal::ZERO;

    for lvl in book.bids.iter().filter(|l| l.price >= floor) {
        let remaining = size_shares - qty;
        if remaining <= Decimal::ZERO {
            break;
        }
        let take = remaining.min(lvl.size);
        if take <= Decimal::ZERO {
            break;
        }
        got += take * lvl.price;
        qty += take;
    }

    let avg_price = if qty.is_zero() {
        Decimal::ZERO
    } else {
        got / qty
    };

    Fill {
        size: qty,
        avg_price,
        notional: got,
        fee_usd: taker_fee(fee, avg_price, qty),
        source: FillSource::BookWalk,
    }
}
