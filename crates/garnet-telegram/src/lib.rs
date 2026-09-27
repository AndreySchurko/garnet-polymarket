//! The Telegram bot: the operator's controls.
//!
//! The bot is a **reader**: it changes the wallet registry and sets the stop, but it does
//! not interfere with the trading path and lives as a separate process. A hung
//! long-polling call or a ban on the bot must not land in the same process as trade
//! execution.
//!
//! Parsing an update, checking access and rendering messages are pure functions: they are
//! easy to get wrong, and no network is needed to check them.

pub mod api;
pub mod commands;
pub mod fmt;
pub mod guard;
pub mod poll;
pub mod push;
pub mod summary;

/// The mini-app's public address. Set once at startup from the config.
///
/// Not a constant: the address depends on the installation, and a domain baked into the
/// code sooner or later points at somebody else's server. Not an argument to `handle`:
/// that would drag the address through every command for the sake of one.
static DASHBOARD_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// # Errors
///
/// Never: a repeated set is silently ignored — there is one address per process.
pub fn set_dashboard_url(url: impl Into<String>) {
    let _ = DASHBOARD_URL.set(url.into());
}

/// The mini-app's address, if one is configured.
#[must_use]
pub fn dashboard_url() -> Option<&'static str> {
    DASHBOARD_URL
        .get()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
}
/// The installation's name, forming the second half of the emergency-close confirmation
/// phrase.
static FLATTEN_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// # Errors
///
/// Never: a repeated set is silently ignored — there is one name per process.
pub fn set_flatten_name(name: impl Into<String>) {
    let _ = FLATTEN_NAME.set(name.into());
}

/// The installation's name. The default matches the config's default: a bot whose name was
/// not set must demand the same phrase as the trading process, not an empty one.
#[must_use]
pub fn flatten_name() -> &'static str {
    FLATTEN_NAME.get().map_or("garnet", String::as_str)
}

pub mod update;
