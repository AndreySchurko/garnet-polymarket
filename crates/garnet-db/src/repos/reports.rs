//! Summaries for readers: Telegram first, the dashboard after.
//!
//! Two rules common to the whole file:
//!
//! * **the modes are never added together.** A paper result next to a real one is the
//!   only comparison shadow exists for, and a sum destroys it;
//! * **a position's result is payout and proceeds against stake and fees.** The proceeds
//!   of a sale must not be lost: the leader exits before resolution nine times out of
//!   ten, and where they exited and we followed, the entire result lies in it.

use crate::Mode;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPool;
use std::collections::HashMap;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OpenPosition {
    pub wallet: String,
    pub nickname: Option<String>,
    pub token_id: String,
    pub mode: Mode,
    pub size: Decimal,
    pub cost_usd: Decimal,
    /// The human-readable outcome text from the leader trade: `markets` is not yet
    /// populated, and without it the row is unreadable.
    pub outcome_text: String,
    pub opened_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ModePnl {
    pub mode: Mode,
    pub closed: i64,
    pub won: i64,
    pub cost_usd: Decimal,
    pub fees_usd: Decimal,
    pub payout_usd: Decimal,
    pub proceeds_usd: Decimal,
    pub pnl_usd: Decimal,
}

/// One bucket of the price miss: how far the best ask was above the leader's price.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SlippageBucket {
    /// The bucket's lower bound as a fraction: 0.00, 0.01, 0.02, 0.03, 0.05, 0.10.
    pub from_pct: Decimal,
    pub copied: i64,
    pub skipped: i64,
    /// Closed positions from this bucket and their result. It answers the question without
    /// which the threshold is not moved: **do expensive fills lose money**.
    pub closed: i64,
    pub cost_usd: Decimal,
    pub pnl_usd: Decimal,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SignalRow {
    pub ts_signal: DateTime<Utc>,
    pub wallet: String,
    pub nickname: Option<String>,
    pub mode: Mode,
    pub verdict: String,
    pub target_size_usd: Decimal,
    pub outcome_text: String,
    pub market_text: String,
    /// The side of the leader trade. On an exit `target_size_usd` is zero, and without the
    /// side "for $0" reads as an arithmetic error rather than as a sale.
    pub side: String,
    /// Both halves of the slippage miss. `None` in `best_ask` means "there was no book",
    /// not "the price was zero": a refusal without a price and a refusal on price are
    /// different outcomes, and they must not be shown to the reader identically either.
    pub best_ask: Option<Decimal>,
    pub limit_price: Option<Decimal>,
}

/// A row of the circuit race: who brings trades and who merely confirms them.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SourceRace {
    pub source: String,
    /// How many times this circuit saw a trade first.
    pub wins: i64,
    /// How many times it brought what another had already brought.
    pub confirmations: i64,
    /// The median lag behind the first, in seconds. For a circuit that is always first
    /// this is zero.
    pub median_lag_secs: Option<Decimal>,
    /// How many trades **only** it brought. This is the answer to the question about the
    /// poll: zero means it pays in duplicates for nothing.
    pub only_source: i64,
    pub sightings: i64,
}

/// One bucket of the entry-queue depth distribution.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FireDepth {
    pub mode: Mode,
    /// How many copies this wallet already had within the window, the current one included.
    pub depth: i64,
    pub copies: i64,
}

/// One stage of the signal path, with its distribution.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LatencyStage {
    pub stage: String,
    pub n: i64,
    pub median_secs: Option<Decimal>,
    pub p90_secs: Option<Decimal>,
}

