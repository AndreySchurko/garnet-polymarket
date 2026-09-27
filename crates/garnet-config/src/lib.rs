//! Process configuration.
//!
//! There is not a single selection knob here: the scoring, screening, bucket and
//! bench thresholds that the predecessor's config was stuffed with do not exist
//! in Garnet. Everything concerning a wallet — mode, stake, slippage — lives in
//! the database and is changed by the operator on the fly, not at restart.

use rust_decimal::Decimal;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub database_url: String,
    #[serde(default)]
    pub shadow: Shadow,
    #[serde(default)]
    pub api: Api,
    #[serde(default)]
    pub feed: Feed,
    #[serde(default)]
    pub settlement: Settlement,
    #[serde(default)]
    pub reconcile: Reconcile,
    #[serde(default)]
    pub flatten: Flatten,
    #[serde(default)]
    pub equity: Equity,
    #[serde(default)]
    pub telegram: Telegram,
    #[serde(default)]
    pub dashboard: Dashboard,
    #[serde(default)]
    pub copy: CopyRules,
    #[serde(default)]
    pub risk: Risk,
}

/// The limits beyond which the bot stops live trading by itself.
#[derive(Debug, Clone, Deserialize)]
pub struct Risk {
    /// Acceptable realised live loss per UTC day, in dollars.
    /// **Zero disables the stop** — a deliberate default: a wrong limit is more
    /// dangerous than an absent one, because it looks like protection.
    ///
    /// Computed from closed positions, without revaluing open ones: revaluation
    /// depends on the mid of the book, which some positions do not have at all,
    /// and a stop that fires because a price is missing is the worst kind of
    /// false alarm.
    #[serde(default)]
    pub daily_loss_limit_usd: Decimal,
    /// Ceiling on open exposure per event, in dollars, separately per mode.
    /// **Zero disables it** — a fresh installation that refuses its very first
    /// signal is the most expensive failure in this project (the predecessor
    /// rejected 35 out of 35).
    ///
    /// Set against concentration, not for returns: as a way to raise ROI, a
    /// ceiling only works insofar as it accidentally trims slices of one order —
    /// and that is what the slice window does (invariant 29), deliberately.
    #[serde(default)]
    pub per_market_cap_usd: Decimal,
    /// The window of the entry rate limit, in seconds. Measured **by our
    /// clock**: what is limited here is the speed at which we spend capital, not
    /// an interpretation of the leader's behaviour (invariant 44).
    #[serde(default = "default_fire_window")]
    pub fire_window_secs: i64,
    /// How many entries per wallet are allowed within the window. **Zero disables
    /// the refusal** — and that is the default: a threshold comes from a
    /// measurement, not from a guess (invariant 33), and no measurement on
    /// Garnet's own data exists yet. The counting happens regardless, and the
    /// queue depth is visible in `/firerate` long before the refusal is switched
    /// on.
    #[serde(default)]
    pub fire_limit: usize,
}

fn default_fire_window() -> i64 {
    30
}

impl Default for Risk {
    fn default() -> Self {
        Self {
            daily_loss_limit_usd: Decimal::ZERO,
            per_market_cap_usd: Decimal::ZERO,
            fire_window_secs: default_fire_window(),
            fire_limit: 0,
        }
    }
}

/// Copying rules. There are no selection knobs here and there will not be: this
/// is the shape of an order, not the choice of whom to follow.
#[derive(Debug, Clone, Deserialize)]
pub struct CopyRules {
    /// A gap in the leader's feed after which their next buy in the same market
    /// counts as a new decision rather than a slice of the previous order.
    ///
    /// Zero disables the collapse — every frame becomes an order again.
    #[serde(default = "default_slice_window")]
    pub slice_window_secs: i64,
    /// After how many hours an open position is sold into the book without
    /// waiting for resolution. **Zero disables it.**
    ///
    /// This is a rule about capital, not about returns, and confusing the two is
    /// expensive: on a prediction market the position will reach zero or one
    /// anyway, and dumping into a thin book is usually worse than waiting. Worth
    /// enabling when money is tied up, not when you would like to lose less.
    #[serde(default)]
    pub max_hold_hours: i64,
}

fn default_slice_window() -> i64 {
    300
}

