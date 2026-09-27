//! The daily summary.
//!
//! One message a day, at the appointed hour UTC. A missed hour is carried over to tomorrow
//! rather than sent immediately: otherwise every restart of the process after that hour
//! would add another summary.

use crate::api::Api;
use garnet_db::{Db, Mode};
use rust_decimal::Decimal;

/// Parse `HH:MM`. `None` — the summary is disabled, or the string is unparseable.
///
/// An unparseable string **disables** the summary rather than guessing the hour: "25:00"
/// with a silent shift would mean a message arriving when it is not expected.
#[must_use]
pub fn parse_at(raw: &str) -> Option<(u32, u32)> {
    let (h, m) = raw.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    (h < 24 && m < 60).then_some((h, m))
}

/// The next moment of sending after `now`.
#[must_use]
pub fn next_run(
    now: chrono::DateTime<chrono::Utc>,
    hour: u32,
    minute: u32,
) -> chrono::DateTime<chrono::Utc> {
    let today = now
        .date_naive()
        .and_hms_opt(hour, minute, 0)
        .expect("the hour and minute were validated by the parser")
        .and_utc();
    if today > now {
        today
    } else {
        today + chrono::Duration::days(1)
    }
}

/// The text of the summary: what happened over the day and where we stand now.
///
/// # Errors
///
/// A database failure.
pub async fn daily_text(db: &Db) -> anyhow::Result<String> {
    let since = chrono::Utc::now() - chrono::Duration::hours(24);
    let pnl = db.reports().realised_pnl(Some(since)).await?;
    let open = db.reports().open_positions().await?;
    let live_exists = db
        .wallets()
        .list()
        .await?
        .iter()
        .any(|w| w.mode == Mode::Live);

    let mut out = String::from("📊 Daily summary\n\n");

    // The modes are not added together: a paper result next to a real one is the only
    // comparison the paper one exists for.
    for mode in [Mode::Live, Mode::Shadow] {
        out.push_str(crate::fmt::mode_title(mode));
        out.push('\n');
        match pnl.iter().find(|r| r.mode == mode) {
            Some(r) => {
                out.push_str(&format!(
                    "   Closed {} · won {}\n   Result {}",
                    r.closed,
                    r.won,
                    crate::fmt::signed_money(r.pnl_usd)
                ));
                if let Some(pct) = crate::fmt::roi(r.pnl_usd, r.cost_usd) {
                    out.push_str(&format!(" ({pct})"));
                }
                out.push('\n');
            }
            // A missing block reads as a breakage, so an empty mode says in words why it
            // is empty.
            None if mode == Mode::Live && !live_exists => {
                out.push_str("   There are no live wallets — no real money is being spent.\n");
            }
            None => out.push_str("   Nothing closed.\n"),
        }

        let held: Vec<_> = open.iter().filter(|p| p.mode == mode).collect();
        if !held.is_empty() {
            let cost: Decimal = held.iter().map(|p| p.cost_usd).sum();
            out.push_str(&format!(
                "   Holding {} worth {}\n",
                crate::fmt::plural(held.len(), "position", "positions"),
                crate::fmt::money(cost),
            ));
        }
        if let Some(s_) = db.equity().latest(mode).await? {
            if mode == Mode::Shadow || live_exists {
                out.push_str(&format!("   Account {}\n", crate::fmt::money(s_.total_usd)));
            }
        }
        out.push('\n');
    }

    if db.controls().manual_stop().await? {
        out.push_str("⚠️ Live trading is halted by the operator. Lift it with /resume\n");
    }
    Ok(out)
}

/// Send the summary once a day for as long as the process lives.
///
/// A send failure does not interrupt the loop: a missed summary costs one message, while a
/// task that has left the loop costs every later one.
pub async fn run(api: &Api, db: &Db, chats: &[i64], hour: u32, minute: u32) {
    loop {
        let at = next_run(chrono::Utc::now(), hour, minute);
        let wait = (at - chrono::Utc::now()).to_std().unwrap_or_default();
        tokio::time::sleep(wait).await;

        match daily_text(db).await {
            Ok(text) => {
                for chat in chats {
                    if let Err(e) = api.send_message(*chat, &text, &[]).await {
                        eprintln!("the summary was not sent: {e}");
                    }
                }
            }
            Err(e) => eprintln!("the summary was not assembled: {e}"),
        }
    }
}
