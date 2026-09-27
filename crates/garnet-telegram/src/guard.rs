//! Who is entitled to control the bot.
//!
//! To a chat that is not ours we **reply with nothing at all** — not "access denied" but
//! silence: a reply confirms that the bot exists and controls something.
/// Whether a chat is on the allowlist.
///
/// An empty list means "nobody", not "everybody": otherwise a forgotten line of config
/// would open control over money to anyone who found the bot by searching.
#[must_use]
pub fn allowed(whitelist: &[i64], chat_id: i64) -> bool {
    whitelist.contains(&chat_id)
}

/// Who the bot answers: `TELEGRAM_OWNER_USER_IDS` overrides the config.
///
/// The operator keeps the ids next to the token — in `.env`, which is not in the
/// repository; the config remains the default. An empty variable is not a list: an empty
/// string in the environment more often means "we forgot to substitute it" than "revoke
/// everyone's access". A junk element is discarded silently — a typo in one id must not
/// deprive the operator of the controls entirely.
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
