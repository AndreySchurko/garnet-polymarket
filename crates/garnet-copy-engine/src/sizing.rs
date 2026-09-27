//! The size of a copy.
//!
//! The wallet's stake goes in **on every signal**: the leader adding to a
//! position is another stake of the same size, not a top-up towards a position
//! ceiling. That way their averaging is reproduced rather than clipped.

use rust_decimal::Decimal;

/// How many shares `size_usd` buys at `price`.
pub fn shares_for(size_usd: Decimal, price: Decimal) -> Decimal {
    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    size_usd / price
}
