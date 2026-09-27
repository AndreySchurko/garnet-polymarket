//! Market metadata and the fee.
//!
//! The data arrives from two different APIs, and that is not an implementation
//! detail:
//!   * **CLOB** `/markets/<condition_id>` — the only source of `tokens[]`, and
//!     therefore of outcome labels and the `winner` flag. The labels are not
//!     Yes/No: crypto pairs use Up/Down, totals use Over/Under, sports use team
//!     names.
//!   * **Gamma** `/markets?condition_ids=` — the only source of `feeSchedule`. The
//!     CLOB does not have it at all, and its `taker_base_fee` equals 1000 for every
//!     market and is not a rate.
//!
//! The fee is paid by the taker only and has the form `rate * (p*(1-p))^exponent`.
//! A rate applied linearly to the price is the wrong form: at p=0.5 it overstates
//! by a factor of four.

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use rust_decimal::prelude::*;
use rust_decimal::{Decimal, MathematicalOps, RoundingStrategy};

/// A market's fee. Specific to each market: the category is not a proxy for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeSchedule {
    pub rate: Decimal,
    pub exponent: Decimal,
    pub taker_only: bool,
}

impl FeeSchedule {
    /// A market with no fee — about 5% of them.
    pub fn free() -> Self {
        Self {
            rate: Decimal::ZERO,
            exponent: Decimal::ONE,
            taker_only: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketMeta {
    pub token_id: String,
    pub condition_id: String,
    pub question: String,
    /// The real label of the side: "Up", "Over 2.5", "Los Angeles Lakers".
    pub outcome_label: String,
    pub category: Option<String>,
    /// Sports timing comes from here. `end_date` is unfit for it: by that measure
    /// in-play is indistinguishable from pre-game.
    pub game_start_time: Option<DateTime<Utc>>,
    pub end_date: Option<DateTime<Utc>>,
    pub neg_risk: bool,
    pub closed: bool,
    pub fee: FeeSchedule,
    /// The label of the winning side. Filled only from `tokens[].winner`; a sale at
    /// 0.99 is not a resolution.
    pub resolved_outcome: Option<String>,
    /// The position of our outcome within the CTF condition. Redemption needs it: it
    /// addresses an outcome by number (`index_set` 1 or 2), not by label.
    pub outcome_index: Option<garnet_types::market::OutcomeIndex>,
    /// The `token_id` of the winning side. A win is determined by **comparing
    /// token_id values**, not labels: labels can be Up/Down, Over/Under or team
    /// names, and comparing by them books every win as a total loss.
    pub winner_token_id: Option<String>,
    /// The market's tick size. A limit that is off the grid is rejected locally by
    /// the SDK's builder — the predecessor merely logged such refusals.
    pub tick: Option<Decimal>,
    /// The minimum order size in shares. A $1 stake on the expensive side yields
    /// less than the minimum, and there is no point sending such an order.
    pub min_order_size: Option<Decimal>,
}

impl MarketMeta {
    /// Whether **our** side won.
    pub fn we_won(&self) -> Option<bool> {
        self.winner_token_id.as_ref().map(|w| *w == self.token_id)
    }
}

/// The taker fee on a fill: `rate * (p*(1-p))^exponent` per share.
///
/// Applied identically to real fills and to hypothetical ones in shadow — a free
/// shadow would be systematically better than live by exactly this amount.
pub fn taker_fee(fee: &FeeSchedule, price: Decimal, size: Decimal) -> Decimal {
    if fee.rate.is_zero() {
        return Decimal::ZERO;
    }
    let base = price * (Decimal::ONE - price);
    let shaped = if fee.exponent == Decimal::ONE {
        base
    } else {
        base.powd(fee.exponent)
    };
    fee.rate * shaped * size
}

/// Parses the CLOB response. The label and the resolution come only from here.
pub fn parse_clob_market(
    raw: &serde_json::Value,
    wanted_token: &str,
) -> anyhow::Result<MarketMeta> {
    let tokens = raw["tokens"].as_array().context(
        "the CLOB response has no tokens[] — without it there is nowhere to get the outcome label",
    )?;

    let token = tokens
        .iter()
        .find(|t| t["token_id"].as_str() == Some(wanted_token))
        .ok_or_else(|| anyhow!("token {wanted_token} is absent from this market's tokens[]"))?;

    let outcome_index = tokens
        .iter()
        .position(|t| t["token_id"].as_str() == Some(wanted_token))
        .and_then(garnet_types::market::OutcomeIndex::from_position);

    let outcome_label = token["outcome"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("an empty outcome label for token {wanted_token}"))?
        .to_string();

    // A resolution exists only if the platform raised winner.
    let winner = tokens.iter().find(|t| t["winner"].as_bool() == Some(true));
    let resolved_outcome = winner
        .and_then(|t| t["outcome"].as_str())
        .map(str::to_string);
    let winner_token_id = winner
        .and_then(|t| t["token_id"].as_str())
        .map(str::to_string);

    Ok(MarketMeta {
        token_id: wanted_token.to_string(),
        condition_id: raw["condition_id"]
            .as_str()
            .context("no condition_id")?
            .to_string(),
        question: raw["question"].as_str().unwrap_or_default().to_string(),
        outcome_label,
        category: raw["tags"]
            .as_array()
            .and_then(|t| t.first())
            .and_then(|v| v.as_str())
            .map(str::to_string),
        game_start_time: parse_ts(&raw["game_start_time"])?,
        end_date: parse_ts(&raw["end_date_iso"])?,
        neg_risk: raw["neg_risk"].as_bool().unwrap_or(false),
        closed: raw["closed"].as_bool().unwrap_or(false),
        fee: FeeSchedule::free(), // until merged with Gamma the fee is unknown
        outcome_index,
        resolved_outcome,
        winner_token_id,
        tick: num_field(&raw["minimum_tick_size"]),
        min_order_size: num_field(&raw["minimum_order_size"]),
    })
}

/// A number from JSON, where it arrives sometimes as a number and sometimes as a string.
fn num_field(v: &serde_json::Value) -> Option<Decimal> {
    match v {
        serde_json::Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        serde_json::Value::String(s) => Decimal::from_str(s).ok(),
        _ => None,
    }
}

/// Snap a price **down** to the market's grid.
///
/// Down, not to the nearest: up would mean paying more than the operator permitted
/// with their `max_slippage_pct`. The result is never zero — the exchange will not
/// accept an order at zero, so one tick remains.
#[must_use]
pub fn snap_price_down(price: Decimal, tick: Option<Decimal>) -> Decimal {
    let Some(tick) = tick.filter(|t| *t > Decimal::ZERO) else {
        return price;
    };
    let steps = (price / tick).floor();
    let snapped = steps * tick;
    if snapped <= Decimal::ZERO {
        tick
    } else {
        snapped
    }
}

/// The condition id from a Gamma row.
///
/// Needed when the book is already gone: `GET /book?token_id=` on a resolved market
/// answers 404, and the condition was exactly what we used to fetch from there.
/// Gamma, queried by the token itself (`?clob_token_ids=<t>&closed=true`), returns
/// both the condition and the fee in one row — without the flag it does not show a
/// closed market at all.
pub fn parse_condition_id(raw: &serde_json::Value) -> anyhow::Result<String> {
    let m = gamma_row(raw)?;
    m["conditionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .context("the Gamma row has no conditionId")
}

/// The first row of a Gamma response. An empty array means "ask differently" (a
/// closed market needs `closed=true`), not "there is no data".
fn gamma_row(raw: &serde_json::Value) -> anyhow::Result<&serde_json::Value> {
    match raw {
        serde_json::Value::Array(a) => a.first().context("Gamma returned an empty array"),
        other => Ok(other),
    }
}

/// Parses the Gamma `/markets?condition_ids=` response, which arrives as an array.
pub fn parse_fee_schedule(raw: &serde_json::Value) -> anyhow::Result<FeeSchedule> {
    let m = gamma_row(raw)?;

    if m["feesEnabled"].as_bool() != Some(true) || m["feeSchedule"].is_null() {
        return Ok(FeeSchedule::free());
    }

    let fs = &m["feeSchedule"];
    Ok(FeeSchedule {
        rate: num(&fs["rate"]).context("a feeSchedule without a rate")?,
        exponent: num(&fs["exponent"]).unwrap_or(Decimal::ONE),
        taker_only: fs["takerOnly"].as_bool().unwrap_or(true),
    })
}

fn num(v: &serde_json::Value) -> Option<Decimal> {
    match v {
        serde_json::Value::String(s) => Decimal::from_str(s).ok(),
        serde_json::Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        _ => None,
    }
}

fn parse_ts(v: &serde_json::Value) -> anyhow::Result<Option<DateTime<Utc>>> {
    match v.as_str() {
        None | Some("") => Ok(None),
        Some(s) => Ok(Some(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc))),
    }
}

/// Snap a price **up** to the market's grid.
///
/// The mirror image of [`snap_price_down`] for a sale: there the limit is a floor,
/// and rounding down would drop us below the slippage the operator permitted.
#[must_use]
pub fn snap_price_up(price: Decimal, tick: Option<Decimal>) -> Decimal {
    let Some(tick) = tick.filter(|t| *t > Decimal::ZERO) else {
        return price;
    };
    (price / tick).ceil() * tick
}

/// The maximum number of decimal places in an order size.
///
/// The exchange rejects anything longer: "Size ... has 27 decimal places. Maximum
/// lot size is 2". The size comes from dividing the stake by the price and is almost
/// always non-terminating in decimal.
const SIZE_DECIMALS: u32 = 2;

/// Truncate a size to the permitted precision, **downwards**.
///
/// Down, not to the nearest: rounding up would spend more than the stake the
/// operator set.
#[must_use]
pub fn snap_size_down(size: Decimal) -> Decimal {
    size.round_dp_with_strategy(SIZE_DECIMALS, RoundingStrategy::ToZero)
}
