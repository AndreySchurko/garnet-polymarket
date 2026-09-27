//! Positions.
//!
//! The key is the triple `(wallet, token_id, mode)`. The mode is part of the key because
//! a wallet is switched between shadow and live while holding open positions: without
//! the mode, paper and real lots would add up into one row and the ledger would become a
//! lie.

use crate::Mode;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Position {
    pub id: i64,
    pub wallet: String,
    pub token_id: String,
    pub mode: Mode,
    pub size_bought: Decimal,
    pub size_sold: Decimal,
    pub cost_usd: Decimal,
    pub proceeds_usd: Decimal,
    pub fees_usd: Decimal,
    /// The leader's position as we saw it. The fraction of a sale is computed against this
    /// rather than the real one: before the wallet was added, its trades were invisible.
    pub leader_observed_size: Decimal,
    pub attempts: i32,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub opened_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    /// Shares the leader has already sold that we deferred: the fraction did not reach the
    /// exchange minimum. It accumulates until it does.
    pub pending_exit_shares: Decimal,
    /// The leader's price the floor of the deferred exit was computed from. A retry takes
    /// that rather than today's bid: a bid would mean a new decision, while a retry
    /// carries the old one through. Zero — there is nothing to retry.
    pub pending_exit_price: Decimal,
}

impl Position {
    /// How many shares we have left.
    pub fn open_size(&self) -> Decimal {
        self.size_bought - self.size_sold
    }
}

