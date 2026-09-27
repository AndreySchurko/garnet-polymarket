//! The CLOB order book.
//!
//! A trap that would have cost money silently: `/book` returns **asks by
//! descending price**, meaning the best price is the last element of the array
//! while the first is 0.999. Walking in array order would buy at the worst price
//! in the book. That is why the levels here are always re-sorted, and the order
//! from the response is never used.

use anyhow::Context;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone)]
pub struct Book {
    /// By ascending price: the best ask first.
    pub asks: Vec<Level>,
    /// By descending price: the best bid first.
    pub bids: Vec<Level>,
    pub tick_size: Decimal,
    pub min_order_size: Decimal,
}

impl Book {
    pub fn from_clob(raw: &serde_json::Value) -> anyhow::Result<Self> {
        let mut asks = levels(&raw["asks"])?;
        let mut bids = levels(&raw["bids"])?;
        asks.sort_by_key(|a| a.price);
        bids.sort_by_key(|b| std::cmp::Reverse(b.price));

        Ok(Self {
            asks,
            bids,
            tick_size: dec_field(&raw["tick_size"]).unwrap_or(Decimal::new(1, 3)),
            min_order_size: dec_field(&raw["min_order_size"]).unwrap_or(Decimal::ZERO),
        })
    }

    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.first().map(|l| l.price)
    }

    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.first().map(|l| l.price)
    }

    /// The mid of the market — the price an open position is valued at.
    ///
    /// `None` when either side is empty: such a position counts as unpriced and is
    /// visible as a number in the equity snapshot. An invented mid would distort
    /// equity silently, and drawdown is measured from equity.
    pub fn mid(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => Some((b + a) / Decimal::TWO),
            _ => None,
        }
    }
}

fn levels(v: &serde_json::Value) -> anyhow::Result<Vec<Level>> {
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    arr.iter()
        .map(|l| {
            Ok(Level {
                price: dec_field(&l["price"]).context("a level without a price")?,
                size: dec_field(&l["size"]).context("a level without a size")?,
            })
        })
        .collect()
}

/// Prices and sizes arrive as strings, not as numbers.
fn dec_field(v: &serde_json::Value) -> Option<Decimal> {
    match v {
        serde_json::Value::String(s) => Decimal::from_str(s).ok(),
        serde_json::Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        _ => None,
    }
}
