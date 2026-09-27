//! Distributed token bucket shared with the Python services and the second bot.
//!
//! One Redis key per HOST, so `clob.polymarket.com` has a single budget across
//! garnet-core, the scanner, the portfolio manager and spinel — Cloudflare
//! counts the aggregate per IP, so anything short of one shared bucket is
//! guesswork (spec §A1).
//!
//! The key space is fixed by the Python implementation
//! (`garnet_common/redis_bucket.py`) and MUST match it byte for byte:
//! `egress:bucket:{host}`, hash fields `tokens` and `ts` (milliseconds).
//!
//! Fail-open by construction: any Redis error logs and returns, letting the
//! call through. A coordination outage degrades politeness, never trading.

use std::time::{SystemTime, UNIX_EPOCH};

use redis::aio::ConnectionManager;

/// Env var holding the limiter Redis URL. Absent → no shared limiting.
pub const EGRESS_REDIS_URL_ENV: &str = "EGRESS_REDIS_URL";

/// Env var that switches the shared limiter off without unsetting the URL.
///
/// Both `.env.example` files and Spinel's TypeScript bucket already honour it;
/// the Rust path did not, so `false` silently kept limiting.
pub const EGRESS_LIMITER_ENABLED_ENV: &str = "EGRESS_LIMITER_ENABLED";

/// How long to wait for the limiter before giving up and running unlimited.
///
/// `ConnectionManager::new` retries internally for ~8 minutes before it errors.
/// The limiter is a politeness device on a loopback Redis: if it is not there
/// within a few seconds it is not coming, and blocking startup on it would turn
/// an optional dependency into a hard one.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// `KEYS[1]` = bucket key. `ARGV`: rate, capacity, now (ms), cost.
/// Returns milliseconds to wait; 0 means the token was granted.
///
/// `EVAL`, not `EVALSHA`: the script is small, the call rate is low, and a
/// `NOSCRIPT` after a Redis restart would otherwise need its own recovery path.
const LUA_ACQUIRE: &str = r"
local key = KEYS[1]
local rate = tonumber(ARGV[1])
local capacity = tonumber(ARGV[2])
local now = tonumber(ARGV[3])
local cost = tonumber(ARGV[4])
local h = redis.call('HMGET', key, 'tokens', 'ts')
local tokens = tonumber(h[1])
local ts = tonumber(h[2])
if tokens == nil then tokens = capacity; ts = now end
local elapsed = math.max(0, now - ts) / 1000.0
tokens = math.min(capacity, tokens + elapsed * rate)
local wait = 0
if tokens >= cost then
  tokens = tokens - cost
else
  wait = math.ceil((cost - tokens) / rate * 1000.0)
end
redis.call('HSET', key, 'tokens', tokens, 'ts', now)
redis.call('PEXPIRE', key, math.ceil(capacity / rate * 1000.0) + 1000)
return wait
";

/// Same key space and refill as [`LUA_ACQUIRE`], but the caller never waits:
/// the cost is taken even when the bucket is empty, driving `tokens` negative
/// so the *next* caller pays the delay instead.
///
/// The debt is floored at `-capacity` — one bucket-length. Without a floor an
/// outage that blocks every request for minutes would leave a debt no rate can
/// work off, and the first call after recovery would sit out the whole backlog.
const LUA_CHARGE: &str = r"
local key = KEYS[1]
local rate = tonumber(ARGV[1])
local capacity = tonumber(ARGV[2])
local now = tonumber(ARGV[3])
local cost = tonumber(ARGV[4])
local h = redis.call('HMGET', key, 'tokens', 'ts')
local tokens = tonumber(h[1])
local ts = tonumber(h[2])
if tokens == nil then tokens = capacity; ts = now end
local elapsed = math.max(0, now - ts) / 1000.0
tokens = math.min(capacity, tokens + elapsed * rate)
tokens = math.max(-capacity, tokens - cost)
redis.call('HSET', key, 'tokens', tokens, 'ts', now)
redis.call('PEXPIRE', key, math.ceil(capacity / rate * 1000.0) + 1000)
return 0
";

/// Redis key for a host's bucket. Must equal the key the Python
/// `make_host_bucket` builds, or the two stacks silently stop coordinating.
#[must_use]
pub fn bucket_key(host: &str) -> String {
    format!("egress:bucket:{host}")
}