impl Default for CopyRules {
    fn default() -> Self {
        Self {
            slice_window_secs: default_slice_window(),
            max_hold_hours: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Shadow {
    /// Shared starting virtual capital. The ledger is allowed to go negative.
    pub initial_capital_usd: Decimal,
}

impl Default for Shadow {
    fn default() -> Self {
        Self {
            initial_capital_usd: Decimal::from(1000),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Api {
    pub clob_host: String,
    pub gamma_host: String,
    pub rtds_url: String,
    /// The data-api host: `/activity` lives there, the safety-net detection
    /// circuit.
    #[serde(default = "default_data_host")]
    pub data_host: String,
    /// The event bus. Its unavailability costs events, but not trading.
    #[serde(default = "default_nats_url")]
    pub nats_url: String,
}

fn default_nats_url() -> String {
    "nats://127.0.0.1:4223".to_string()
}

fn default_data_host() -> String {
    "https://data-api.polymarket.com".to_string()
}

impl Default for Api {
    fn default() -> Self {
        Self {
            clob_host: "https://clob.polymarket.com".into(),
            gamma_host: "https://gamma-api.polymarket.com".into(),
            rtds_url: "wss://ws-live-data.polymarket.com".into(),
            data_host: default_data_host(),
            nats_url: default_nats_url(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Feed {
    /// Period of the safety-net `/activity` poll, per wallet.
    pub poll_interval_secs: u64,
    /// The silence after which the feed counts as stalled.
    pub stall_threshold_secs: i64,
    /// The silence on the control topic after which we assume that we have gone
    /// deaf.
    pub control_stall_secs: i64,
    /// The Polygon node's WebSocket, for the third detection circuit.
    ///
    /// **An empty string disables the circuit entirely** — and that is the
    /// default: it needs a node of your own, and an installation that demands
    /// one on the first run does not start at all.
    #[serde(default)]
    pub polygon_ws_url: String,
    /// HTTP of the same node: block timestamps come from it. The log does not
    /// carry them, and `ts_trade` is the leader's time, by which the slice window
    /// closes (invariant 29); substituting our own clock there would turn the
    /// slice window into a measurement of our own latency.
    ///
    /// An empty string while `polygon_ws_url` is set is a configuration error,
    /// not a "we'll manage somehow": the circuit without timestamps would record
    /// trades with someone else's time.
    #[serde(default)]
    pub polygon_rpc_url: String,
}

impl Default for Feed {
    fn default() -> Self {
        Self {
            poll_interval_secs: 3,
            stall_threshold_secs: 900,
            control_stall_secs: 120,
            polygon_ws_url: String::new(),
            polygon_rpc_url: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settlement {
    /// Positions whose market is already past game_start_time or end_date.
    pub hot_interval_secs: u64,
    pub cold_interval_secs: u64,
    pub batch: i64,
}

impl Default for Settlement {
    fn default() -> Self {
        Self {
            hot_interval_secs: 600,
            cold_interval_secs: 3600,
            batch: 200,
        }
    }
}

/// Emergency closing of positions.
#[derive(Debug, Clone, Deserialize)]
pub struct Flatten {
    /// The installation's name, forming the second half of the confirmation
    /// phrase.
    ///
    /// Its purpose is that a phrase copied from documentation or from someone
    /// else's chat log **will not fire on this host**. An identical name across
    /// every installation strips the phrase of half its job.
    pub name: String,
}

impl Default for Flatten {
    fn default() -> Self {
        Self {
            name: "garnet".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Reconcile {
    pub interval_secs: u64,
    /// How many seconds an order with status `unknown` counts as "in flight".
    ///
    /// The outcome of an IOC order is known within seconds, but nobody ever
    /// moves an `unknown` row to another status: it is created for exactly one
    /// purpose, to be seen by the reconciler. Without an expiry, such a row would
    /// explain a divergence **forever** — meaning a genuine loss of tokens would
    /// stay "explained by an order in flight" permanently. Zero disables the
    /// explanation entirely: the reconciler returns to its earlier behaviour,
    /// where market resolution is the only excuse.
    #[serde(default = "default_in_flight_window")]
    pub in_flight_window_secs: i64,
}

fn default_in_flight_window() -> i64 {
    900
}

impl Default for Reconcile {
    fn default() -> Self {
        Self {
            interval_secs: 600,
            in_flight_window_secs: default_in_flight_window(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Equity {
    pub snapshot_interval_secs: u64,
}

impl Default for Equity {
    fn default() -> Self {
        Self {
            snapshot_interval_secs: 300,
        }
    }
}

/// The operator's control bot. The token is not stored here: it arrives from
/// `TELEGRAM_BOT_TOKEN`, while the config lives in the repository.
#[derive(Debug, Clone, Deserialize)]
pub struct Telegram {
    /// Who the bot answers. **An empty list means "nobody"**: the opposite
    /// default would hand control over money to anyone who found the bot by
    /// searching.
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,
    /// How long Telegram holds `getUpdates` open while there are no updates.
    pub poll_timeout_secs: u64,
    pub api_host: String,
    /// Hour of the daily summary, `HH:MM` UTC. An empty string: no summary.
    #[serde(default)]
    pub daily_summary_utc: String,
    /// Which fills go to the chat: `live` (the default), `all` or `none`.
    ///
    /// Twelve wallets in shadow produce dozens of fills a minute, and a push for
    /// each one drowns the very alerts the notifications exist for.
    #[serde(default = "default_push_fills")]
    pub push_fills: String,
}

fn default_push_fills() -> String {
    "live".into()
}

impl Default for Telegram {
    fn default() -> Self {
        Self {
            allowed_chat_ids: Vec::new(),
            poll_timeout_secs: 30,
            api_host: "https://api.telegram.org".into(),
            daily_summary_utc: "21:00".into(),
            push_fills: default_push_fills(),
        }
    }
}

/// The dashboard mini-app.
#[derive(Debug, Clone, Deserialize)]
pub struct Dashboard {
    /// The listen address. **Loopback only**: the outside world reaches it
    /// through Caddy, which also terminates TLS. A public bind would mean a
    /// dashboard on the internet without TLS.
    pub bind: String,
    /// How long a signed `initData` string stays valid. It does not expire by
    /// itself, and an intercepted one would work forever.
    pub init_data_max_age_secs: i64,
    /// The address at which the dashboard is reachable from outside. An empty
    /// string: the `/app` command reports that it is not configured.
    #[serde(default)]
    pub public_url: String,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:3001".into(),
            init_data_max_age_secs: 86_400,
            public_url: String::new(),
        }
    }
}

impl Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
    }

    pub fn load(path: &str) -> anyhow::Result<Self> {
        Self::from_toml(&std::fs::read_to_string(path)?)
    }
}
