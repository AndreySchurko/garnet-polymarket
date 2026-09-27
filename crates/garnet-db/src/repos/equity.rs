//! Equity snapshots — the source of the chart on the dashboard's main view.

use crate::Mode;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EquitySnapshot {
    pub id: i64,
    pub mode: Mode,
    pub cash_usd: Decimal,
    pub positions_value: Decimal,
    pub total_usd: Decimal,
    /// How many open positions entered the snapshot without a price. Their value is not
    /// counted in the total, and the reader needs to know: otherwise the total looks more
    /// precise than it is.
    pub unpriced: i32,
    pub ts: DateTime<Utc>,
}

pub struct EquityRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> EquityRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn record(
        &self,
        mode: Mode,
        cash_usd: Decimal,
        positions_value: Decimal,
        unpriced: i32,
    ) -> anyhow::Result<EquitySnapshot> {
        let s = sqlx::query_as::<_, EquitySnapshot>(
            "INSERT INTO equity_snapshots
               (mode, cash_usd, positions_value, total_usd, unpriced)
             VALUES ($1::mode, $2, $3, $4, $5) RETURNING *",
        )
        .bind(mode.as_str())
        .bind(cash_usd)
        .bind(positions_value)
        .bind(cash_usd + positions_value)
        .bind(unpriced)
        .fetch_one(self.pool)
        .await?;
        Ok(s)
    }

    /// The chart's points over a period, oldest to newest.
    pub async fn series(&self, mode: Mode, limit: i64) -> anyhow::Result<Vec<EquitySnapshot>> {
        let rows = sqlx::query_as::<_, EquitySnapshot>(
            "SELECT * FROM (
               SELECT * FROM equity_snapshots WHERE mode = $1::mode ORDER BY ts DESC LIMIT $2
             ) t ORDER BY ts ASC",
        )
        .bind(mode.as_str())
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn latest(&self, mode: Mode) -> anyhow::Result<Option<EquitySnapshot>> {
        let s = sqlx::query_as::<_, EquitySnapshot>(
            "SELECT * FROM equity_snapshots WHERE mode = $1::mode ORDER BY ts DESC LIMIT 1",
        )
        .bind(mode.as_str())
        .fetch_optional(self.pool)
        .await?;
        Ok(s)
    }

    /// Shadow's virtual account: the starting capital minus everything the paper mode
    /// spent, plus everything it took in from proceeds and payouts.
    ///
    /// Computed **from the database, not from process memory**. On 05.09.2026 the account
    /// was a `Mutex` inside `App` and was taken from the config at startup: a restart
    /// gifted it $14,854 and broke the series by which shadow's drawdown is measured. It
    /// also knew nothing of payouts — the ledger changed only on fills, and every
    /// resolution understated the account by its full amount.
    ///
    /// It goes negative and does not block trading: shadow is a measuring instrument.
    pub async fn shadow_cash(&self, initial_capital: Decimal) -> anyhow::Result<Decimal> {
        let (spent, fees, proceeds): (Decimal, Decimal, Decimal) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(fees_usd), 0),
                    COALESCE(SUM(proceeds_usd), 0)
             FROM positions WHERE mode = 'shadow'::mode",
        )
        .fetch_one(self.pool)
        .await?;

        let (payouts,): (Decimal,) = sqlx::query_as(
            "SELECT COALESCE(SUM(s.payout_usd), 0)
             FROM settlements s
             JOIN positions p ON p.id = s.position_id
             WHERE p.mode = 'shadow'::mode",
        )
        .fetch_one(self.pool)
        .await?;

        Ok(initial_capital - spent - fees + proceeds + payouts)
    }

    /// A mode's open positions: `(token_id, shares, invested)`.
    pub async fn open_exposure(
        &self,
        mode: Mode,
    ) -> anyhow::Result<Vec<(String, Decimal, Decimal)>> {
        let rows: Vec<(String, Decimal, Decimal)> = sqlx::query_as(
            "SELECT token_id, size_bought - size_sold, cost_usd
             FROM positions
             WHERE mode = $1::mode AND closed_at IS NULL AND size_bought > size_sold",
        )
        .bind(mode.as_str())
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }
}
