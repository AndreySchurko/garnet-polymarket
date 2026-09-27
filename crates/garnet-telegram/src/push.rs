//! A bus event -> a message to the operator.
//!
//! Not everything is forwarded: `signal.detected` fires on **every** leader trade,
//! skipped ones included, and would turn the chat into a stream people stop reading.
//! What is addressed to the operator is a fill, a refusal, a stop and a resolution.
//!
//! A push is not a precondition of trading: an unreachable Telegram costs us
//! notifications, not trades, so no errors are raised out of here.

use crate::api::Api;
use garnet_bus::subjects;
use garnet_db::{Db, Mode};
use rust_decimal::Decimal;
use serde_json::Value;

/// The text of a push. `None` — the event is not addressed to the operator.
///
/// `nickname` and `outcome` come from outside: a trading-path event does not carry them,
/// and the wallet's name and the outcome label are the only things that make the message
/// readable.
#[must_use]
pub fn push_text(
    subject: &str,
    ev: &Value,
    nickname: Option<&str>,
    outcome: Option<&str>,
) -> Option<String> {
    match subject {
        subjects::ORDER_FILLED => Some(fill(ev, nickname, outcome)),
        subjects::POSITION_SETTLED => Some(settled(ev, nickname, outcome)),
        subjects::ALERT_ORDER_REJECTED => Some(format!(
            "🔴 The order did not go through · {}
{} · {}
Why: {}",
            mode_title(ev),
            who(ev, nickname),
            what(ev, outcome),
            ev["reason"].as_str().unwrap_or("no reason given"),
        )),
        subjects::ALERT_KILLSWITCH
        | subjects::ALERT_FEED_STALLED
        | subjects::ALERT_LOW_BALANCE
        | subjects::ALERT_RECONCILE_DIVERGENCE
        | subjects::ALERT_RECONCILE_UNREADABLE => Some(format!(
            "⚠️ {}",
            ev["text"].as_str().unwrap_or("an alert with no text")
        )),
        // An unknown alert is **shown** rather than lost. The `_ => None` branch used to
        // swallow any subject not in the list above: a new alert reached the bus, reached
        // the `alert.*` subscription — and died here silently. An alert the operator never
        // learned about is worse than a missing alert: the first creates confidence that
        // all is quiet.
        other if other.starts_with("alert.") => Some(format!(
            "⚠️ {}\n({other})",
            ev["text"].as_str().unwrap_or("an alert with no text")
        )),
        _ => None,
    }
}

fn fill(ev: &Value, nickname: Option<&str>, outcome: Option<&str>) -> String {
    let sold = ev["side"].as_str() == Some("sell");
    format!(
        "{} · {}
{} {} at {}
{} sh for {} · fee {}",
        mode_title(ev),
        who(ev, nickname),
        if sold { "Sold" } else { "Bought" },
        what(ev, outcome),
        num(ev, "avg_price", 4),
        num(ev, "size", 4),
        cash(ev, "notional"),
        fee_of(ev, "fee_usd"),
    )
}

/// A resolution.
///
/// This is exactly what the operator asked: did **our** bet win, and what do the numbers
/// mean. So the verdict is the first line and every number is labelled: "payout $14.29 ·
/// total $12.20" answered neither the first question nor the second.
///
/// The verdict is about the outcome, the result is about money, and they are not obliged
/// to agree: a winning outcome bought at 0.99 brings a loss on fees.
fn settled(ev: &Value, nickname: Option<&str>, outcome: Option<&str>) -> String {
    let won = ev["won"].as_bool().unwrap_or(false);
    let pnl = dec(ev, "pnl_usd");
    let cost = dec(ev, "cost_usd");
    let proceeds = dec(ev, "proceeds_usd");

    let mut out = format!(
        "{} · {}
{} · {}
Winning outcome: {}

Stake        {}
",
        if won { "✅ Won" } else { "❌ Lost" },
        mode_words_of(ev),
        who(ev, nickname),
        what(ev, outcome),
        ev["resolved_outcome"].as_str().unwrap_or("not named"),
        cash(ev, "cost_usd"),
    );
    if proceeds > Decimal::ZERO {
        // The leader exited before resolution, and part of the result is in the sale.
        // Without this line the payout does not add up against the stake, and the numbers
        // look like a lie.
        out.push_str(&format!(
            "Sold         {}
",
            cash(ev, "proceeds_usd")
        ));
    }
    out.push_str(&format!(
        "Payout       {}
",
        cash(ev, "payout_usd")
    ));
    out.push_str(&format!(
        "Fees         {}
",
        fee_of(ev, "fees_usd")
    ));
    out.push_str(&format!("Result       {}", crate::fmt::signed_money(pnl)));
    if let Some(pct) = crate::fmt::roi(pnl, cost) {
        out.push_str(&format!("  ({pct})"));
    }
    out
}

fn is_live(ev: &Value) -> bool {
    ev["mode"].as_str() == Some("live")
}

fn mode_of(ev: &Value) -> Mode {
    if is_live(ev) {
        Mode::Live
    } else {
        Mode::Shadow
    }
}

fn mode_title(ev: &Value) -> &'static str {
    crate::fmt::mode_title(mode_of(ev))
}

fn mode_words_of(ev: &Value) -> &'static str {
    crate::fmt::mode_words(mode_of(ev))
}

fn who(ev: &Value, nickname: Option<&str>) -> String {
    nickname
        .map(str::to_string)
        .unwrap_or_else(|| crate::fmt::short(ev["wallet"].as_str().unwrap_or("wallet")))
}

