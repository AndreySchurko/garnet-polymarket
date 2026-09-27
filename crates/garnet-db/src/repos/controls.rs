//! Switches that have to survive a restart.
//!
//! The operator's manual stop lives here. It is not in process memory (a restart would
//! lift it silently) and not on the bus (`/kill` must work when NATS is unreachable).

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::postgres::PgPool;

/// The key of the manual stop. One per installation: the stop halts live entirely, not
/// an individual wallet — a wallet has `enabled` for that.
const MANUAL_STOP: &str = "manual_stop";

/// The day the loss stop latched on. Stored as a `YYYY-MM-DD` string in UTC. It has no
/// place in process memory: a restart after a bad day is the most likely event of that
/// day, and it would lift the stop silently.
const LOSS_STOP_DAY: &str = "loss_stop_day";

/// The intent to close positions in an emergency. It lives here rather than in the bot's
/// memory: between "decided" and "confirmed by phrase" the process may restart, and
/// losing the decision at that moment means demanding it be taken again under the same
/// pressure (invariant 20).
const FLATTEN_INTENT: &str = "flatten_intent";

/// An intent that passed the gate. It is written by whoever checked the phrase (the bot)
/// and executed by the trading process, which has the exchange and the book. Through the
/// database rather than the bus, for the same reason as `/kill`: an emergency close must
/// work when NATS is unreachable.
const FLATTEN_APPROVED: &str = "flatten_approved";

/// What the intent row holds.
///
/// Three states, not two. "There is no intent" and "the intent is unreadable" are
/// different outcomes: on the first the bot stays silent (nobody was waiting for a
/// phrase), on the second it must speak up, because the operator declared an intent and
/// is waiting for an answer. Merging them means showing a broken bot where one row is
/// broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredIntent {
    None,
    Unreadable(String),
    Some {
        mode: String,
        declared_at: DateTime<Utc>,
        acknowledge_forfeit: bool,
    },
}

/// Parsing the intent string. A separate function so that "a corrupted value closes the
/// gate" is verified by a test rather than by argument.
fn parse_intent(v: &str) -> Option<(String, DateTime<Utc>, bool)> {
    let mut parts = v.splitn(3, '|');
    let mode = parts.next()?.to_string();
    let ts = DateTime::parse_from_rfc3339(parts.next()?)
        .ok()?
        .with_timezone(&Utc);
    let ack = parts.next()? == "true";
    Some((mode, ts, ack))
}

/// The trading process's liveness mark.
///
/// Written by a timer and by nothing else. For the watcher it answers the question that
/// neither the age of the trades nor the age of the snapshots answers: **is the process
/// running right now**. Silence in the trades means the leaders were not trading;
/// silence in the heartbeat means there is nobody to trade.
const HEARTBEAT: &str = "heartbeat";

