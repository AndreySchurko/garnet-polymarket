//! Vocabulary and formatting: the one place where how the bot talks to a human is decided.
//!
//! The rule of this file: **internal words never appear on screen**. `live`, `shadow`,
//! `payout`, `verdict` are the code authors' terms; the reader of a message did not choose
//! them and pays for the difference between the first two in money.

use garnet_db::Mode;
use rust_decimal::Decimal;

/// The ceiling on one Bot API message is 4096 **characters**.
///
/// Telegram does not trim a long message, it **rejects** it, and the command looks broken:
/// on 05.09.2026 `/positions` over 201 positions produced 13,607 bytes and never arrived
/// once.
pub const TELEGRAM_LIMIT: usize = 4096;

/// Headroom for the "N more — /app" tail: the trim has to happen before we hit the wall.
const CLAMP_BUDGET: usize = TELEGRAM_LIMIT - 200;

/// The mode in words.
#[must_use]
pub fn mode_words(mode: Mode) -> &'static str {
    match mode {
        Mode::Live => "real money",
        Mode::Shadow => "paper",
    }
}

/// The mode as an icon and a word — a block heading.
#[must_use]
pub fn mode_title(mode: Mode) -> &'static str {
    match mode {
        Mode::Live => "💵 Real money",
        Mode::Shadow => "📄 Paper",
    }
}

#[must_use]
pub fn mode_badge(mode: Mode) -> &'static str {
    match mode {
        Mode::Live => "💵",
        Mode::Shadow => "📄",
    }
}

/// Money for reading: two decimal places and a thousands separator.
///
/// `numeric(18,6)` prints "$1.000000", and "11950.07" on a phone reads as "1195007" — the
/// eye looks for groups that are not there.
#[must_use]
pub fn money(v: Decimal) -> String {
    with_sign(v.round_dp(2))
}

/// Money with a sign: "+5" and "5" read the same and mean different things when there is a
/// "-5" next to them.
#[must_use]
pub fn signed_money(v: Decimal) -> String {
    let v = v.round_dp(2);
    if v > Decimal::ZERO {
        format!("+${}", group(v))
    } else if v < Decimal::ZERO {
        format!("−${}", group(-v))
    } else {
        "$0".to_string()
    }
}

/// Percentage points with a sign. A minus here is good news (we are ahead), and without the
/// sign the line reads the other way round.
#[must_use]
pub fn signed_pts(v: Decimal) -> String {
    let v = v.round_dp(1).normalize();
    if v > Decimal::ZERO {
        format!("+{v}")
    } else if v < Decimal::ZERO {
        format!("−{}", -v)
    } else {
        "0".to_string()
    }
}

/// A fee: two decimal places, and four when it is under a cent.
///
/// It can be smaller than a cent, and rounding to two places would turn it into "$0" — zero
/// means "free", which does not happen on this exchange. But the opposite is bad too:
/// "$14.461" on a large amount reads as a typo rather than as money. So there are exactly as
/// many decimal places as it takes not to show a zero.
#[must_use]
pub fn fee(v: Decimal) -> String {
    // The first precision at which the amount does not turn into zero. The floor is six
    // places: that is what `numeric(18,6)` stores, and there is nothing beyond it to show.
    for places in [2, 4, 6] {
        let rounded = v.round_dp(places);
        if !rounded.is_zero() || v.is_zero() {
            return with_sign(rounded);
        }
    }
    with_sign(v.round_dp(6))
}

/// "−$11 114.71", not "$−11 114.71": the minus applies to the amount as a whole, and inside
/// the currency it reads as a typo.
fn with_sign(v: Decimal) -> String {
    if v.is_sign_negative() && !v.is_zero() {
        format!("−${}", group(-v))
    } else {
        format!("${}", group(v))
    }
}

/// The return as a percentage of what was invested. `None` — nothing was invested and there
/// is nothing to divide by; an invented percentage is worse than none.
#[must_use]
pub fn roi(pnl: Decimal, cost: Decimal) -> Option<String> {
    if cost <= Decimal::ZERO {
        return None;
    }
    let pct = (pnl / cost * Decimal::ONE_HUNDRED).round_dp(1);
    Some(if pct > Decimal::ZERO {
        format!("+{}%", pct.normalize())
    } else if pct < Decimal::ZERO {
        format!("−{}%", (-pct).normalize())
    } else {
        "0%".to_string()
    })
}

/// A fraction -> percent. In the database `max_slippage_pct` is a **fraction**: printing it
/// with a percent sign without multiplying means being wrong by exactly a factor of a
/// hundred.
#[must_use]
pub fn percent(fraction: Decimal) -> String {
    format!(
        "{}%",
        (fraction * Decimal::ONE_HUNDRED).round_dp(2).normalize()
    )
}

/// Thousands separated by a space, trailing zeros dropped: "11 950", "3 234.48", "0.04".
///
/// It does not draw the sign: the caller does that — "−$5", not "$−5".
fn group(v: Decimal) -> String {
    let v = v.normalize();
    let s = v.abs().to_string();
    let (int, frac) = s.split_once('.').map_or((s.as_str(), ""), |(i, f)| (i, f));

    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push('\u{202f}'); // a narrow no-break space: it does not wrap
        }
        out.push(c);
    }
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
        // The fractional part of money has two digits: "$12.2" reads as a truncated number.
        // Whole amounts get no tail — "$25", not "$25.00".
        for _ in frac.len()..2 {
            out.push('0');
        }
    }
    out
}

/// How much time has passed, in words.
#[must_use]
pub fn ago(t: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - t).num_seconds().max(0);
    match secs {
        0..=90 => format!("{secs}s ago"),
        91..=5400 => format!("{}m ago", secs / 60),
        5401..=172_800 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// The short form of an address or a token: they are longer than a phone's line.
#[must_use]
pub fn short(s: &str) -> String {
    if s.chars().count() > 12 {
        let head: String = s.chars().take(10).collect();
        format!("{head}…")
    } else {
        s.to_string()
    }
}

/// A count with its noun: "1 position", "2 positions".
#[must_use]
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Trim to what Telegram will accept, naming the trim in words.
///
/// A silently lost tail is worse than a rejected message: there it is visible that something
/// is missing, whereas here the reader is sure they are seeing everything.
#[must_use]
pub fn clamp(text: String, tail: &str) -> String {
    if text.chars().count() <= TELEGRAM_LIMIT {
        return text;
    }
    let mut out = String::new();
    for line in text.lines() {
        if out.chars().count() + line.chars().count() + 1 > CLAMP_BUDGET {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(tail);
    out
}
