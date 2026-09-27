//! Resolution and payout.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Settlement {
    pub id: i64,
    pub position_id: i64,
    pub token_id: String,
    pub resolved_outcome: String,
    pub won: bool,
    pub payout_usd: Decimal,
    pub tx_hash: Option<String>,
    pub ts: DateTime<Utc>,
}

pub struct SettlementRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> SettlementRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn record(
        &self,
        position_id: i64,
        token_id: &str,
        resolved_outcome: &str,
        won: bool,
        payout_usd: Decimal,
        tx_hash: Option<&str>,
    ) -> anyhow::Result<Settlement> {
        let s = sqlx::query_as::<_, Settlement>(
            "INSERT INTO settlements
               (position_id, token_id, resolved_outcome, won, payout_usd, tx_hash)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
        )
        .bind(position_id)
        .bind(token_id)
        .bind(resolved_outcome)
        .bind(won)
        .bind(payout_usd)
        .bind(tx_hash)
        .fetch_one(self.pool)
        .await?;
        Ok(s)
    }

    pub async fn for_position(&self, position_id: i64) -> anyhow::Result<Option<Settlement>> {
        let s = sqlx::query_as::<_, Settlement>("SELECT * FROM settlements WHERE position_id = $1")
            .bind(position_id)
            .fetch_optional(self.pool)
            .await?;
        Ok(s)
    }
}