/// Whether [`EGRESS_LIMITER_ENABLED_ENV`] leaves the limiter on.
///
/// Only the exact string `false` disables it, matching Spinel's TypeScript
/// bucket — a third reading of the same variable is worse than a strict one.
fn limiter_enabled(raw: Option<&str>) -> bool {
    raw.is_none_or(|v| !v.trim().eq_ignore_ascii_case("false"))
}

/// A shared token bucket for one host.
pub struct EgressBucket {
    conn: ConnectionManager,
    key: String,
    rate: f64,
    capacity: f64,
}

impl std::fmt::Debug for EgressBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressBucket")
            .field("key", &self.key)
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

impl EgressBucket {
    /// Connect using [`EGRESS_REDIS_URL_ENV`].
    ///
    /// Returns `None` when the variable is unset or the limiter is unreachable
    /// at startup — the caller then simply does not rate-limit, the same
    /// fail-open posture as a mid-flight Redis outage.
    pub async fn from_env(host: &str, rate: f64) -> Option<Self> {
        if !limiter_enabled(std::env::var(EGRESS_LIMITER_ENABLED_ENV).ok().as_deref()) {
            tracing::warn!(
                "egress bucket: {EGRESS_LIMITER_ENABLED_ENV}=false, cross-process coordination off"
            );
            return None;
        }
        let url = std::env::var(EGRESS_REDIS_URL_ENV).ok()?;
        Self::connect(&url, host, rate).await
    }

