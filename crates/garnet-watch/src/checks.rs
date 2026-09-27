//! The checks themselves. Pure: facts already gathered in, a verdict out.
//!
//! The separation is not cosmetic. A check that goes to the database itself can only
//! be tested through the database, whereas what has to be tested here is the
//! **boundaries**: how many minutes of silence it is still too early to shout about,
//! and how "could not be read" differs from "agreed".

use chrono::{DateTime, Duration, Utc};

/// The outcome of one check.
///
/// Three, not two. "Could not be checked" is not "agreed" (invariant 47): a watcher
/// that stays silent about what it failed to check lies in the same way as a
/// reconciler substituting zero for an unread balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Agreed(String),
    Diverged(String),
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub verdict: Verdict,
}

impl Check {
    #[must_use]
    pub fn agreed(name: &'static str, what: impl Into<String>) -> Self {
        Self {
            name,
            verdict: Verdict::Agreed(what.into()),
        }
    }
    #[must_use]
    pub fn diverged(name: &'static str, what: impl Into<String>) -> Self {
        Self {
            name,
            verdict: Verdict::Diverged(what.into()),
        }
    }
    #[must_use]
    pub fn unknown(name: &'static str, what: impl Into<String>) -> Self {
        Self {
            name,
            verdict: Verdict::Unknown(what.into()),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// The exit code: `0` agreed, `1` a divergence, `2` could not be checked.
    ///
    /// A divergence outranks an unknown: both are printed, but a divergence is
    /// something already known to be wrong, and the signal about it has to be louder.
    /// An unknown is raised only when there are no divergences at all.
    ///
    /// "Could not" is not "agreed" — that is the whole point of the third code.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self
            .checks
            .iter()
            .any(|c| matches!(c.verdict, Verdict::Diverged(_)))
        {
            1
        } else if self
            .checks
            .iter()
            .any(|c| matches!(c.verdict, Verdict::Unknown(_)))
        {
            2
        } else {
            0
        }
    }
}

/// Whether the trading process is running.
///
/// The only check that answers this question. The age of the trades does not answer
/// it: silence in the trades means the **leaders** were not trading.
#[must_use]
pub fn heartbeat(last: Option<DateTime<Utc>>, now: DateTime<Utc>, allowed: Duration) -> Check {
    const NAME: &str = "process";
    match last {
        // There is no mark at all: the process may never have been started. That is
        // neither "agreed" nor "diverged" — it is "we do not know whether it ran".
        None => Check::unknown(NAME, "no liveness mark: the process may never have started"),
        Some(t) if now - t <= allowed => {
            Check::agreed(NAME, format!("last seen {} ago", ago(now - t)))
        }
        Some(t) => Check::diverged(
            NAME,
            format!(
                "silent for {} against a limit of {}",
                ago(now - t),
                ago(allowed)
            ),
        ),
    }
}

/// Whether equity snapshots are being written.
///
/// A snapshot runs on a timer and does not depend on whether the leaders are trading:
/// its absence means the loop inside the trading process has stalled, even if the
/// process itself is alive.
#[must_use]
pub fn equity(last: Option<DateTime<Utc>>, now: DateTime<Utc>, allowed: Duration) -> Check {
    const NAME: &str = "equity snapshots";
    match last {
        None => Check::unknown(NAME, "there are no snapshots at all"),
        Some(t) if now - t <= allowed => {
            Check::agreed(NAME, format!("the latest one {} ago", ago(now - t)))
        }
        Some(t) => Check::diverged(
            NAME,
            format!(
                "the latest one is {} old against a limit of {}",
                ago(now - t),
                ago(allowed)
            ),
        ),
    }
}

/// Whether the stream of trades is flowing.
///
/// **Silence by itself is not a divergence**: the leaders do not trade around the
/// clock, and a watcher that shouts at a quiet night is the first to stop being read.
/// So the check only raises an alarm when there are enabled wallets, and its threshold
/// is generous.
#[must_use]
pub fn feed(
    last: Option<DateTime<Utc>>,
    enabled_wallets: usize,
    now: DateTime<Utc>,
    allowed: Duration,
) -> Check {
    const NAME: &str = "trade flow";
    if enabled_wallets == 0 {
        return Check::agreed(NAME, "no enabled wallets — silence is expected");
    }
    match last {
        None => Check::unknown(
            NAME,
            format!("no trades at all with {enabled_wallets} enabled wallets"),
        ),
        Some(t) if now - t <= allowed => {
            Check::agreed(NAME, format!("the latest one {} ago", ago(now - t)))
        }
        Some(t) => Check::diverged(
            NAME,
            format!(
                "the latest one is {} old against a limit of {}",
                ago(now - t),
                ago(allowed)
            ),
        ),
    }
}

/// Orders whose outcome is unknown and will not become known by itself.
///
/// The `unknown` status is created when the exchange accepted an order and we never
/// received a confirmation. Nobody ever moves such a row to another status — it exists
/// precisely to be seen. While it is fresh the reconciler explains it (invariant 48);
/// once it has aged there is no explanation left, and the money may have been spent.
#[must_use]
pub fn stale_unknown_orders(count: i64) -> Check {
    const NAME: &str = "orders without an outcome";
    if count == 0 {
        Check::agreed(NAME, "none")
    } else {
        Check::diverged(
            NAME,
            format!(
                "{count}: accepted by the exchange, outcome unknown, nothing left to explain them"
            ),
        )
    }
}

/// Leader merges we never exited on.
///
/// The leader left the position at $1 on both legs while we stayed in it with a
/// `leader_observed_size` that no longer means anything (invariant 40).
#[must_use]
pub fn unhandled_merges(count: i64) -> Check {
    const NAME: &str = "leader merges";
    if count == 0 {
        Check::agreed(NAME, "all handled")
    } else {
        Check::diverged(NAME, format!("{count} with no exit of ours"))
    }
}

/// Positions that have sat around longer than permitted.
///
/// Zero hours means the time-based exit loop is disabled entirely — and then stale
/// positions do not exist by definition, rather than because there are none.
#[must_use]
pub fn stale_positions(count: i64, max_hold_hours: i64) -> Check {
    const NAME: &str = "stale positions";
    if max_hold_hours <= 0 {
        return Check::agreed(NAME, "the time-based exit is disabled");
    }
    if count == 0 {
        Check::agreed(NAME, format!("none older than {max_hold_hours} h"))
    } else {
        Check::diverged(
            NAME,
            format!("{count} older than {max_hold_hours} h, and the exit loop did not close them"),
        )
    }
}

/// A duration in words. The watcher gets read half asleep.
fn ago(d: Duration) -> String {
    let s = d.num_seconds().max(0);
    if s < 90 {
        format!("{s} s")
    } else if s < 5400 {
        format!("{} min", s / 60)
    } else {
        format!("{} h", s / 3600)
    }
}
