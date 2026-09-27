//! The market: the order book and the outcome position.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// The position of an outcome within a CTF condition.
///
/// **This is an ordinal, not a label.** A condition has two outcomes, and
/// redemption addresses them by number: `index_set` 1 for the first, 2 for the
/// second. The labels, meanwhile, can be anything at all — Up/Down, Over/Under,
/// team names — and in the predecessor this same type was called
/// `TokenSide::Yes/No`, which openly invited reading it as "yes/no" and
/// swapping the sides on payout.
///
/// The order comes from `tokens[]` in the CLOB response: the `clobTokenIds` and
/// `outcomes` arrays agree positionally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutcomeIndex {
    /// The condition's first outcome, `index_set = 1`.
    First,
    /// The condition's second outcome, `index_set = 2`.
    Second,
}

impl OutcomeIndex {
    /// Position within `tokens[]`.
    pub fn position(self) -> usize {
        match self {
            OutcomeIndex::First => 0,
            OutcomeIndex::Second => 1,
        }
    }

    /// The `index_set` for CTF: the outcome's bit mask.
    pub fn index_set(self) -> u64 {
        match self {
            OutcomeIndex::First => 1,
            OutcomeIndex::Second => 2,
        }
    }

    pub fn from_position(pos: usize) -> Option<Self> {
        match pos {
            0 => Some(OutcomeIndex::First),
            1 => Some(OutcomeIndex::Second),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceLevel {
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
}

/// A snapshot of the order book, in the shape the CLOB returns it.
///
/// **The order of the levels in the response is not guaranteed**: `/book`
/// returns asks by descending price, best last. Anyone reading `asks` directly
/// must sort them themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookSnapshot {
    pub asset_id: String,
    pub market: String,
    /// The exchange's clock. Unfit for checking freshness: the exchange's clock
    /// and ours drift by tens of seconds when NTP is out of sync, and in the
    /// predecessor that produced a false `STALE_ORDERBOOK` on a perfectly fresh
    /// book.
    pub timestamp: DateTime<Utc>,
    /// Our clock at the moment of receipt — the only frame of reference for
    /// freshness.
    #[serde(default = "Utc::now")]
    pub received_at: DateTime<Utc>,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    #[serde(default)]
    pub hash: String,
}
