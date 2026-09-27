//! Leader actions that are not trades but do change the position.
//!
//! Invariant 40. Merging a pair is an exit at $1 on both legs; neither RTDS nor
//! `/activity` reports it as a `TRADE`, and until 19.09.2026 it was silently discarded.
//! A position the leader merged out of was held by us until resolution with a
//! `leader_observed_size` that no longer meant anything.
//!
//! The table is separate from `leader_trades` for a mechanical reason: in Postgres NULL
//! in a UNIQUE constraint is not equal to itself, so the key `(tx_hash, wallet,
//! token_id, side)` with a NULL side would stop catching repeats — one and the same
//! merge, delivered by two circuits, would be inserted twice and would sell our
//! position twice.

use crate::Source;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPool;

#[derive(Debug, Clone)]
pub struct NewLeaderAction {
    pub wallet: String,
    pub tx_hash: String,
    pub condition_id: String,
    /// `merge` | `split` | `redeem`.
    pub kind: String,
    pub size: Decimal,
    pub ts_action: DateTime<Utc>,
    pub source: Source,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LeaderActionRow {
    pub id: i64,
    pub wallet: String,
    pub tx_hash: String,
    pub condition_id: String,
    pub kind: String,
    pub size: Decimal,
    pub ts_action: DateTime<Utc>,
    pub ts_seen: DateTime<Utc>,
    pub source: String,
    pub handled_at: Option<DateTime<Utc>>,
}

pub struct ActionRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> ActionRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Record an action. `None` — it was already there.
    ///
    /// The same technique as for trades: a repeat is cut off by the key, not by a check
    /// before the insert. Two circuits deliver the same event, and "check then insert" is
    /// a race in which the merge is acted upon twice.
    pub async fn insert_new(&self, a: &NewLeaderAction) -> anyhow::Result<Option<LeaderActionRow>> {
        let row = sqlx::query_as::<_, LeaderActionRow>(
            "INSERT INTO leader_actions
               (wallet, tx_hash, condition_id, kind, size, ts_action, source)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (tx_hash, wallet, condition_id, kind) DO NOTHING
             RETURNING *",
        )
        .bind(&a.wallet)
        .bind(&a.tx_hash)
        .bind(&a.condition_id)
        .bind(&a.kind)
        .bind(a.size)
        .bind(a.ts_action)
        .bind(a.source.as_str())
        .fetch_optional(self.pool)
        .await?;
        Ok(row)
    }

    /// Note that an action has been acted upon.
    ///
    /// Kept separate from `ts_seen`: seeing and acting are different events. A merge we
    /// saw and did not exit on (a stop, an empty book) must stay visible rather than look
    /// done.
    pub async fn mark_handled(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE leader_actions SET handled_at = now() WHERE id = $1")
            .bind(id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// The most recent actions, newest first.
    pub async fn recent(&self, limit: i64) -> anyhow::Result<Vec<LeaderActionRow>> {
        let rows = sqlx::query_as::<_, LeaderActionRow>(
            "SELECT * FROM leader_actions ORDER BY id DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    /// Merges we saw and did not act upon. Read by `garnet-watch` and by the operator's
    /// summary.
    pub async fn unhandled_merges(&self, limit: i64) -> anyhow::Result<Vec<LeaderActionRow>> {
        let rows = sqlx::query_as::<_, LeaderActionRow>(
            "SELECT * FROM leader_actions
              WHERE kind = 'merge' AND handled_at IS NULL
              ORDER BY ts_action ASC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }
}
