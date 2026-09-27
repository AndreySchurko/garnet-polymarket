//! The leader trades we saw.

use crate::{Mode, Side};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Rtds,
    Poll,
    /// Polygon logs. More reliable than the socket — it does not depend on Polymarket's
    /// infrastructure at all — but **not faster**: a log appears once the settlement
    /// transaction is in a block, and the order was matched by the CLOB before that.
    Chain,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Rtds => "rtds",
            Source::Poll => "poll",
            Source::Chain => "chain",
        }
    }
}

/// A leader trade. `market_text` and `outcome_text` are a snapshot taken at the moment of
/// the event: 7.8% of RTDS frames arrive with empty metadata, and reconstructing it after
/// the fact would give a history that is either unreadable or wrongly readable.
#[derive(Debug, Clone)]
pub struct NewLeaderTrade {
    pub wallet: String,
    pub tx_hash: String,
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub ts_trade: DateTime<Utc>,
    pub source: Source,
    pub market_text: String,
    pub outcome_text: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LeaderTrade {
    pub id: i64,
    pub wallet: String,
    pub tx_hash: String,
    pub token_id: String,
    pub side: String,
    pub price: Decimal,
    pub size: Decimal,
    pub ts_trade: DateTime<Utc>,
    pub ts_seen: DateTime<Utc>,
    pub source: String,
    pub market_text: String,
    pub outcome_text: String,
}

/// How the delivery of a trade ended.
///
/// The distinction is mandatory: only the first is acted upon, while a sighting is
/// recorded for both.
#[derive(Debug, Clone)]
pub enum Seen {
    /// The first delivery: the trade is recorded and has to be acted upon.
    First(LeaderTrade),
    /// Another circuit already brought the same on-chain trade. There is nothing to copy —
    /// but there is something to measure.
    Again(LeaderTrade),
    /// The row was deleted between the insert and the select. A sighting has nothing to
    /// attach itself to.
    Gone,
}

impl Seen {
    /// The trade to act upon. `None` — a repeat or a race.
    #[must_use]
    pub fn fresh(self) -> Option<LeaderTrade> {
        match self {
            Seen::First(t) => Some(t),
            Seen::Again(_) | Seen::Gone => None,
        }
    }
}

pub struct TradeRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> TradeRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Record a trade's delivery and say whether it is the first or a repeat.
    ///
    /// The dedup is held by a database constraint, not by application logic: Redis can be
    /// empty after a restart, while the constraint survives everything.
    ///
    /// **A sighting is recorded in both cases** (invariant 43). Until 19.09.2026 a repeat
    /// delivery returned `None` and vanished without trace — along with the only evidence
    /// of which circuit is faster. A circuit whose value cannot be measured can neither
    /// be switched off nor defended.
    pub async fn record(&self, t: &NewLeaderTrade) -> anyhow::Result<Seen> {
        let fresh = sqlx::query_as::<_, LeaderTrade>(
            "INSERT INTO leader_trades
               (wallet, tx_hash, token_id, side, price, size, ts_trade, source,
                market_text, outcome_text)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (tx_hash, wallet, token_id, side) DO NOTHING
             RETURNING *",
        )
        .bind(&t.wallet)
        .bind(&t.tx_hash)
        .bind(&t.token_id)
        .bind(t.side.as_str())
        .bind(t.price)
        .bind(t.size)
        .bind(t.ts_trade)
        .bind(t.source.as_str())
        .bind(&t.market_text)
        .bind(&t.outcome_text)
        .fetch_optional(self.pool)
        .await?;

        // A repeat: the row already exists and has to be fetched by the same key the
        // dedup fired on. Without it a sighting has nothing to attach to.
        let (row, first) = match fresh {
            Some(r) => (r, true),
            None => {
                let existing = sqlx::query_as::<_, LeaderTrade>(
                    "SELECT * FROM leader_trades
                      WHERE tx_hash = $1 AND wallet = $2 AND token_id = $3 AND side = $4",
                )
                .bind(&t.tx_hash)
                .bind(&t.wallet)
                .bind(&t.token_id)
                .bind(t.side.as_str())
                .fetch_optional(self.pool)
                .await?;
                match existing {
                    Some(r) => (r, false),
                    // The row is absent after both the insert and the select: it was
                    // deleted between the two queries. A sighting has nothing to attach
                    // to — we stay silent rather than inventing a reference.
                    None => return Ok(Seen::Gone),
                }
            }
        };

        self.sight(row.id, t.source).await?;
        Ok(if first {
            Seen::First(row)
        } else {
            Seen::Again(row)
        })
    }

    /// Note that a circuit saw this trade. A repeat by the same circuit adds no sighting:
    /// the poll brings the same trade back twenty times in a row, and counting that as
    /// twenty sightings would declare it twenty times more useful than it is.
    pub async fn sight(&self, leader_trade_id: i64, source: Source) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO trade_sightings (leader_trade_id, source)
             VALUES ($1, $2)
             ON CONFLICT (leader_trade_id, source) DO NOTHING",
        )
        .bind(leader_trade_id)
        .bind(source.as_str())
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn count_for_wallet(&self, wallet: &str) -> anyhow::Result<i64> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM leader_trades WHERE wallet = $1")
            .bind(wallet)
            .fetch_one(self.pool)
            .await?;
        Ok(n)
    }
}

/// The mode plays no part here: there is one leader trade, while there can be two
/// decisions about it — one per wallet mode.
pub const _MODE_IS_NOT_PART_OF_A_TRADE: Option<Mode> = None;
