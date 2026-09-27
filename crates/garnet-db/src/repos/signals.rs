//! Signals, orders and fills — the trace of a decision from frame to position.

use crate::Mode;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Signal {
    pub id: i64,
    pub leader_trade_id: i64,
    pub wallet: String,
    pub mode: Mode,
    pub verdict: String,
    pub target_size_usd: Decimal,
    pub limit_price: Option<Decimal>,
    /// The top of the book at the moment of the decision. `None` — that side did not exist
    /// at all: "no price" and "a high price" must be distinguishable, otherwise the
    /// slippage threshold is tuned from a sample holding observations with no price.
    pub best_ask: Option<Decimal>,
    pub best_bid: Option<Decimal>,
    pub ts_signal: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Order {
    pub id: i64,
    pub signal_id: Option<i64>,
    pub token_id: String,
    pub mode: Mode,
    pub side: String,
    pub limit_price: Decimal,
    pub size_usd: Decimal,
    pub status: String,
    pub tx_hash: Option<String>,
    pub error: Option<String>,
    pub attempts: i32,
    pub ts_submitted: DateTime<Utc>,
    pub ts_filled: Option<DateTime<Utc>>,
}

pub struct SignalRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> SignalRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Written for **every** decision, refusals included: the list of refusal reasons is
    /// what quietly ate every signal in the predecessor, and it has to be visible.
    ///
    /// `limit_price` and `best_ask` are written on every verdict where we had them: a
    /// refusal without a price is a refusal about which nothing can be said, and a
    /// slippage threshold cannot be tuned from such a log at any sample size.
    #[allow(clippy::too_many_arguments)]
    pub async fn record(
        &self,
        leader_trade_id: i64,
        wallet: &str,
        mode: Mode,
        verdict: &str,
        target_size_usd: Decimal,
        limit_price: Option<Decimal>,
        best_ask: Option<Decimal>,
        best_bid: Option<Decimal>,
    ) -> anyhow::Result<Signal> {
        let s = sqlx::query_as::<_, Signal>(
            "INSERT INTO signals
               (leader_trade_id, wallet, mode, verdict, target_size_usd, limit_price,
                best_ask, best_bid)
             VALUES ($1, $2, $3::mode, $4, $5, $6, $7, $8) RETURNING *",
        )
        .bind(leader_trade_id)
        .bind(wallet)
        .bind(mode.as_str())
        .bind(verdict)
        .bind(target_size_usd)
        .bind(limit_price)
        .bind(best_ask)
        .bind(best_bid)
        .fetch_one(self.pool)
        .await?;
        Ok(s)
    }

    /// The timestamp of the leader trade we last followed with a buy in this market, or
    /// `None` if we never did.
    ///
    /// The time comes from the **leader**, not from our order: a wave is their order
    /// breaking against the book, and measuring it by our clock means measuring our own
    /// latency instead of their behaviour. A refusal from the exchange also closes the
    /// wave: we have already acted on the decision, and there is no point repeating it
    /// with a second slice.
    pub async fn last_copied_buy(
        &self,
        wallet: &str,
        token_id: &str,
        mode: Mode,
    ) -> anyhow::Result<Option<DateTime<Utc>>> {
        let row: Option<(DateTime<Utc>,)> = sqlx::query_as(
            "SELECT t.ts_trade
               FROM orders o
               JOIN signals s       ON s.id = o.signal_id
               JOIN leader_trades t ON t.id = s.leader_trade_id
              WHERE o.side = 'buy' AND o.mode = $3::mode
                AND o.token_id = $2 AND s.wallet = $1
              ORDER BY t.ts_trade DESC
              LIMIT 1",
        )
        .bind(wallet)
        .bind(token_id)
        .bind(mode.as_str())
        .fetch_optional(self.pool)
        .await?;
        Ok(row.map(|r| r.0))
    }

    pub async fn recent(&self, limit: i64) -> anyhow::Result<Vec<Signal>> {
        let rows = sqlx::query_as::<_, Signal>(
            "SELECT * FROM signals ORDER BY ts_signal DESC, id DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    /// `signal_id` is `None` for an order with no leader decision behind it: a time-based
    /// exit is our own, and a reference to someone else's trade would be a fabrication in
    /// the registry of decisions.
    pub async fn record_order(
        &self,
        signal_id: Option<i64>,
        token_id: &str,
        mode: Mode,
        side: &str,
        limit_price: Decimal,
        size_usd: Decimal,
        status: &str,
        error: Option<&str>,
        attempts: i32,
    ) -> anyhow::Result<Order> {
        let o = sqlx::query_as::<_, Order>(
            "INSERT INTO orders
               (signal_id, token_id, mode, side, limit_price, size_usd, status, error, attempts,
                ts_filled)
             VALUES ($1, $2, $3::mode, $4, $5, $6, $7, $8, $9,
                     CASE WHEN $7 IN ('filled','partial') THEN now() ELSE NULL END)
             RETURNING *",
        )
        .bind(signal_id)
        .bind(token_id)
        .bind(mode.as_str())
        .bind(side)
        .bind(limit_price)
        .bind(size_usd)
        .bind(status)
        .bind(error)
        .bind(attempts)
        .fetch_one(self.pool)
        .await?;
        Ok(o)
    }

    #[allow(clippy::too_many_arguments)]
    /// The last recorded order. Needed by the execution checks: the decision is visible in
    /// `signals`, while the order's fate is only here.
    /// Orders in flight for a token, **in shares**: buys and sells separately.
    ///
    /// "In flight" means `submitted` and `unknown`. The second status is the main one for
    /// us: execution is IOC, the order does not rest in the book, and `submitted` is
    /// written nowhere. An `unknown` row is created exactly when the exchange accepted the
    /// order while the outcome is unknown to us — and, in the words of migration 0002, it
    /// is created so that the reconciler will see it.
    ///
    /// Shares are reconstructed as `size_usd / limit_price`. In rows written before
    /// 19.09.2026 both values are zero — such an order explains nothing, and that is more
    /// honest than substituting an invented size into it.
    ///
    /// `since` is mandatory: nobody ever changes the `unknown` status, and without an
    /// expiry such a row would explain a divergence forever.
    pub async fn in_flight_shares(
        &self,
        token_id: &str,
        mode: Mode,
        since: DateTime<Utc>,
    ) -> anyhow::Result<(Decimal, Decimal)> {
        let row: (Decimal, Decimal) = sqlx::query_as(
            "SELECT
               COALESCE(sum(CASE WHEN side = 'buy'  AND limit_price > 0
                                 THEN size_usd / limit_price ELSE 0 END), 0),
               COALESCE(sum(CASE WHEN side = 'sell' AND limit_price > 0
                                 THEN size_usd / limit_price ELSE 0 END), 0)
             FROM orders
            WHERE token_id = $1 AND mode = $2::mode
              AND status IN ('submitted', 'unknown')
              AND ts_submitted >= $3",
        )
        .bind(token_id)
        .bind(mode.as_str())
        .bind(since)
        .fetch_one(self.pool)
        .await?;
        Ok(row)
    }

    /// An order by its id. Needed by the latency measurement: its timestamps are the same
    /// ones anyone who opens the registry later will see.
    pub async fn order_by_id(&self, id: i64) -> anyhow::Result<Option<Order>> {
        let o = sqlx::query_as::<_, Order>("SELECT * FROM orders WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool)
            .await?;
        Ok(o)
    }

    pub async fn last_order(&self) -> anyhow::Result<Option<Order>> {
        let o = sqlx::query_as::<_, Order>("SELECT * FROM orders ORDER BY id DESC LIMIT 1")
            .fetch_optional(self.pool)
            .await?;
        Ok(o)
    }

    /// Eight arguments because a fill has eight fields; folding them into a struct
    /// for the sake of a lint would add a type that exactly one call site needs.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_fill(
        &self,
        order_id: i64,
        mode: Mode,
        source: &str,
        size: Decimal,
        avg_price: Decimal,
        notional: Decimal,
        fee_usd: Decimal,
    ) -> anyhow::Result<i64> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO fills (order_id, mode, source, size, avg_price, notional, fee_usd)
             VALUES ($1, $2::mode, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(order_id)
        .bind(mode.as_str())
        .bind(source)
        .bind(size)
        .bind(avg_price)
        .bind(notional)
        .bind(fee_usd)
        .fetch_one(self.pool)
        .await?;
        Ok(id)
    }
}
