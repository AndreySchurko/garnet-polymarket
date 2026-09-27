//! In Garnet, Redis holds **state**, not events.
//!
//! Only the parts unrelated to wallet selection were carried over from the
//! predecessor: the egress request limiter and the fast path of the dedup.
//! `active_set` and `bankroll` are not carried over — those are the
//! predecessor's scoring and bankroll concepts, which Garnet does not have.
//!
//! The dedup here is an **accelerator, not a source of truth**: the truth lives
//! in the `UNIQUE (tx_hash, wallet, token_id, side)` constraint in Postgres,
//! because Redis can be empty after a restart while the constraint survives
//! everything.

pub mod egress_bucket;

pub use egress_bucket::EgressBucket;

use redis::aio::ConnectionManager;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RedisError {
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
}

/// The dedup key of a leader trade. Matches the key of the constraint in the
/// database.
pub fn dedup_key(tx_hash: &str, wallet: &str, token_id: &str, side: &str) -> String {
    format!("dedup:{tx_hash}:{wallet}:{token_id}:{side}")
}

#[derive(Clone)]
pub struct GarnetRedis {
    conn: ConnectionManager,
}

impl GarnetRedis {
    pub async fn connect(url: &str) -> Result<Self, RedisError> {
        let client = redis::Client::open(url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self { conn })
    }

    /// Attempts to claim a trade. `true` — we are first, `false` — already seen.
    ///
    /// A miss for any reason (an empty Redis, a restart, a network failure) costs
    /// one extra trip to the database, but never a second stake: `ON CONFLICT`
    /// stands there.
    pub async fn try_claim_trade(
        &self,
        tx_hash: &str,
        wallet: &str,
        token_id: &str,
        side: &str,
        ttl_secs: u64,
    ) -> Result<bool, RedisError> {
        let mut conn = self.conn.clone();
        let key = dedup_key(tx_hash, wallet, token_id, side);
        let claimed: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg("1")
            .arg("NX")
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut conn)
            .await?;
        Ok(claimed.is_some())
    }
}