pub struct ControlRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> ControlRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// Whether live trading is halted by the operator's decision.
    ///
    /// A missing row means "not halted": a fresh database trades.
    pub async fn manual_stop(&self) -> anyhow::Result<bool> {
        let v: Option<String> = sqlx::query_scalar("SELECT value FROM controls WHERE key = $1")
            .bind(MANUAL_STOP)
            .fetch_optional(self.pool)
            .await?;
        Ok(v.as_deref() == Some("true"))
    }

    /// Set or lift the stop. The `actor` stays in the row: somebody is answerable for
    /// halting trading.
    pub async fn set_manual_stop(&self, stopped: bool, actor: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO controls (key, value, actor) VALUES ($1, $2, $3)
             ON CONFLICT (key) DO UPDATE SET
               value = EXCLUDED.value, actor = EXCLUDED.actor, updated_at = now()",
        )
        .bind(MANUAL_STOP)
        .bind(if stopped { "true" } else { "false" })
        .bind(actor)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Record the intent to close positions.
    ///
    /// Stored as the string `<mode>|<RFC3339 timestamp>|<consent>` rather than as JSON:
    /// three fields of fixed shape are readable by eye straight in `psql`, and an intent
    /// is precisely the row an operator will want to see with their own eyes.
    pub async fn set_flatten_intent(
        &self,
        mode: &str,
        declared_at: DateTime<Utc>,
        acknowledge_forfeit: bool,
        actor: &str,
    ) -> anyhow::Result<()> {
        let value = format!("{mode}|{}|{acknowledge_forfeit}", declared_at.to_rfc3339());
        sqlx::query(
            "INSERT INTO controls (key, value, actor) VALUES ($1, $2, $3)
             ON CONFLICT (key) DO UPDATE SET
               value = EXCLUDED.value, actor = EXCLUDED.actor, updated_at = now()",
        )
        .bind(FLATTEN_INTENT)
        .bind(value)
        .bind(actor)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The current intent.
    ///
    /// A corrupted value does not open an irreversible action — it forbids it, and it
    /// **names itself** while doing so: `Unreadable` differs from `None` precisely in
    /// that the operator has to be told about it.
    pub async fn flatten_intent(&self) -> anyhow::Result<StoredIntent> {
        let v: Option<String> = sqlx::query_scalar("SELECT value FROM controls WHERE key = $1")
            .bind(FLATTEN_INTENT)
            .fetch_optional(self.pool)
            .await?;
        let Some(v) = v else {
            return Ok(StoredIntent::None);
        };
        Ok(match parse_intent(&v) {
            Some((mode, declared_at, acknowledge_forfeit)) => StoredIntent::Some {
                mode,
                declared_at,
                acknowledge_forfeit,
            },
            None => StoredIntent::Unreadable(v),
        })
    }

    /// Clear the intent: an executed or cancelled intent must not open the gate a second
    /// time.
    pub async fn clear_flatten_intent(&self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM controls WHERE key = $1")
            .bind(FLATTEN_INTENT)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Record an approved intent: the gate is passed, execution remains.
    pub async fn approve_flatten(&self, mode: &str, actor: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO controls (key, value, actor) VALUES ($1, $2, $3)
             ON CONFLICT (key) DO UPDATE SET
               value = EXCLUDED.value, actor = EXCLUDED.actor, updated_at = now()",
        )
        .bind(FLATTEN_APPROVED)
        .bind(mode)
        .bind(actor)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The approved intent, and who approved it.
    pub async fn flatten_approved(&self) -> anyhow::Result<Option<(String, String)>> {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT value, actor FROM controls WHERE key = $1")
                .bind(FLATTEN_APPROVED)
                .fetch_optional(self.pool)
                .await?;
        Ok(row)
    }

    /// Clear the approval. Cleared **before** the sale, not after: a close that failed
    /// halfway must not start again from the beginning — a second attempt would sell what
    /// the first had already sold.
    pub async fn clear_flatten_approved(&self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM controls WHERE key = $1")
            .bind(FLATTEN_APPROVED)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Mark ourselves alive. Called from the trading process by a timer.
    pub async fn beat(&self, actor: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO controls (key, value, actor) VALUES ($1, $2, $3)
             ON CONFLICT (key) DO UPDATE SET
               value = EXCLUDED.value, actor = EXCLUDED.actor, updated_at = now()",
        )
        .bind(HEARTBEAT)
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(actor)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// When the trading process last marked itself alive.
    ///
    /// The row's `updated_at` is taken rather than the value written into it: the value is
    /// written by the process using its own clock, while `updated_at` comes from the
    /// database's. The watcher compares against the database's clock, and a process with a
    /// skewed clock must not look alive.
    pub async fn last_beat(&self) -> anyhow::Result<Option<DateTime<Utc>>> {
        let t: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT updated_at FROM controls WHERE key = $1")
                .bind(HEARTBEAT)
                .fetch_optional(self.pool)
                .await?;
        Ok(t)
    }

    /// The day the loss stop latched on, or `None`.
    ///
    /// An unreadable row is `None`, not a panic: a corrupted value must not stop the
    /// health loop, and at worst the stop will be armed again by the same loss.
    pub async fn loss_stop_day(&self) -> anyhow::Result<Option<NaiveDate>> {
        let v: Option<String> = sqlx::query_scalar("SELECT value FROM controls WHERE key = $1")
            .bind(LOSS_STOP_DAY)
            .fetch_optional(self.pool)
            .await?;
        Ok(v.and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok()))
    }

    pub async fn set_loss_stop_day(&self, day: NaiveDate, actor: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO controls (key, value, actor) VALUES ($1, $2, $3)
             ON CONFLICT (key) DO UPDATE SET
               value = EXCLUDED.value, actor = EXCLUDED.actor, updated_at = now()",
        )
        .bind(LOSS_STOP_DAY)
        .bind(day.format("%Y-%m-%d").to_string())
        .bind(actor)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}