/// What was bought. The metadata is sometimes unavailable — 7.8% of RTDS frames arrive
/// with empty fields — and then a short token is better than no push at all.
fn what(ev: &Value, outcome: Option<&str>) -> String {
    outcome
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::fmt::short(ev["token_id"].as_str().unwrap_or("?")))
}

fn dec(ev: &Value, field: &str) -> Decimal {
    ev[field]
        .as_str()
        .and_then(|s| Decimal::from_str_exact(s).ok())
        .unwrap_or_default()
}

/// Money out of an event. A non-number is shown as it arrived: substituting zero would
/// mean saying "free" where we simply do not know.
fn cash(ev: &Value, field: &str) -> String {
    match ev[field].as_str() {
        Some(raw) => match Decimal::from_str_exact(raw) {
            Ok(v) => crate::fmt::money(v),
            Err(_) => raw.to_string(),
        },
        None => "?".to_string(),
    }
}

/// The fee out of an event: in its own format, because it can be less than a cent.
fn fee_of(ev: &Value, field: &str) -> String {
    match ev[field].as_str() {
        Some(raw) => match Decimal::from_str_exact(raw) {
            Ok(v) => crate::fmt::fee(v),
            Err(_) => raw.to_string(),
        },
        None => "?".to_string(),
    }
}

/// A number out of an event — for reading, not as it sits in the database.
///
/// It arrives in the event as a string from `numeric(18,6)`: "59.500000". Trailing zeros
/// are dropped and excess digits rounded. A fee can be less than a cent, so more digits
/// are asked for than for money.
fn num(ev: &Value, field: &str, places: u32) -> String {
    let raw = ev[field].as_str().unwrap_or("?");
    match Decimal::from_str_exact(raw) {
        Ok(v) => v.round_dp(places).normalize().to_string(),
        Err(_) => raw.to_string(),
    }
}

/// Whether this event should be sent to the chat.
///
/// The filter concerns **fills only**: the killswitch, a stalled feed and a resolution are
/// what the notifications exist for, and they always go out.
///
/// Measured 2026-09-04: twelve wallets in shadow produced 83 fills within minutes. A push
/// for each one is a stream people stop reading, and with it they stop noticing the real
/// alerts too. Shadow measures, and the place for its stream is the dashboard.
#[must_use]
pub fn wanted(subject: &str, ev: &Value, fills: &str) -> bool {
    if subject != subjects::ORDER_FILLED {
        return true;
    }
    match fills {
        "all" => true,
        "none" => false,
        // The default: real money only.
        _ => is_live(ev),
    }
}

/// Send one event out across the allowlist.
///
/// A send failure is printed and ends there: an unreachable Telegram costs us
/// notifications, not trades.
pub async fn deliver(api: &Api, db: &Db, chats: &[i64], subject: &str, ev: &Value, fills: &str) {
    if !wanted(subject, ev, fills) {
        return;
    }
    let (nickname, outcome) = enrich(db, ev).await;
    let Some(text) = push_text(subject, ev, nickname.as_deref(), outcome.as_deref()) else {
        return;
    };
    for chat in chats {
        if let Err(e) = api.send_message(*chat, &text, &[]).await {
            eprintln!("the push was not sent: {e}");
        }
    }
}

/// Listen to the bus and send pushes for as long as the process lives.
///
/// Three subscriptions rather than one on `>`: the chat must not receive what is not
/// addressed to it, and the filter is better placed on the NATS side.
///
/// # Errors
///
/// The subscription could not be created.
pub async fn listen(
    bus: &garnet_bus::Bus,
    api: &Api,
    db: &Db,
    chats: &[i64],
    fills: &str,
) -> anyhow::Result<()> {
    use tokio_stream::StreamExt as _;

    let mut filled = bus.subscribe(subjects::ORDER_FILLED).await?;
    let mut settled = bus.subscribe(subjects::POSITION_SETTLED).await?;
    let mut alerts = bus.subscribe("alert.*").await?;

    loop {
        let msg = tokio::select! {
            Some(m) = filled.next() => m,
            Some(m) = settled.next() => m,
            Some(m) = alerts.next() => m,
            else => return Ok(()),
        };
        let Ok(ev) = serde_json::from_slice::<Value>(&msg.payload) else {
            eprintln!("the event {} did not parse", msg.subject);
            continue;
        };
        deliver(api, db, chats, msg.subject.as_str(), &ev, fills).await;
    }
}

/// The wallet's name and the outcome's label: a trading-path event does not carry them, and
/// without them the message reads like a log line.
async fn enrich(db: &Db, ev: &Value) -> (Option<String>, Option<String>) {
    let nickname = match ev["wallet"].as_str() {
        Some(w) => db
            .wallets()
            .get(w)
            .await
            .ok()
            .flatten()
            .map(|w| w.display()),
        None => None,
    };
    // `markets` is not populated yet, but the leader's trade did bring human-readable text
    // along with the frame — we take that.
    let outcome = match ev["token_id"].as_str() {
        Some(t) => sqlx::query_scalar::<_, String>(
            "SELECT outcome_text FROM leader_trades
             WHERE token_id = $1 AND outcome_text <> '' ORDER BY id DESC LIMIT 1",
        )
        .bind(t)
        .fetch_optional(db.pool())
        .await
        .ok()
        .flatten(),
        None => None,
    };
    (nickname, outcome)
}
