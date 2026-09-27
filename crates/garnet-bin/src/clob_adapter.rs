//! The bridge between the carried-over CLOB client and Garnet's execution contract.
//!
//! Three things here cannot be left to the caller:
//!   * the order type is always `FAK` — take whatever is on offer immediately, cancel the
//!     remainder. `GTC` would leave us a maker, while we copy a taker;
//!   * the submission response **does not contain the filled size** — it exists only in
//!     `get_order().size_matched`, so the fill is read separately;
//!   * the fee is computed by the caller from the market's schedule: the exchange does not
//!     return it in the response, and a zero fee would make live better than shadow for no
//!     reason at all.

use garnet_clob::traits::ClobClient;
use garnet_clob::types::{ClobOrderStatus, OrderKind, Side as ClobSide};
use garnet_core::execute::{ClobExec, ExecError, OrderRequest};
use garnet_core::shadow::{Fill, FillSource};
use garnet_db::Side;
use rust_decimal::Decimal;

/// How many times we ask the exchange about a fill before declaring the outcome unknown.
///
/// The exchange does not reflect a match instantly, and an instant conclusion of "nothing
/// filled" leads to a repeat order on top of one already filled.
/// Measured 2026-09-04: a trade appears in the feed seconds after the fill, and in that
/// interval `get_order` shows zero filled. The polling window has to cover that delay,
/// otherwise a filled order looks unknown.
const FILL_POLL_ATTEMPTS: u32 = 12;
const FILL_POLL_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

pub struct ClobAdapter<C: ClobClient> {
    client: C,
    attempts: u32,
    delay: std::time::Duration,
}

impl<C: ClobClient> ClobAdapter<C> {
    pub fn new(client: C) -> Self {
        Self {
            client,
            attempts: FILL_POLL_ATTEMPTS,
            delay: FILL_POLL_DELAY,
        }
    }

    /// A different polling window. Needed by the tests: there is no point sleeping in them.
    #[must_use]
    pub fn with_poll(mut self, attempts: u32, delay: std::time::Duration) -> Self {
        self.attempts = attempts;
        self.delay = delay;
        self
    }

    pub fn client(&self) -> &C {
        &self.client
    }
}

fn map_side(side: Side) -> ClobSide {
    match side {
        Side::Buy => ClobSide::Buy,
        Side::Sell => ClobSide::Sell,
    }
}

impl<C: ClobClient> ClobAdapter<C> {
    /// The weighted average price of our trades on this order.
    ///
    /// `None` when no trades are visible yet: the exchange does not show them instantly.
    /// The caller then takes the order's price — it overstates the cost rather than
    /// understating it, and the reconciler will correct the divergence.
    async fn traded(
        &self,
        order_id: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Option<(Decimal, Decimal)> {
        let trades = self.client.get_trades(since).await.ok()?;
        let mine: Vec<_> = trades
            .iter()
            .filter(|t| t.order_ids().any(|id| id == order_id))
            .collect();

        let size: Decimal = mine.iter().map(|t| t.size).sum();
        if size <= Decimal::ZERO {
            return None;
        }
        let notional: Decimal = mine.iter().map(|t| t.size * t.price).sum();
        Some((size, notional / size))
    }
}

impl<C: ClobClient + Sync> ClobExec for ClobAdapter<C> {
    async fn place_ioc(&self, req: &OrderRequest) -> Result<Fill, ExecError> {
        // Trades are read from the moment of submission: somebody else's matches on the
        // same token will not enter the selection.
        let submitted_at = chrono::Utc::now() - chrono::Duration::seconds(5);

        let placed = self
            .client
            .place_limit_order(
                &req.token_id,
                map_side(req.side),
                req.limit_price,
                req.size_shares,
                OrderKind::Fak,
            )
            .await
            // Before the exchange accepts the order a retry is safe: no money is committed.
            .map_err(|e| ExecError::Rejected(e.to_string()))?;

        // The order is accepted. From here any unclear response is "unknown" rather than
        // "refused": a retry here buys twice over. Measured 2026-09-04: two trades of
        // 7.142856 shares in the same second, $2.00 instead of $1.00.
        if matches!(
            placed.status,
            ClobOrderStatus::Cancelled | ClobOrderStatus::Expired
        ) {
            return Err(ExecError::Rejected(format!(
                "the exchange rejected the order: {:?}",
                placed.status
            )));
        }

        for attempt in 0..self.attempts {
            // The feed is the source of truth: it carries both the size and the real price.
            // `OrderInfo.price` is the **order's** price, and recording by it overstated the
            // cost by 15% (measured 2026-09-04: a limit of 0.021 against a fill at
            // 0.0182).
            if let Some((size, avg_price)) = self.traded(&placed.order_id, submitted_at).await {
                return Ok(Fill {
                    size,
                    avg_price,
                    notional: size * avg_price,
                    // The fee is set by the caller from the market's `taker_fee`.
                    fee_usd: Decimal::ZERO,
                    source: FillSource::Clob,
                });
            }

            let info = self.client.get_order(&placed.order_id).await.map_err(|e| {
                ExecError::Unknown(format!(
                    "the order was submitted, its state was not read: {e}"
                ))
            })?;

            if info.size_matched > Decimal::ZERO {
                // There is a size but no trade yet: the order's price as an upper bound.
                // It overstates the cost rather than understating it, and the reconciler
                // will correct that.
                return Ok(Fill {
                    size: info.size_matched,
                    avg_price: info.price,
                    notional: info.size_matched * info.price,
                    fee_usd: Decimal::ZERO,
                    source: FillSource::Clob,
                });
            }

            // FAK does not rest in the book: a cancelled order with zero filled is an honest
            // "the book gave nothing inside the limit".
            if matches!(
                info.status,
                ClobOrderStatus::Cancelled | ClobOrderStatus::Expired
            ) {
                return Err(ExecError::Rejected(
                    "the book gave nothing inside the limit".to_string(),
                ));
            }

            if attempt + 1 < self.attempts {
                tokio::time::sleep(self.delay).await;
            }
        }

        Err(ExecError::Unknown(format!(
            "order {} was accepted, the fill was not confirmed within {:?}",
            placed.order_id,
            self.delay * self.attempts
        )))
    }
}