pub struct ReportRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> ReportRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// What we hold right now, both modes, newest to oldest.
    pub async fn open_positions(&self) -> anyhow::Result<Vec<OpenPosition>> {
        let rows = sqlx::query_as::<_, OpenPosition>(
            "SELECT p.wallet, w.nickname, p.token_id, p.mode,
                    p.size_bought - p.size_sold AS size,
                    p.cost_usd,
                    COALESCE((SELECT t.outcome_text FROM leader_trades t
                              WHERE t.token_id = p.token_id AND t.outcome_text <> ''
                              ORDER BY t.id DESC LIMIT 1), '') AS outcome_text,
                    p.opened_at
             FROM positions p
             JOIN wallets w ON w.address = p.wallet
             WHERE p.closed_at IS NULL AND p.size_bought > p.size_sold
             ORDER BY p.opened_at DESC",
        )
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    /// The realised result per mode. `since = None` — over all time.
    ///
    /// Computed from **closed** positions: while a position is open its result is not yet
    /// an event but a quote.
    pub async fn realised_pnl(&self, since: Option<DateTime<Utc>>) -> anyhow::Result<Vec<ModePnl>> {
        let rows = sqlx::query_as::<_, ModePnl>(
            "SELECT p.mode,
                    count(*)                                        AS closed,
                    count(*) FILTER (WHERE s.won)                   AS won,
                    COALESCE(sum(p.cost_usd), 0)                    AS cost_usd,
                    COALESCE(sum(p.fees_usd), 0)                    AS fees_usd,
                    COALESCE(sum(s.payout_usd), 0)                  AS payout_usd,
                    COALESCE(sum(p.proceeds_usd), 0)                AS proceeds_usd,
                    COALESCE(sum(COALESCE(s.payout_usd, 0) + p.proceeds_usd
                                 - p.cost_usd - p.fees_usd), 0)     AS pnl_usd
             FROM positions p
             LEFT JOIN settlements s ON s.position_id = p.id
             WHERE p.closed_at IS NOT NULL
               AND ($1::timestamptz IS NULL OR p.closed_at >= $1)
             GROUP BY p.mode
             ORDER BY p.mode",
        )
        .bind(since)
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    /// The result per wallet over a period: it answers which of them pays.
    pub async fn pnl_by_wallet(
        &self,
        since: Option<DateTime<Utc>>,
    ) -> anyhow::Result<HashMap<String, Decimal>> {
        let rows: Vec<(String, Decimal)> = sqlx::query_as(
            "SELECT p.wallet,
                    COALESCE(sum(COALESCE(s.payout_usd, 0) + p.proceeds_usd
                                 - p.cost_usd - p.fees_usd), 0)
             FROM positions p
             LEFT JOIN settlements s ON s.position_id = p.id
             WHERE p.closed_at IS NOT NULL
               AND ($1::timestamptz IS NULL OR p.closed_at >= $1)
             GROUP BY p.wallet",
        )
        .bind(since)
        .fetch_all(self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// The earliest equity snapshot in the window — the baseline for the delta.
    ///
    /// Specifically the earliest **in the window**, not "exactly 24 hours ago": there may
    /// have been no snapshots that day at all, and demanding an exact timestamp means
    /// never showing the delta.
    pub async fn equity_at(
        &self,
        mode: Mode,
        since: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Option<crate::EquitySnapshot>> {
        let row = sqlx::query_as::<_, crate::EquitySnapshot>(
            "SELECT * FROM equity_snapshots
             WHERE mode = $1::mode AND ($2::timestamptz IS NULL OR ts >= $2)
             ORDER BY ts ASC LIMIT 1",
        )
        .bind(mode.as_str())
        .bind(since)
        .fetch_optional(self.pool)
        .await?;
        Ok(row)
    }

    /// The most recent decisions, newest first.
    ///
    /// A skip with its reason is the main thing here: it answers the question "why did the
    /// bot not copy", which is why anyone looks at this list.
    pub async fn recent_signals(&self, limit: i64) -> anyhow::Result<Vec<SignalRow>> {
        let rows = sqlx::query_as::<_, SignalRow>(
            "SELECT s.ts_signal, s.wallet, w.nickname, s.mode, s.verdict,
                    s.target_size_usd, t.outcome_text, t.market_text, t.side,
                    s.best_ask, s.limit_price
             FROM signals s
             JOIN wallets w      ON w.address = s.wallet
             JOIN leader_trades t ON t.id = s.leader_trade_id
             ORDER BY s.id DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    /// The distribution of entry-queue depth per wallet, made into a permanent summary.
    ///
    /// It answers the one question without which `fire_limit` cannot be set: **how often
    /// does the leader fire a burst** (invariant 33 — a threshold from an instrument, not
    /// from a guess). A measurement on Garnet's own production data could not be made:
    /// the database went down together with the box on 18.09.2026, and the archive that
    /// was brought over turned out to be the predecessor's. So it lives here and is
    /// computed on whatever data accumulates.
    ///
    /// **Depth is measured in copies, not in signals.** A refusal spends no capital, and
    /// its presence in the distribution would inflate every bucket: against the
    /// `baseline-seed.sql` fixture the first revision of this query counted refusals as
    /// observations, and a queue of twelve copies interleaved with twelve refusals doubled
    /// buckets 2-12.
    ///
    /// The window is an argument rather than taken from the config: the summary is looked
    /// at precisely in order to compare windows with each other.
    pub async fn fire_depth(&self, window_secs: i64) -> anyhow::Result<Vec<FireDepth>> {
        let rows = sqlx::query_as::<_, FireDepth>(
            "WITH depth AS (
               SELECT wallet, mode, verdict, ts_signal,
                      count(*) FILTER (WHERE verdict = 'copy') OVER (
                        PARTITION BY wallet, mode
                        ORDER BY ts_signal
                        RANGE BETWEEN $1::interval PRECEDING AND CURRENT ROW
                      ) AS depth
                 FROM signals
             )
             SELECT mode, depth, count(*) AS copies
               FROM depth
              WHERE verdict = 'copy'
              GROUP BY mode, depth
              ORDER BY mode, depth",
        )
        .bind(sqlx::postgres::types::PgInterval {
            months: 0,
            days: 0,
            microseconds: window_secs.max(0) * 1_000_000,
        })
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    /// Latency per stage of the signal path (invariant 42).
    ///
    /// Computed **from the database, not from process metrics**. Metrics live in the
    /// trading process's memory: neither the bot nor the operator sees them after a
    /// restart, and "before and after" has to be compared precisely across a restart —
    /// otherwise there is nothing to compare. The registry's timestamps are the same ones
    /// anyone will investigate with later.
    ///
    /// Negative differences are clamped to zero: the database's clock and the exchange's
    /// drift apart, and a negative latency in the distribution is not "faster than
    /// instantaneous" but a clock mismatch.
    ///
    /// `submitted_to_filled` is computed from live fills only: a paper one is simulated
    /// against the book within the same task, its latency is identically zero, and mixing
    /// it in would understate the median by shadow's share.
    pub async fn latency(&self, since: Option<DateTime<Utc>>) -> anyhow::Result<Vec<LatencyStage>> {
        let rows = sqlx::query_as::<_, LatencyStage>(
            "WITH j AS (
               SELECT t.ts_trade, t.ts_seen, sg.ts_signal,
                      o.ts_submitted, o.ts_filled, o.mode AS order_mode
                 FROM leader_trades t
                 JOIN signals sg  ON sg.leader_trade_id = t.id
                 LEFT JOIN orders o ON o.signal_id = sg.id
                WHERE ($1::timestamptz IS NULL OR t.ts_trade >= $1)
             ), d AS (
               SELECT 'trade_to_seen' AS stage, 1 AS ord,
                      greatest(0, extract(epoch FROM ts_seen - ts_trade)) AS secs
                 FROM j WHERE ts_seen IS NOT NULL
               UNION ALL
               SELECT 'seen_to_signal', 2,
                      greatest(0, extract(epoch FROM ts_signal - ts_seen))
                 FROM j WHERE ts_signal IS NOT NULL
               UNION ALL
               SELECT 'signal_to_submitted', 3,
                      greatest(0, extract(epoch FROM ts_submitted - ts_signal))
                 FROM j WHERE ts_submitted IS NOT NULL
               UNION ALL
               SELECT 'submitted_to_filled', 4,
                      greatest(0, extract(epoch FROM ts_filled - ts_submitted))
                 FROM j WHERE ts_filled IS NOT NULL AND order_mode = 'live'
               UNION ALL
               -- The end-to-end figure: from the leader's trade to our submitted order.
               -- Computed PER TRADE rather than by adding medians per stage: the median
               -- of a sum does not equal the sum of medians, and the added-up number
               -- would resemble the truth without being it. Execution is not included —
               -- submission is where it ends.
               SELECT 'trade_to_submitted', 5,
                      greatest(0, extract(epoch FROM ts_submitted - ts_trade))
                 FROM j WHERE ts_submitted IS NOT NULL
             )
             SELECT stage, count(*) AS n,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY secs)::numeric AS median_secs,
                    percentile_cont(0.9) WITHIN GROUP (ORDER BY secs)::numeric AS p90_secs
               FROM d
              GROUP BY stage, ord
              ORDER BY ord",
        )
        .bind(since)
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    /// Copies in time order: the raw material for the threshold's counterfactual.
    ///
    /// Returned as a list rather than an aggregate, because the answer to "how much would
    /// a limit of N have refused" comes from **replaying the history through the limiter
    /// itself**, not from a formula over depths: a refused entry does not enter the queue
    /// and does not deepen the ones that follow.
    ///
    /// The key is the wallet and the mode together, as on the trading path: paper and real
    /// are different pools of capital.
    pub async fn copy_times(&self, limit: i64) -> anyhow::Result<Vec<(String, i64)>> {
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT wallet || \':\' || mode::text, ts_signal
               FROM signals WHERE verdict = \'copy\'
              ORDER BY ts_signal ASC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        Ok(rows.into_iter().map(|(k, t)| (k, t.timestamp())).collect())
    }

    /// The race between delivery circuits (invariant 43).
    ///
    /// `leader_trades.source` holds the winner, and from it nothing can be said about the
    /// loser: whether the safety-net poll pays for itself does not follow from it. Here
    /// every delivery is counted, including those the dedup refused.
    ///
    /// "The unique share" is the trades only this circuit brought. If `poll` has zero of
    /// them, it is paying in backfill duplicates for nothing; if it has some, it is
    /// catching what RTDS loses.
    ///
    /// Simultaneity counts as a win for both: two deliveries agreeing to the microsecond
    /// are a tie, and naming a winner between them would invent an order that never
    /// existed.
    pub async fn source_race(
        &self,
        since: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<SourceRace>> {
        let rows = sqlx::query_as::<_, SourceRace>(
            "WITH firsts AS (
               SELECT leader_trade_id, min(ts_seen) AS first_seen, count(*) AS n_sources
                 FROM trade_sightings
                GROUP BY leader_trade_id
             )
             SELECT s.source,
                    count(*) FILTER (WHERE s.ts_seen = f.first_seen)  AS wins,
                    count(*) FILTER (WHERE s.ts_seen > f.first_seen)  AS confirmations,
                    percentile_cont(0.5) WITHIN GROUP (
                      ORDER BY extract(epoch FROM s.ts_seen - f.first_seen)
                    )::numeric                                        AS median_lag_secs,
                    count(*) FILTER (WHERE f.n_sources = 1)           AS only_source,
                    count(*)                                          AS sightings
               FROM trade_sightings s
               JOIN firsts f ON f.leader_trade_id = s.leader_trade_id
               JOIN leader_trades t ON t.id = s.leader_trade_id
              WHERE ($1::timestamptz IS NULL OR t.ts_trade >= $1)
              GROUP BY s.source
              -- The winner first: the reader wants who is faster, not the alphabet.
              ORDER BY wins DESC, s.source",
        )
        .bind(since)
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }

    /// The distribution of the price miss, with returns per bucket.
    ///
    /// A slippage threshold cannot be tuned from a log that records only the fact of a
    /// refusal: on 05.09.2026 `signals.limit_price` was `NULL` on a skip, while an empty
    /// book was substituted with one and was indistinguishable from real slippage. Since
    /// 06.09 `best_ask` is written on every verdict (invariant 27), and this is the
    /// summary it is written for.
    ///
    /// The tail of the distribution is **truncated by the ceiling**: no observations more
    /// expensive than the active threshold exist, because they were refused. So the
    /// buckets show the skips too — they reveal how much of the sample was cut off.
    pub async fn slippage_profile(&self) -> anyhow::Result<Vec<SlippageBucket>> {
        let rows = sqlx::query_as::<_, SlippageBucket>(
            "WITH m AS (
               SELECT s.verdict,
                      (s.best_ask / t.price - 1) AS miss,
                      p.id AS pos_id
                 FROM signals s
                 JOIN leader_trades t ON t.id = s.leader_trade_id
                 LEFT JOIN positions p
                        ON p.wallet = s.wallet AND p.token_id = t.token_id
                       AND p.mode = s.mode AND p.closed_at IS NOT NULL
                WHERE t.side = 'buy' AND s.best_ask IS NOT NULL AND t.price > 0
             ), b AS (
               SELECT CASE
                        WHEN miss < 0.01 THEN 0.00
                        WHEN miss < 0.02 THEN 0.01
                        WHEN miss < 0.03 THEN 0.02
                        WHEN miss < 0.05 THEN 0.03
                        WHEN miss < 0.10 THEN 0.05
                        ELSE 0.10
                      END AS from_pct,
                      verdict, pos_id
                 FROM m
             )
             SELECT b.from_pct,
                    count(*) FILTER (WHERE b.verdict = 'copy')  AS copied,
                    count(*) FILTER (WHERE b.verdict <> 'copy') AS skipped,
                    count(DISTINCT p.id)                        AS closed,
                    COALESCE(sum(p.cost_usd), 0)                AS cost_usd,
                    COALESCE(sum(COALESCE(st.payout_usd, 0) + p.proceeds_usd
                                 - p.cost_usd - p.fees_usd), 0) AS pnl_usd
               FROM b
               LEFT JOIN positions p   ON p.id = b.pos_id
               LEFT JOIN settlements st ON st.position_id = p.id
              GROUP BY b.from_pct
              ORDER BY b.from_pct",
        )
        .fetch_all(self.pool)
        .await?;
        Ok(rows)
    }
}