    /// Connect to an explicit URL. `None` on any connection failure.
    pub async fn connect(url: &str, host: &str, rate: f64) -> Option<Self> {
        if rate <= 0.0 {
            tracing::warn!(rate, host, "egress bucket: non-positive rate, disabled");
            return None;
        }
        let client = match redis::Client::open(url) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "egress bucket: bad URL, limiter disabled");
                return None;
            }
        };
        match tokio::time::timeout(CONNECT_TIMEOUT, ConnectionManager::new(client)).await {
            Err(_) => {
                tracing::warn!(
                    timeout_secs = CONNECT_TIMEOUT.as_secs(),
                    "egress bucket: limiter did not answer in time, disabled"
                );
                None
            }
            Ok(Ok(conn)) => {
                tracing::info!(host, rate, "egress bucket: sharing a per-host budget");
                Some(Self {
                    conn,
                    key: bucket_key(host),
                    rate,
                    // Burst equal to one second of budget, floor 1: a bucket
                    // that cannot hold a single token would block forever.
                    capacity: rate.max(1.0),
                })
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "egress bucket: unreachable, limiter disabled");
                None
            }
        }
    }

    /// Block until a token is available.
    ///
    /// Never returns an error: on any Redis failure it logs and lets the caller
    /// through. Losing coordination is worse than nothing but far better than
    /// halting the bot on a cache outage.
    pub async fn acquire(&self) {
        let mut conn = self.conn.clone();
        loop {
            let res: Result<i64, redis::RedisError> = self.eval(&mut conn, LUA_ACQUIRE).await;
            match res {
                Ok(wait_ms) if wait_ms > 0 => {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        u64::try_from(wait_ms).unwrap_or(1000),
                    ))
                    .await;
                }
                Ok(_) => return,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        key = %self.key,
                        "egress bucket unavailable; failing open"
                    );
                    return;
                }
            }
        }
    }

    /// Spend a token **without waiting for one**.
    ///
    /// For callers whose request cannot be delayed but whose traffic still has
    /// to come out of the shared budget — the CLOB heartbeat above all: its
    /// cadence is an exchange contract (a beat missing for 10 s cancels every
    /// resting order), so making it queue behind a saturated bucket would turn
    /// a politeness device into an order-cancelling one. Charging instead makes
    /// the *other* callers absorb the heartbeat's share.
    ///
    /// One round trip, no retry, no sleep. Any Redis failure logs and returns,
    /// same fail-open posture as [`acquire`](Self::acquire).
    pub async fn charge(&self) {
        let mut conn = self.conn.clone();
        let res: Result<i64, redis::RedisError> = self.eval(&mut conn, LUA_CHARGE).await;
        if let Err(e) = res {
            tracing::warn!(
                error = %e,
                key = %self.key,
                "egress bucket unavailable; charge dropped"
            );
        }
    }

    /// Run one bucket script against this bucket's key with a cost of one.
    async fn eval(
        &self,
        conn: &mut ConnectionManager,
        script: &str,
    ) -> Result<i64, redis::RedisError> {
        let now_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis()),
        )
        .unwrap_or(0);
        redis::cmd("EVAL")
            .arg(script)
            .arg(1)
            .arg(&self.key)
            .arg(self.rate)
            .arg(self.capacity)
            .arg(now_ms)
            .arg(1)
            .query_async(conn)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{bucket_key, EgressBucket};

    #[test]
    fn only_the_literal_false_disables_the_limiter() {
        assert!(super::limiter_enabled(None));
        assert!(super::limiter_enabled(Some("true")));
        assert!(super::limiter_enabled(Some("")));
        // "0" is not "false": the TypeScript bucket reads it as enabled, and a
        // limiter that is on in one process and off in another is worse than
        // either.
        assert!(super::limiter_enabled(Some("0")));
        assert!(!super::limiter_enabled(Some("false")));
        assert!(!super::limiter_enabled(Some(" FALSE ")));
    }

    #[test]
    fn key_matches_the_python_key_space() {
        assert_eq!(
            bucket_key("clob.polymarket.com"),
            "egress:bucket:clob.polymarket.com"
        );
    }

    #[tokio::test]
    async fn unreachable_redis_disables_the_limiter_without_hanging_startup() {
        // Port 1 is never a Redis. ConnectionManager retries internally for
        // ~8 minutes, so this asserts the timeout, not just the None: without
        // it, constructing a CLOB client would block the whole boot.
        let started = std::time::Instant::now();
        let b = EgressBucket::connect("redis://127.0.0.1:1/0", "clob.polymarket.com", 2.0).await;
        assert!(b.is_none());
        assert!(
            started.elapsed() < super::CONNECT_TIMEOUT * 3,
            "took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn non_positive_rate_is_refused() {
        let b = EgressBucket::connect("redis://127.0.0.1:6382/0", "x", 0.0).await;
        assert!(b.is_none());
    }

    #[tokio::test]
    async fn a_malformed_url_is_refused() {
        let b = EgressBucket::connect("not-a-url", "clob.polymarket.com", 2.0).await;
        assert!(b.is_none());
    }

    #[tokio::test]
    #[ignore = "needs EGRESS_REDIS_URL against docker Redis"]
    async fn a_charge_is_paid_by_the_next_acquirer() {
        // The heartbeat's whole point: it never waits, but the tokens it spends
        // are gone, so whoever comes next queues for them.
        let url = std::env::var("EGRESS_REDIS_URL").expect("EGRESS_REDIS_URL");
        let host = format!("charge-test-{}.invalid", std::process::id());
        // 1 token/s, capacity 1: one charge empties the bucket, the next
        // acquire has to wait a full second for the refill.
        let b = EgressBucket::connect(&url, &host, 1.0)
            .await
            .expect("limiter reachable");

        b.acquire().await; // drain the initial capacity
        let charged = std::time::Instant::now();
        b.charge().await;
        assert!(
            charged.elapsed() < std::time::Duration::from_millis(500),
            "charge must not wait, took {:?}",
            charged.elapsed()
        );

        let waited = std::time::Instant::now();
        b.acquire().await;
        assert!(
            waited.elapsed() >= std::time::Duration::from_millis(900),
            "the charge went unpaid: acquire returned after {:?}",
            waited.elapsed()
        );
    }

    #[tokio::test]
    #[ignore = "needs EGRESS_REDIS_URL against docker Redis"]
    async fn debt_is_bounded_by_one_bucket_length() {
        // An outage that charges without ever acquiring must not build a debt
        // the next caller has to sit out in full.
        let url = std::env::var("EGRESS_REDIS_URL").expect("EGRESS_REDIS_URL");
        let host = format!("debt-test-{}.invalid", std::process::id());
        let b = EgressBucket::connect(&url, &host, 2.0)
            .await
            .expect("limiter reachable");

        for _ in 0..50 {
            b.charge().await;
        }
        // Floor is -capacity (= rate = 2 tokens), so the wait is at most
        // (2 + 1) / 2 = 1.5 s, not the 25 s the raw debt would imply.
        let waited = std::time::Instant::now();
        b.acquire().await;
        assert!(
            waited.elapsed() < std::time::Duration::from_secs(3),
            "debt is unbounded: acquire took {:?}",
            waited.elapsed()
        );
    }
}
