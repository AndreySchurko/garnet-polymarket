//! The decision to copy.
//!
//! A wallet added by the operator is copied unconditionally. There are, and can
//! be, no checks here on price, category, the leader's size, time to the event or
//! the phase of a match: the predecessor rejected 35 signals out of 35 precisely
//! because its list of refusal reasons grew on its own.

pub mod exit;
pub mod sizing;

use garnet_core::market_meta::MarketMeta;
use garnet_db::{Mode, Wallet};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Reasons to skip a signal. Every one after the fifth appeared by a **decision
/// of the operator**, not by a refactor: the predecessor rejected 35 signals
/// out of 35 precisely because its list of refusal reasons grew on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    WalletDisabled,
    SlippageExceeded,
    InsufficientBalance,
    MarketNotTradable,
    Duplicate,
    /// The sixth reason, and it appeared by an operator's decision on 06.09.2026,
    /// not by a refactor: concentration within one event is limited by money.
    ExposureCapped,
    /// The seventh, by the same route (19.09.2026): the queue of entries is
    /// limited **by time**, not by market. The slice window trims slices of one
    /// order, the ceiling counts money within one event — a leader who fired
    /// twelve decisions within a minute across twelve markets passes both and
    /// brings 94% of the loss (invariant 44).
    RateLimited,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::WalletDisabled => "skip:wallet_disabled",
            SkipReason::SlippageExceeded => "skip:slippage_exceeded",
            SkipReason::InsufficientBalance => "skip:insufficient_balance",
            SkipReason::MarketNotTradable => "skip:market_not_tradable",
            SkipReason::Duplicate => "skip:duplicate",
            SkipReason::ExposureCapped => "skip:exposure_capped",
            SkipReason::RateLimited => "skip:rate_limited",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Copy {
        size_usd: Decimal,
        limit_price: Decimal,
    },
    Skip(SkipReason),
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Copy { .. } => "copy",
            Verdict::Skip(r) => r.as_str(),
        }
    }
}

/// The leader's price and what is visible in the book right now.
#[derive(Debug, Clone, Copy)]
pub struct Quote {
    /// The price the leader entered at.
    pub leader_price: Decimal,
    /// The best price available to us at the moment of the decision. `None` —
    /// there are no asks at all, there is nobody to buy from.
    pub best_ask: Option<Decimal>,
}

/// The ceiling on the buy price: we do not take anything worse.
///
/// The only price guard in the system. This is execution protection, not a
/// selection filter: our order arrives seconds later, and without a ceiling a
/// thin in-play book would be bought out at any price.
pub fn buy_limit(leader_price: Decimal, max_slippage_pct: Decimal) -> Decimal {
    (leader_price * (Decimal::ONE + max_slippage_pct)).min(dec!(0.999))
}

/// The mirror-image floor on the sell price.
pub fn sell_limit(leader_price: Decimal, max_slippage_pct: Decimal) -> Decimal {
    (leader_price * (Decimal::ONE - max_slippage_pct)).max(dec!(0.001))
}

/// The decision about one buy by the leader.
///
/// `balance` — the mode's free funds: the on-chain balance for live and the
/// virtual ledger for shadow. Shadow never refuses on balance: it is a measuring
/// instrument, and halting it would bias the sample exactly where it is most
/// interesting.
/// `already_in_wave` — we have already followed this wallet into this market with
/// a buy, and the slice window has not elapsed since. The leader's taker order
/// consumes as much of the book as is standing there and arrives as that many
/// frames with different `tx_hash` values; dedup by hash does not catch them and
/// cannot. Measured 06.09.2026: 43% of our buys were such slices, 75% for one
/// wallet. Orders after the first in a wave return -15% against +7.4% for the
/// first (a difference of +23.6 pp, interval [3.1; 44.3], 6 wallets out of 8).
/// `market_open` — how much of this mode's money is already standing in the same
/// event, `market_cap` — the ceiling; zero disables it. The ceiling is set
/// against concentration, not for returns: the counterfactual of 06.09.2026
/// showed that as a way to raise ROI it works only insofar as it accidentally
/// trims slices of one order — and that is what the slice window does (invariant
/// 29), deliberately.
#[allow(clippy::too_many_arguments)]
pub fn decide_with(
    w: &Wallet,
    m: &MarketMeta,
    q: Quote,
    balance: Decimal,
    already_in_wave: bool,
    market_open: Decimal,
    market_cap: Decimal,
) -> Verdict {
    if !w.enabled {
        return Verdict::Skip(SkipReason::WalletDisabled);
    }
    if m.closed || m.resolved_outcome.is_some() {
        return Verdict::Skip(SkipReason::MarketNotTradable);
    }
    // Before price: a slice of an already-copied order is not "expensive", and it
    // has no place in the slippage statistics.
    if already_in_wave {
        return Verdict::Skip(SkipReason::Duplicate);
    }

    // An empty book is not "expensive". Until 06.09.2026 `app.rs` substituted 1.0
    // here, and a missing price landed in the slippage statistics as a refusal on
    // price: the "expensive" bucket filled up with observations where there was no
    // price at all, and a threshold cannot be tuned from such a log at any sample
    // size. The refusal reason stays one of the existing ones — a market with
    // nothing to buy is not tradable for us.
    let Some(best_ask) = q.best_ask else {
        return Verdict::Skip(SkipReason::MarketNotTradable);
    };

    let limit_price = buy_limit(q.leader_price, w.max_slippage_pct);
    if best_ask > limit_price {
        return Verdict::Skip(SkipReason::SlippageExceeded);
    }
    if w.mode == Mode::Live && balance < w.stake_usd {
        return Verdict::Skip(SkipReason::InsufficientBalance);
    }
    // The ceiling is judged last: an order that would not have been taken on price
    // or on funds anyway must not report itself as having hit the limit. A
    // remainder smaller than the stake is a refusal, not a reduced stake: the part
    // that fits is half of the leader's decision, not a decision.
    if market_cap > Decimal::ZERO && market_cap - market_open < w.stake_usd {
        return Verdict::Skip(SkipReason::ExposureCapped);
    }

    Verdict::Copy {
        size_usd: w.stake_usd,
        limit_price,
    }
}

/// The decision without the exposure ceiling. Kept for callers that do not need
/// it, and for the tests: substituting two zeros into every one of them is noise
/// that hides which parameter a test is actually checking.
pub fn decide(
    w: &Wallet,
    m: &MarketMeta,
    q: Quote,
    balance: Decimal,
    already_in_wave: bool,
) -> Verdict {
    decide_with(
        w,
        m,
        q,
        balance,
        already_in_wave,
        Decimal::ZERO,
        Decimal::ZERO,
    )
}
