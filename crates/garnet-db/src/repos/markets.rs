//! The market registry: what we learned about a token, outliving the process.
//!
//! The metadata cache lives in the process for ten minutes and does not survive a
//! restart. That would not matter if the metadata could always be asked for again, but
//! **a resolved market loses its order book** (invariant 18), and the token -> condition
//! path was exactly what we used to fetch from `/book`. At precisely that moment
//! settlement needs `condition_id`, and the only fallback is Gamma.
//!
//! The row is updated on every encounter rather than written once: a market closes and
//! resolves, and a record that has fallen behind reality is worse than none.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPool;

/// What we know about a market at the moment of the encounter.
#[derive(Debug, Clone)]
pub struct NewMarket {
    pub token_id: String,
    pub condition_id: String,
    pub question: String,
    /// The real label of the side: Up/Down, Over/Under, a team name. Not Yes/No —
    /// settling by label books every win as a loss.
    pub outcome_label: String,
    pub category: Option<String>,
    pub game_start_time: Option<DateTime<Utc>>,
    pub end_date: Option<DateTime<Utc>>,
    pub neg_risk: bool,
    pub fee_rate: Decimal,
    pub fee_exponent: Decimal,
    pub fee_taker_only: bool,
    /// Only from `tokens[].winner`. A sale at 0.99 is not a resolution.
    pub resolved_outcome: Option<String>,
    pub closed: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Market {
    pub token_id: String,
    pub condition_id: String,
    pub question: String,
    pub outcome_label: String,
    pub category: Option<String>,
    pub game_start_time: Option<DateTime<Utc>>,
    pub end_date: Option<DateTime<Utc>>,
    pub neg_risk: bool,
    pub fee_rate: Decimal,
    pub fee_exponent: Decimal,
    pub fee_taker_only: bool,
    pub resolved_outcome: Option<String>,
    pub closed: bool,
    pub updated_at: DateTime<Utc>,
}

pub struct MarketRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> MarketRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Record an encounter with a market. The key is `token_id`, so a second encounter
    /// updates the row.
    pub async fn upsert(&self, m: &NewMarket) -> anyhow::Result<Market> {
        let row = sqlx::query_as::<_, Market>(
            "INSERT INTO markets
               (token_id, condition_id, question, outcome_label, category,
                game_start_time, end_date, neg_risk, fee_rate, fee_exponent,
                fee_taker_only, resolved_outcome, closed, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13, now())
             ON CONFLICT (token_id) DO UPDATE SET
               condition_id     = EXCLUDED.condition_id,
               question         = EXCLUDED.question,
               outcome_label    = EXCLUDED.outcome_label,
               category         = EXCLUDED.category,
               game_start_time  = EXCLUDED.game_start_time,
               end_date         = EXCLUDED.end_date,
               neg_risk         = EXCLUDED.neg_risk,
               fee_rate         = EXCLUDED.fee_rate,
               fee_exponent     = EXCLUDED.fee_exponent,
               fee_taker_only   = EXCLUDED.fee_taker_only,
               -- A resolution is not erased by a fresher response lacking it: the
               -- winner can be learned once, but forgotten any number of times.
               resolved_outcome = COALESCE(EXCLUDED.resolved_outcome, markets.resolved_outcome),
               closed           = EXCLUDED.closed OR markets.closed,
               updated_at       = now()
             RETURNING *",
        )
        .bind(&m.token_id)
        .bind(&m.condition_id)
        .bind(&m.question)
        .bind(&m.outcome_label)
        .bind(&m.category)
        .bind(m.game_start_time)
        .bind(m.end_date)
        .bind(m.neg_risk)
        .bind(m.fee_rate)
        .bind(m.fee_exponent)
        .bind(m.fee_taker_only)
        .bind(&m.resolved_outcome)
        .bind(m.closed)
        .fetch_one(self.pool)
        .await?;
        Ok(row)
    }

    pub async fn get(&self, token_id: &str) -> anyhow::Result<Option<Market>> {
        let row = sqlx::query_as::<_, Market>("SELECT * FROM markets WHERE token_id = $1")
            .bind(token_id)
            .fetch_optional(self.pool)
            .await?;
        Ok(row)
    }

    /// The condition for a token. The very question `/book` stops answering at exactly the
    /// moment the answer is needed.
    pub async fn condition_of(&self, token_id: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT condition_id FROM markets WHERE token_id = $1")
                .bind(token_id)
                .fetch_optional(self.pool)
                .await?;
        Ok(row.map(|r| r.0))
    }

    pub async fn count(&self) -> anyhow::Result<i64> {
        let row: (i64,) = sqlx::query_as("SELECT count(*) FROM markets")
            .fetch_one(self.pool)
            .await?;
        Ok(row.0)
    }
}
