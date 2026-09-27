//! Live execution.
//!
//! The order is always **IOC**: we take what is in the book right now, the
//! remainder is cancelled. A GTC limit order would turn us into a maker and wait in
//! the book — the opposite of copying, and precisely what the predecessor's only
//! trait could do (`place_limit_order`, with no order type).
//!
//! A partial fill is accepted and not chased: chasing is a strategy of its own, not
//! copying.

use crate::shadow::{Fill, FillSource};
use garnet_db::Side;
use rust_decimal::Decimal;

/// An order for execution, assembled by the copy engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRequest {
    pub token_id: String,
    pub side: Side,
    /// A ceiling for a buy, a floor for a sale.
    pub limit_price: Decimal,
    pub size_shares: Decimal,
    /// Neg-risk markets go through their own adapter; the collateral is always USDC.e.
    pub neg_risk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderOutcome {
    Filled(Fill),
    Partial(Fill),
    Rejected(String),
    /// The order was accepted by the exchange, but its fate is unknown.
    ///
    /// Neither a refusal nor a fill: the position may have been taken on without
    /// being visible to us. Such an outcome is **not retried** and not booked — it is
    /// reconciled.
    Unknown(String),
}

/// Why the executor returned no fill.
///
/// The distinction was paid for by a double buy on 2026-09-04: the exchange accepted
/// the order, `get_order` still showed zero filled, we took that for a refusal and
/// retried — buying twice over, $2.00 instead of $1.00.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// The exchange **did not accept** the order: a retry against a fresh book is safe.
    Rejected(String),
    /// The order was accepted, the outcome is unconfirmed. A retry is forbidden.
    Unknown(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(m) | Self::Unknown(m) => f.write_str(m),
        }
    }
}

/// The minimal contract of an executor. The real implementation is an adapter over
/// the carried-over `garnet-clob`; in tests, a stub.
pub trait ClobExec {
    fn place_ioc(
        &self,
        req: &OrderRequest,
    ) -> impl std::future::Future<Output = Result<Fill, ExecError>> + Send;
}

/// The minimum notional of a buy order, in dollars.
///
/// Published in no API response; known from a live refusal on 2026-09-04: "invalid
/// amount for a marketable BUY order ($0.99975), min size: 1".
pub const MIN_ORDER_NOTIONAL_USD: Decimal = Decimal::ONE;

/// The size step at which an order's notional lands on the grid of cents.
///
/// The exchange computes the notional as `price x size` and requires no more than two
/// decimal places from it: "the market buy orders maker amount supports a max
/// accuracy of 2 decimals". At a price of 0.021 that means orders are only possible
/// in multiples of ten shares, and at 0.48 in multiples of 0.0625.
fn cent_grid_step(price: Decimal) -> Decimal {
    let price = price.normalize();
    let scale = price.scale();
    let mantissa = price.mantissa().unsigned_abs();
    if mantissa == 0 {
        return Decimal::ONE;
    }
    // The size lives to a precision of 10^-5 and the notional to 10^-2, so the
    // condition "notional on the grid of cents" becomes `mantissa x size_in_units`
    // being a multiple of 10^(scale + 5 - 2).
    let modulus = 10_u128.pow(scale + SIZE_DECIMALS_MAX - AMOUNT_DECIMALS_MAX);
    let step_units = modulus / gcd(mantissa, modulus);
    Decimal::from_i128_with_scale(
        i128::try_from(step_units).unwrap_or(i128::MAX),
        SIZE_DECIMALS_MAX,
    )
    .normalize()
}

fn gcd(a: u128, b: u128) -> u128 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// The maximum number of decimals in a size: "taker amount a max of 5 decimals".
const SIZE_DECIMALS_MAX: u32 = 5;

/// The maximum number of decimals in a notional: "maker amount supports a max
/// accuracy of 2 decimals".
const AMOUNT_DECIMALS_MAX: u32 = 2;

/// How many shares to order for a given stake.
///
/// Three exchange constraints at once, and all three are known only from its
/// refusals:
///   * the notional `price x size` — no longer than two decimal places;
///   * the notional no lower than [`MIN_ORDER_NOTIONAL_USD`];
///   * the size no lower than the market's minimum, where it declares one.
///
/// Rounding goes down so as not to spend more than the stake. When the grid step is
/// large — at three-digit prices it reaches ten shares — an order rounded down falls
/// through the exchange's minimum, and it is raised by one step. If the market's
/// minimum costs more than the stake, the order does not exist: that is a refusal
/// with a reason, not a silent doubling of the spend.
///
/// # Errors
///
/// The stake does not cover the minimum order for this market.
pub fn order_size(
    stake_usd: Decimal,
    limit_price: Decimal,
    min_shares: Option<Decimal>,
) -> Result<Decimal, String> {
    if limit_price <= Decimal::ZERO {
        return Err("the limit price is not positive".to_string());
    }
    let step = cent_grid_step(limit_price);
    let floor_to_step = |v: Decimal| (v / step).floor() * step;
    let ceil_to_step = |v: Decimal| (v / step).ceil() * step;

    let mut size = floor_to_step(stake_usd / limit_price);

    if size * limit_price < MIN_ORDER_NOTIONAL_USD {
        size = ceil_to_step(MIN_ORDER_NOTIONAL_USD / limit_price);
    }
    // `min_shares` from the CLOB response (`minimum_order_size`) equals 5 for every
    // market and is not a minimum: the exchange measures in money. Taking it as
    // binding, we would be refusing the expensive sides — $1 at 0.82 is one and a
    // half shares, and such an order is legitimate.
    let _ = min_shares;

    // One grid step above the stake is the price of rounding, not a decision to trade
    // larger: on the cheap sides a step costs cents. The exchange's minimum order
    // also passes here when the stake is under a dollar: no order cheaper than a
    // dollar exists, and that is no reason not to trade.
    let ceiling = stake_usd.max(MIN_ORDER_NOTIONAL_USD) + step * limit_price;
    if size * limit_price > ceiling {
        return Err(format!(
            "the minimum order on this market is {size} shares at {limit_price}, which is ${}, more than the stake of ${stake_usd}",
            (size * limit_price).round_dp(2)
        ));
    }
    Ok(size.normalize())
}

/// Submits an IOC order and, on a refusal, retries **exactly once**.
///
/// The second attempt exists for the race against a book update; a third and beyond
/// would be chasing a price that has left, which is not copying.
pub async fn submit_ioc<C: ClobExec>(clob: &C, req: &OrderRequest) -> OrderOutcome {
    let first = match clob.place_ioc(req).await {
        Ok(fill) => return classify(fill, req),
        Err(e) => e,
    };

    // We retry only what the exchange did not accept. An unknown outcome means the
    // order may have filled: a retry here buys twice over.
    let ExecError::Rejected(first) = first else {
        return OrderOutcome::Unknown(first.to_string());
    };

    match clob.place_ioc(req).await {
        Ok(fill) => classify(fill, req),
        Err(ExecError::Rejected(second)) => {
            OrderOutcome::Rejected(format!("{first}; retry: {second}"))
        }
        Err(ExecError::Unknown(second)) => {
            OrderOutcome::Unknown(format!("{first}; retry: {second}"))
        }
    }
}

fn classify(mut fill: Fill, req: &OrderRequest) -> OrderOutcome {
    fill.source = FillSource::Clob;
    if fill.size <= Decimal::ZERO {
        OrderOutcome::Rejected("the book gave nothing inside the limit".into())
    } else if fill.size < req.size_shares {
        OrderOutcome::Partial(fill)
    } else {
        OrderOutcome::Filled(fill)
    }
}
