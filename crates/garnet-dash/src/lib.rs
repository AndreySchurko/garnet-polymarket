//! The dashboard: a Telegram mini-app over the same database as the trading process.
//!
//! A reader, like the bot: it shows equity, wallets, positions and signals, and the
//! only thing it can change is a nickname. Everything that moves money lives in the
//! bot, where there is confirmation by button.

pub mod api;
pub mod initdata;

/// Who the dashboard is open to. The same rule as the bot's: the environment variable
/// overrides the config, and an empty list means "nobody".
///
/// One list for both processes, deliberately: two places deciding "who is one of us"
/// drift apart, and the drift is always towards excess access.
#[must_use]
pub fn owners(from_config: &[i64], from_env: Option<&str>) -> Vec<i64> {
    let parsed: Vec<i64> = from_env
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if parsed.is_empty() {
        from_config.to_vec()
    } else {
        parsed
    }
}
