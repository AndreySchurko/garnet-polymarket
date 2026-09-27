//! Emergency closing of positions.
//!
//! `/kill` stops trading and **leaves the positions**. We had no "close everything
//! now" path at all, and at the moment one is needed it would have been done by hand
//! through the exchange — under pressure, without a trace and without checks.
//!
//! Invariant 46: an irreversible action requires **a phrase, not a button**. A button
//! is pressed by accident, a phrase is not. The three modes differ not in force but
//! in what each one gives up, and each is guarded by its own condition.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

/// How long an intent lives. A minute is "I decided and I am doing it", not "I
/// decided yesterday": a confirmation found in a chat log an hour later confirms a
/// decision that no longer exists.
pub const INTENT_TTL_SECS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlattenMode {
    /// Sell nothing, let the positions live to resolution. Trading stops all the
    /// same: this is a wind-down, not an exit.
    Graceful,
    /// Sell only what is in profit. It **gives up the expected payout** on winning
    /// positions — which is why it demands a separate confirmation.
    Hybrid,
    /// Sell everything. Requires that trading already be stopped.
    Panic,
}

impl FlattenMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FlattenMode::Graceful => "graceful",
            FlattenMode::Hybrid => "hybrid",
            FlattenMode::Panic => "panic",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "graceful" => Some(FlattenMode::Graceful),
            "hybrid" => Some(FlattenMode::Hybrid),
            "panic" => Some(FlattenMode::Panic),
            _ => None,
        }
    }
}

/// The operator's intent. It lives in `controls` because it must survive a restart
/// (invariant 20): between "decided" and "confirmed" the process may restart, and
/// losing the decision at that moment means demanding that it be taken again under
/// the same pressure.
#[derive(Debug, Clone)]
pub struct Intent {
    pub mode: FlattenMode,
    pub declared_at: DateTime<Utc>,
    /// The operator said out loud that they give up the expected payout. Only for
    /// `hybrid`, and as **a separate field** rather than inferred from the mode:
    /// inferring consent from the choice of mode means not asking for it at all.
    pub acknowledge_forfeit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    WrongPhrase,
    Expired { age_secs: i64 },
    FromTheFuture,
    TradingStillRunning,
    ForfeitNotAcknowledged,
}

impl Refusal {
    #[must_use]
    pub fn why(&self) -> String {
        match self {
            Refusal::WrongPhrase => "wrong phrase".into(),
            Refusal::Expired { age_secs } => {
                format!("the intent has expired: it is {age_secs} s old against a limit of {INTENT_TTL_SECS}")
            }
            Refusal::FromTheFuture => {
                "the intent is timestamped in the future: the clock cannot be trusted".into()
            }
            Refusal::TradingStillRunning => {
                "trading is not stopped: closing everything while continuing to buy \
                 is not a halt, it is a swap"
                    .into()
            }
            Refusal::ForfeitNotAcknowledged => {
                "hybrid sells winning positions before resolution; giving up the \
                 expected payout is confirmed separately"
                    .into()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Go(FlattenMode),
    Refuse(Refusal),
}

/// The phrase the operator has to type.
///
/// The installation's name in it is not decoration: a phrase copied from
/// documentation must not fire on someone else's host.
#[must_use]
pub fn required_phrase(mode: FlattenMode, name: &str) -> String {
    format!("{} {name}", mode.as_str())
}

/// Whether to let an irreversible action through.
///
/// The order of the checks is not arbitrary. A timestamp from the future is checked
/// **before** expiry: such a timestamp has a negative age, and the expiry check would
/// pass it as fresh — meaning an intent marked with tomorrow's date would never
/// expire.
#[must_use]
pub fn check(
    intent: &Intent,
    phrase: &str,
    name: &str,
    now: DateTime<Utc>,
    trading_stopped: bool,
) -> Gate {
    if intent.declared_at > now {
        return Gate::Refuse(Refusal::FromTheFuture);
    }
    let age = (now - intent.declared_at).num_seconds();
    if age > INTENT_TTL_SECS {
        return Gate::Refuse(Refusal::Expired { age_secs: age });
    }

    // Only the outer whitespace is trimmed: the client adds it by itself, and that is
    // not intent. Inside, the phrase is compared character by character, case
    // included — normalisation widens a target that is not meant to be hit by
    // accident.
    if phrase.trim() != required_phrase(intent.mode, name) {
        return Gate::Refuse(Refusal::WrongPhrase);
    }

    match intent.mode {
        FlattenMode::Panic if !trading_stopped => Gate::Refuse(Refusal::TradingStillRunning),
        FlattenMode::Hybrid if !intent.acknowledge_forfeit => {
            Gate::Refuse(Refusal::ForfeitNotAcknowledged)
        }
        mode => Gate::Go(mode),
    }
}

/// Whether a position is in profit at the current best bid.
///
/// Computed against the **full** entry price — stake and fees (invariant 4): a
/// position whose proceeds do not cover the fee paid is not in profit.
///
/// `None` — there is nothing to measure with: no bid, or an empty position. `hybrid`
/// must **leave such a position alone**: selling something whose profitability is
/// unknown means going beyond what the operator agreed to (invariant 27).
#[must_use]
pub fn in_profit(
    cost_usd: Decimal,
    fees_usd: Decimal,
    size_bought: Decimal,
    best_bid: Option<Decimal>,
) -> Option<bool> {
    let bid = best_bid?;
    if size_bought <= Decimal::ZERO {
        return None;
    }
    let entry = (cost_usd + fees_usd).checked_div(size_bought)?;
    Some(bid > entry)
}