pub struct PositionRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> PositionRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Our buy: it increases lots, cost and fees.
    pub async fn apply_buy(
        &self,
        wallet: &str,
        token_id: &str,
        mode: Mode,
        size: Decimal,
        notional: Decimal,
        fee: Decimal,
    ) -> anyhow::Result<Position> {
        let p = sqlx::query_as::<_, Position>(
            "INSERT INTO positions (wallet, token_id, mode, size_bought, cost_usd, fees_usd)
             VALUES ($1, $2, $3::mode, $4, $5, $6)
             ON CONFLICT (wallet, token_id, mode) DO UPDATE SET
               size_bought = positions.size_bought + EXCLUDED.size_bought,
               cost_usd    = positions.cost_usd    + EXCLUDED.cost_usd,
               fees_usd    = positions.fees_usd    + EXCLUDED.fees_usd
             RETURNING *",
        )
        .bind(wallet)
        .bind(token_id)
        .bind(mode.as_str())
        .bind(size)
        .bind(notional)
        .bind(fee)
        .fetch_one(self.pool)
        .await?;
        Ok(p)
    }

    /// Our sale: it increases the amount sold and the proceeds.
    pub async fn apply_sell(
        &self,
        wallet: &str,
        token_id: &str,
        mode: Mode,
        size: Decimal,
        notional: Decimal,
        fee: Decimal,
    ) -> anyhow::Result<Position> {
        let p = sqlx::query_as::<_, Position>(
            "UPDATE positions SET
               size_sold    = size_sold + $4,
               proceeds_usd = proceeds_usd + $5,
               fees_usd     = fees_usd + $6,
               closed_at    = CASE WHEN size_bought - (size_sold + $4) <= 0
                                   THEN now() ELSE closed_at END
             WHERE wallet = $1 AND token_id = $2 AND mode = $3::mode
             RETURNING *",
        )
        .bind(wallet)
        .bind(token_id)
        .bind(mode.as_str())
        .bind(size)
        .bind(notional)
        .bind(fee)
        .fetch_one(self.pool)
        .await?;
        Ok(p)
    }

    /// Remembers how much the leader accumulated in front of us.
    pub async fn observe_leader(
        &self,
        wallet: &str,
        token_id: &str,
        mode: Mode,
        delta: Decimal,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO positions (wallet, token_id, mode, leader_observed_size)
             VALUES ($1, $2, $3::mode, $4)
             ON CONFLICT (wallet, token_id, mode) DO UPDATE SET
               leader_observed_size = positions.leader_observed_size + EXCLUDED.leader_observed_size",
        )
        .bind(wallet)
        .bind(token_id)
        .bind(mode.as_str())
        .bind(delta)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(
        &self,
        wallet: &str,
        token_id: &str,
        mode: Mode,
    ) -> anyhow::Result<Option<Position>> {
        let p = sqlx::query_as::<_, Position>(
            "SELECT * FROM positions WHERE wallet = $1 AND token_id = $2 AND mode = $3::mode",
        )
        .bind(wallet)
        .bind(token_id)
        .bind(mode.as_str())
        .fetch_optional(self.pool)
        .await?;
        Ok(p)
    }

    pub async fn for_wallet(&self, wallet: &str) -> anyhow::Result<Vec<Position>> {
        let ps =
            sqlx::query_as::<_, Position>("SELECT * FROM positions WHERE wallet = $1 ORDER BY id")
                .bind(wallet)
                .fetch_all(self.pool)
                .await?;
        Ok(ps)
    }

    /// The settlement queue: **oldest to newest**. In the predecessor the queue ran
    /// newest-first, without counting attempts, and jammed completely.
    pub async fn open_for_settlement(&self, limit: i64) -> anyhow::Result<Vec<Position>> {
        let ps = sqlx::query_as::<_, Position>(
            "SELECT * FROM positions
             WHERE closed_at IS NULL AND size_bought > size_sold
             ORDER BY opened_at ASC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(ps)
    }

    pub async fn note_attempt(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE positions SET attempts = attempts + 1, last_attempt_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn close(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE positions SET closed_at = now() WHERE id = $1")
            .bind(id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// How much of a mode's money stands in open positions of the same event.
    ///
    /// The event, not the token: the outcomes of one condition are correlated, and a
    /// ceiling set per token is bypassed by buying the neighbouring outcome. Tokens with
    /// no row in `markets` do not count — their condition is unknown to us, and an invented
    /// one would be worse than none.
    pub async fn open_cost_in_condition(
        &self,
        condition_id: &str,
        mode: Mode,
    ) -> anyhow::Result<Decimal> {
        let (sum,): (Decimal,) = sqlx::query_as(
            "SELECT COALESCE(sum(p.cost_usd), 0)
               FROM positions p
               JOIN markets m ON m.token_id = p.token_id
              WHERE p.mode = $2::mode AND p.closed_at IS NULL AND m.condition_id = $1",
        )
        .bind(condition_id)
        .bind(mode.as_str())
        .fetch_one(self.pool)
        .await?;
        Ok(sum)
    }

    /// Every open live position, oldest to newest.
    ///
    /// Paper ones are deliberately absent: an emergency close concerns money, and paper
    /// positions are not money — there is nothing to save in them, and closing them would
    /// destroy the only comparison shadow exists for.
    pub async fn open_live(&self) -> anyhow::Result<Vec<Position>> {
        let rows = sqlx::query_as::<_, Position>(
            "SELECT * FROM positions
              WHERE mode = 'live' AND closed_at IS NULL AND size_bought > size_sold
              ORDER BY opened_at ASC",
        )
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    /// Our open positions for a wallet within one condition — that is, every leg.
    ///
    /// Needed for a leader merge (invariant 40): it burns the whole pair, and our response
    /// concerns **both** legs. The lookup goes through `markets`, where
    /// `token_id -> condition_id`; a merge does not have one token, so there is nothing to
    /// ask by token here.
    pub async fn open_in_condition(
        &self,
        wallet: &str,
        condition_id: &str,
        mode: Mode,
    ) -> anyhow::Result<Vec<Position>> {
        let rows = sqlx::query_as::<_, Position>(
            "SELECT p.* FROM positions p
               JOIN markets m ON m.token_id = p.token_id
              WHERE p.wallet = $1 AND m.condition_id = $2 AND p.mode = $3::mode
                AND p.closed_at IS NULL AND p.size_bought > p.size_sold
              ORDER BY p.token_id",
        )
        .bind(wallet)
        .bind(condition_id)
        .bind(mode.as_str())
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    /// Open positions older than `hours`, oldest first.
    ///
    /// Age is measured from the opening, not from the last add-on buy: a position topped
    /// up near the end of the window has sat around just as long — the money in it has
    /// been tied up since day one.
    pub async fn stale_open(&self, hours: i64, limit: i64) -> anyhow::Result<Vec<Position>> {
        let rows = sqlx::query_as::<_, Position>(
            "SELECT * FROM positions
              WHERE closed_at IS NULL
                AND size_bought > size_sold
                AND opened_at < now() - make_interval(hours => $1::int)
              ORDER BY opened_at ASC
              LIMIT $2",
        )
        .bind(i32::try_from(hours).unwrap_or(i32::MAX))
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    /// Record a deferred exit. Zero shares means "nothing is owed".
    ///
    /// The price is written alongside the shares and cleared together with them: a floor
    /// without shares is junk that a retry would mistake for work.
    pub async fn set_pending_exit(
        &self,
        id: i64,
        shares: Decimal,
        price: Decimal,
    ) -> anyhow::Result<()> {
        let shares = shares.max(Decimal::ZERO);
        let price = if shares > Decimal::ZERO {
            price.max(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };
        sqlx::query(
            "UPDATE positions
                SET pending_exit_shares = $2, pending_exit_price = $3
              WHERE id = $1",
        )
        .bind(id)
        .bind(shares)
        .bind(price)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Open positions with a deferred exit, oldest first.
    ///
    /// Without a floor there is nothing to retry: positions that were deferred before the
    /// price column existed (migration 8) are not picked up — a retry would aim at zero and
    /// sell at any price.
    pub async fn pending_open(&self, limit: i64) -> anyhow::Result<Vec<Position>> {
        let rows = sqlx::query_as::<_, Position>(
            "SELECT * FROM positions
              WHERE closed_at IS NULL
                AND size_bought > size_sold
                AND pending_exit_shares > 0
                AND pending_exit_price > 0
              ORDER BY opened_at ASC
              LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }
}
