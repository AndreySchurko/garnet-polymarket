//! Health is measured by flow, not by whether the process is alive.
//!
//! In the predecessor both guards reported OK while the bot was blind for 6.5
//! hours: they checked that the process was running and the socket was connected.
//! Here health means "data is arriving": the last leader trade was seen recently
//! **and** the control topic is alive.

use chrono::Duration;

#[derive(Debug, Clone)]
pub struct HealthInput {
    /// The process is alive. Means nothing by itself and is not part of the
    /// verdict.
    pub process_up: bool,
    /// The socket is connected. Also means nothing by itself.
    pub socket_connected: bool,
    /// How long since the last leader trade was seen.
    pub last_trade_age: Duration,
    /// How long since the last frame of the control topic (`crypto_prices`). It
    /// separates "the market is quiet" from "we have gone deaf".
    pub control_topic_age: Duration,
    /// The silence threshold for trades.
    pub threshold: Duration,
    /// The silence threshold for the control topic.
    pub control_threshold: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Ok,
    /// The stream of trades has dried up, but the control topic is alive —
    /// probably the leaders are quiet, or we have lost the activity subscription.
    NoTrades {
        age_secs: i64,
    },
    /// The control topic is silent too — we have gone deaf.
    FeedDead {
        age_secs: i64,
    },
}

impl Health {
    pub fn evaluate(i: &HealthInput) -> Self {
        if i.control_topic_age > i.control_threshold {
            return Health::FeedDead {
                age_secs: i.control_topic_age.num_seconds(),
            };
        }
        if i.last_trade_age > i.threshold {
            return Health::NoTrades {
                age_secs: i.last_trade_age.num_seconds(),
            };
        }
        Health::Ok
    }

    pub fn ok(&self) -> bool {
        matches!(self, Health::Ok)
    }

    pub fn reason(&self) -> String {
        match self {
            Health::Ok => "ok".into(),
            Health::NoTrades { age_secs } => format!("no leader trades for {age_secs} s"),
            Health::FeedDead { age_secs } => {
                format!("feed is dead, control topic silent for {age_secs} s")
            }
        }
    }
}
