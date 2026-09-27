//! Executing commands against the wallet registry and the stop.
//!
//! The rule: three actions wait for a button press — moving a wallet **to live**,
//! changing the stake, and `/kill`. Everything else executes immediately.
//! People stop reading a confirmation that appears for everything, and it stops
//! protecting precisely where it is needed.
//!
//! Returning to shadow, disabling a wallet and `/resume` ask no questions: a confirmation
//! protects against spending money, not against stopping.

use crate::fmt;
use crate::update::{parse_command, Command, Incoming, Period};
use garnet_db::{Db, Mode, SignalRow, Wallet};
use rust_decimal::Decimal;
use std::str::FromStr as _;

/// The bot's reply. Buttons are flat rows of `(label, callback_data)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub buttons: Vec<Vec<(String, String)>>,
    /// The reply to a press: we edit the message already sent rather than piling on new
    /// ones.
    pub edit: bool,
}

impl Reply {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// A question with two buttons. `data` encodes the action itself: the bot keeps no
    /// state between messages — a restart must not lose a dialogue already begun.
    fn confirm(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            buttons: vec![vec![
                ("Yes".into(), data.into()),
                ("Cancel".into(), "no".into()),
            ]],
            edit: false,
        }
    }

    fn done(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            buttons: Vec::new(),
            edit: true,
        }
    }
}

/// Handle an incoming item. `None` — there is nothing to reply with (empty text, not a
/// command).
///
/// # Errors
///
/// A database failure. Errors of the command itself (no such wallet, a duplicate, a
/// malformed address) are not errors: they are the reply to the operator.
pub async fn handle(db: &Db, incoming: &Incoming) -> anyhow::Result<Option<Reply>> {
    match incoming {
        Incoming::Message { chat_id, text } => run(db, *chat_id, parse_command(text)).await,
        Incoming::Button { chat_id, data, .. } => press(db, *chat_id, data).await,
    }
}

/// The actor for the audit trail. `operator` does not answer who exactly moved a wallet to
/// live — the chat does.
fn actor(chat_id: i64) -> String {
    // Zero means the console (`--say`): no chat has that number, and signing an edit with
    // it would record a non-existent chat in the audit trail.
    if chat_id == 0 {
        "cli".to_string()
    } else {
        format!("telegram:{chat_id}")
    }
}

async fn run(db: &Db, chat_id: i64, cmd: Command) -> anyhow::Result<Option<Reply>> {
    let reply = match cmd {
        Command::None => return Ok(None),
        Command::Help | Command::Unknown(_) => Reply::text(HELP),
        Command::Malformed(why) => Reply::text(why),

        Command::Wallets => Reply::text(render_list(db, &db.wallets().list().await?).await?),

        Command::Add { address, nickname } => {
            match db.wallets().add(&address, nickname.as_deref()).await {
                Ok(w) => Reply::text(format!(
                    "Added {}\n\nMode: paper · disabled · stake $0\nTo start copying: /stake {} <$>, then /on",
                    w.display(),
                    w.address
                )),
                // The only expected error is the uniqueness of the address.
                Err(_) => Reply::text(format!("The wallet {address} is already in the registry")),
            }
        }

        Command::Wallet { address } => match db.wallets().get(&address).await? {
            None => Reply::text(format!("The wallet {address} was not found")),
            Some(w) => card(&w),
        },

        Command::Mode { address, mode } => match require(db, &address).await? {
            Err(missing) => missing,
            // Moving to live spends real money: we ask.
            Ok(_) if mode == Mode::Live => Reply::confirm(
                format!(
                    "Move {address} to REAL MONEY?\nThe wallet will start spending actual funds from the account."
                ),
                format!("do:mode:{address}:live"),
            ),
            Ok(_) => {
                db.wallets().set_mode(&address, mode).await?;
                audit(db, &address, chat_id).await;
                Reply::text(format!("{address}: back on paper"))
            }
        },

        Command::Stake { address, usd } => match require(db, &address).await? {
            Err(missing) => missing,
            Ok(_) => Reply::confirm(
                format!("The stake for {address} is ${usd} per trade. Confirm?"),
                format!("do:stake:{address}:{usd}"),
            ),
        },

        // The command speaks in percent while a fraction is stored: `buy_limit` computes
        // `price x (1 + fraction)`. Storing "5" as it comes would mean 500% slippage —
        // that is, "buy at any price".
        Command::Slippage { pct, .. } if pct > Decimal::ONE_HUNDRED => Reply::text(format!(
            "{pct}% is not a threshold but consent to any price. The maximum is 100."
        )),
        Command::Slippage { address, pct } => match require(db, &address).await? {
            Err(missing) => missing,
            Ok(_) => {
                let fraction = pct / Decimal::ONE_HUNDRED;
                db.wallets().set_slippage(&address, fraction).await?;
                audit(db, &address, chat_id).await;
                Reply::text(format!("{address}: slippage threshold {}", fmt::percent(fraction)))
            }
        },

        Command::Enable { address, enabled } => match require(db, &address).await? {
            Err(missing) => missing,
            Ok(_) => {
                db.wallets().set_enabled(&address, enabled).await?;
                audit(db, &address, chat_id).await;
                Reply::text(format!(
                    "{address}: {}",
                    if enabled { "enabled" } else { "disabled" }
                ))
            }
        },

        Command::Kill => Reply::confirm(
            "Halt trading with real money?\nPaper mode will keep measuring — that is what it is for.",
            "do:kill",
        ),

        Command::Resume => {
            db.controls().set_manual_stop(false, &actor(chat_id)).await?;
            Reply::text("The stop is lifted. Live trading resumes on the next health tick.")
        }

        Command::SlippageProfile => Reply::text(render_slippage(db).await?),
        Command::Health => Reply::text(render_health(db).await?),
        Command::Balance => Reply::text(render_balance(db).await?),
        Command::Positions { all } => Reply::text(render_positions(db, all).await?),
        Command::Pnl { period } => Reply::text(render_pnl(db, period).await?),
        Command::Matchup { period } => Reply::text(render_matchup(db, period).await?),
        Command::Sources { period } => Reply::text(render_sources(db, period).await?),
        Command::FireRate => Reply::text(render_fire_rate(db).await?),
        Command::Latency { period } => Reply::text(render_latency(db, period).await?),
        Command::Flatten { mode } => declare_flatten(db, chat_id, mode.as_deref()).await?,
        // Plain text means something exactly when a phrase is expected. The rest of the
        // time the bot stays silent as it always did: answering every remark would teach
        // the operator not to read the replies.
        Command::Plain(text) => match confirm_flatten(db, chat_id, &text).await? {
            Some(r) => r,
            None => return Ok(None),
        },
        Command::Signals { n } => Reply::text(render_signals(db, n).await?),
        // The command exists in order to answer in words: silence on a known command reads
        // as a broken bot rather than as a missing scanner.
        Command::App => match crate::dashboard_url() {
            None => Reply::text("The dashboard address is not configured: [dashboard] public_url in the config."),
            Some(url) => Reply {
                text: "The dashboard: equity, wallets, positions and signals.".into(),
                buttons: vec![vec![("Open".into(), format!("webapp:{url}"))]],
                edit: false,
            },
        },
        Command::Scan => Reply::text(
            "There is no scanner yet: it lives as a separate binary and is not built into this release.",
        ),
    };
    Ok(Some(reply))
}

/// A button press. `data` carries the whole action — the bot keeps no state between
/// messages, and a restart loses no dialogue already begun.
async fn press(db: &Db, chat_id: i64, data: &str) -> anyhow::Result<Option<Reply>> {
    let parts: Vec<&str> = data.split(':').collect();
    let reply = match parts.as_slice() {
        ["no"] => Reply::done("Cancelled."),
        ["do", "kill"] => {
            db.controls().set_manual_stop(true, &actor(chat_id)).await?;
            Reply::done("Live trading is halted. Lift it with /resume")
        }
        // Giving up the expected payout is confirmed. The button ONLY records consent;
        // the phrase still does the closing — otherwise an irreversible action would be
        // left behind a button, and a button is pressed by accident.
        ["do", "flatten_ack"] => {
            db.controls()
                .set_flatten_intent(
                    garnet_core::flatten::FlattenMode::Hybrid.as_str(),
                    chrono::Utc::now(),
                    true,
                    &actor(chat_id),
                )
                .await?;
            Reply::done(flatten_prompt(garnet_core::flatten::FlattenMode::Hybrid))
        }
        ["do", "mode", address, "live"] => {
            db.wallets().set_mode(address, Mode::Live).await?;
            audit(db, address, chat_id).await;
            Reply::done(format!(
                "{address}: REAL MONEY — trades will come out of the account"
            ))
        }
        ["do", "mode", address, "shadow"] => {
            db.wallets().set_mode(address, Mode::Shadow).await?;
            audit(db, address, chat_id).await;
            Reply::done(format!("{address}: back on paper"))
        }
        ["do", "on", address, flag] => {
            let enabled = *flag == "1";
            db.wallets().set_enabled(address, enabled).await?;
            audit(db, address, chat_id).await;
            Reply::done(format!(
                "{address}: {}",
                if enabled { "enabled" } else { "disabled" }
            ))
        }
        ["do", "stake", address, usd] => {
            let usd = Decimal::from_str(usd)?;
            db.wallets().set_stake(address, usd).await?;
            audit(db, address, chat_id).await;
            Reply::done(format!("{address}: stake ${usd}"))
        }
        // A button from a message that outlived a change in the data format.
        _ => Reply::done("This button no longer works."),
    };
    Ok(Some(reply))
}

/// Whether such a wallet exists. `Err(Reply)` is a ready-made "not found" reply: a command
/// that silently did nothing looks like one that was carried out.
async fn require(db: &Db, address: &str) -> anyhow::Result<Result<Wallet, Reply>> {
    Ok(db
        .wallets()
        .get(address)
        .await?
        .ok_or_else(|| Reply::text(format!("The wallet {address} was not found"))))
}

/// Override the actor in the last audit entry.
///
/// `WalletRepo` writes the trace itself and signs it "operator": it knows nothing about
/// chats and should not. But a change has to name whoever made it, and the signature is
/// refined here — as a separate statement rather than a second trace, so that one change
/// stays one record.
async fn audit(db: &Db, address: &str, chat_id: i64) {
    let _ = sqlx::query(
        "UPDATE wallet_events SET actor = $2
         WHERE id = (SELECT max(id) FROM wallet_events WHERE wallet = $1)",
    )
    .bind(address)
    .bind(actor(chat_id))
    .execute(db.pool())
    .await;
}

fn card(w: &Wallet) -> Reply {
    Reply {
        text: format!(
            "{} {}\n{}\n\nMode: {}\nStake: {} per trade\nSlippage threshold: {}\nState: {}",
            if w.enabled { "🟢" } else { "⚪" },
            w.display(),
            w.address,
            fmt::mode_words(w.mode),
            fmt::money(w.stake_usd),
            fmt::percent(w.max_slippage_pct),
            if w.enabled {
                "being copied"
            } else {
                "disabled, signals are not executed"
            }
        ),
        buttons: vec![vec![
            if w.mode == Mode::Live {
                (
                    "Back to paper".into(),
                    format!("do:mode:{}:shadow", w.address),
                )
            } else {
                (
                    "To real money".into(),
                    format!("do:mode:{}:live", w.address),
                )
            },
            if w.enabled {
                ("Disable".into(), format!("do:on:{}:0", w.address))
            } else {
                ("Enable".into(), format!("do:on:{}:1", w.address))
            },
        ]],
        edit: false,
    }
}

/// Whether the registry holds even one live wallet.
///
/// The operator saw a LIVE line with an account balance in the summary and concluded the
/// bot was trading real money. An empty live account is not a portfolio, and that has to
/// be said in words.
async fn has_live_wallets(db: &Db) -> anyhow::Result<bool> {
    Ok(db
        .wallets()
        .list()
        .await?
        .iter()
        .any(|w| w.mode == Mode::Live))
}

/// Why there is no live block.
async fn live_absence(db: &Db) -> anyhow::Result<String> {
    Ok(if has_live_wallets(db).await? {
        "   There have been no trades yet.".to_string()
    } else {
        "   There are no live wallets — no real money is being spent.".to_string()
    })
}

/// A signal's verdict in words.
///
/// `copy` and `skip:slippage_exceeded` are our internal labels; the reader wants the
/// meaning, not the name of an enum variant.
fn verdict_words(verdict: &str) -> &'static str {
    match verdict {
        "copy" => "Copied the leader",
        "skip:slippage_exceeded" => "Skipped: the price moved past the slippage threshold",
        "skip:market_not_tradable" => {
            "Skipped: the market is not tradable — there is nobody to buy from"
        }
        "skip:wallet_disabled" => "Skipped: the wallet is disabled",
        "skip:insufficient_balance" => "Skipped: not enough free funds",
        "skip:duplicate" => "Skipped: this is part of an order already copied",
        "skip:exposure_capped" => "Skipped: the ceiling for this event is already reached",
        "skip:rate_limited" => "Skipped: the leader is firing a burst, the entry limit is reached",
        _ => "Skipped",
    }
}

fn outcome_or_token(outcome: &str, token_id: &str) -> String {
    if outcome.is_empty() {
        fmt::short(token_id)
    } else {
        outcome.to_string()
    }
}

/// What we hold right now.
///
/// The summary comes first, the list after and with a ceiling: on 05.09.2026 two hundred
/// and one positions produced 13,607 bytes, Telegram rejected the whole message, and the
/// command looked broken. The modes are not added together even here: a paper portfolio
/// next to a real one is the only comparison it exists for.
async fn render_positions(db: &Db, all: bool) -> anyhow::Result<String> {
    let open = db.reports().open_positions().await?;
    let mut out = String::new();

    for mode in [Mode::Live, Mode::Shadow] {
        let rows: Vec<_> = open.iter().filter(|p| p.mode == mode).collect();
        if rows.is_empty() {
            continue;
        }
        let total: Decimal = rows.iter().map(|p| p.cost_usd).sum();
        out.push_str(&format!(
            "{} · {} · invested {}\n\n",
            fmt::mode_title(mode),
            fmt::plural(rows.len(), "position", "positions"),
            fmt::money(total),
        ));

        // By wallet: two hundred lines of listing do not answer "where are we sitting",
        // whereas four lines of totals do.
        let mut by_wallet: Vec<(String, usize, Decimal)> = Vec::new();
        for p in &rows {
            let name = p.nickname.clone().unwrap_or_else(|| fmt::short(&p.wallet));
            match by_wallet.iter_mut().find(|(n, _, _)| *n == name) {
                Some(e) => {
                    e.1 += 1;
                    e.2 += p.cost_usd;
                }
                None => by_wallet.push((name, 1, p.cost_usd)),
            }
        }
        by_wallet.sort_by_key(|w| std::cmp::Reverse(w.2));
        out.push_str("By wallet:\n");
        for (name, n, cost) in by_wallet.iter().take(5) {
            out.push_str(&format!(
                "  {name} — {n} pos. worth {}\n",
                fmt::money(*cost)
            ));
        }
        if by_wallet.len() > 5 {
            let rest: Decimal = by_wallet.iter().skip(5).map(|(_, _, c)| *c).sum();
            out.push_str(&format!(
                "  {} more worth {}\n",
                fmt::plural(by_wallet.len() - 5, "wallet", "wallets"),
                fmt::money(rest),
            ));
        }

        let mut largest = rows.clone();
        largest.sort_by_key(|p| std::cmp::Reverse(p.cost_usd));
        out.push_str("\nLargest:\n");
        for p in largest.iter().take(10) {
            out.push_str(&format!(
                "  {} — {} · {}\n",
                outcome_or_token(&p.outcome_text, &p.token_id),
                fmt::money(p.cost_usd),
                fmt::ago(p.opened_at),
            ));
        }
        if largest.len() > 10 {
            out.push_str(&format!(
                "\n  {} more — the full list is in the dashboard: /app\n",
                fmt::plural(largest.len() - 10, "position", "positions"),
            ));
        }
        out.push('\n');
    }

    if out.is_empty() {
        out.push_str("There are no open positions.\n");
    }

    if all {
        out.push_str("\n— — —\n\n");
        out.push_str(&render_pnl(db, Period::All).await?);
    }
    Ok(fmt::clamp(
        out,
        "\n…the list is truncated; the full one is in the dashboard: /app",
    ))
}

/// The result: what was invested, what came back, and what that is in percent.
///
/// A mode with no closed positions used to be simply absent from the reply, and its
/// absence read as a breakage. An empty block says in words why it is empty.
async fn render_pnl(db: &Db, period: Period) -> anyhow::Result<String> {
    let rows = db.reports().realised_pnl(period.since()).await?;
    let mut out = format!("📊 Result {}\n\n", period.label());

    for mode in [Mode::Live, Mode::Shadow] {
        out.push_str(fmt::mode_title(mode));
        out.push('\n');
        match rows.iter().find(|r| r.mode == mode) {
            None if mode == Mode::Live => {
                out.push_str(&live_absence(db).await?);
                out.push('\n');
            }
            None => out.push_str("   There were no closed positions.\n"),
            Some(r) => {
                out.push_str(&format!("   Closed {} · won {}\n", r.closed, r.won));
                out.push_str(&format!("   Staked {}\n", fmt::money(r.cost_usd)));
                out.push_str(&format!("   Fees {}\n", fmt::money(r.fees_usd)));
                out.push_str(&format!("   Payouts {}\n", fmt::money(r.payout_usd)));
                if r.proceeds_usd > Decimal::ZERO {
                    // The leader exited before resolution: without this line the payout
                    // does not add up against the stake, and the numbers look like a lie.
                    out.push_str(&format!("   Sold {}\n", fmt::money(r.proceeds_usd)));
                }
                out.push_str(&format!("   Result {}", fmt::signed_money(r.pnl_usd)));
                if let Some(pct) = fmt::roi(r.pnl_usd, r.cost_usd) {
                    out.push_str(&format!(" ({pct})"));
                }
                out.push('\n');
            }
        }
        out.push('\n');
    }
    Ok(fmt::clamp(out, "\n…truncated"))
}

/// The latest decisions and the reasons for skips.
/// How far the price moved past the ceiling.
///
/// "Past the threshold" without figures does not say how far past: a threshold is raised
/// for a tenth of a cent, not for twenty percent. Both halves of the miss have lived in
/// the signal row since 06.09.2026; earlier records do not have them, and then the line
/// stays as it was.
fn miss_words(r: &SignalRow) -> String {
    match (r.best_ask, r.limit_price) {
        (Some(ask), Some(cap)) if r.verdict == "skip:slippage_exceeded" => {
            format!(" ({ask} against a ceiling of {cap})")
        }
        _ => String::new(),
    }
}

async fn render_signals(db: &Db, n: i64) -> anyhow::Result<String> {
    let rows = db.reports().recent_signals(n.clamp(1, 50)).await?;
    if rows.is_empty() {
        return Ok("There have been no decisions yet.".into());
    }
    let mut out = String::from("🎯 Latest decisions\n\n");
    for r in &rows {
        // An exit has no stake size: `target_size_usd` is zero there, and "for $0" reads as
        // an arithmetic error rather than as a sale following the leader.
        let what = if r.side == "sell" {
            format!(
                "Exited \"{}\"",
                outcome_or_token(&r.outcome_text, "outcome unknown")
            )
        } else if r.verdict != "copy" {
            // A skip has no stake: `target_size_usd` is zero there, and "for $0" reads as an
            // arithmetic error — exactly what "for $0" was on an exit. A skipped signal is
            // named by its outcome and its reason.
            outcome_or_token(&r.outcome_text, "outcome unknown")
        } else {
            format!(
                "{} for {}",
                outcome_or_token(&r.outcome_text, "outcome unknown"),
                fmt::money(r.target_size_usd)
            )
        };
        out.push_str(&format!(
            "{} {}{}\n   {} {} · {}\n   {what}\n\n",
            if r.verdict == "copy" { "🟢" } else { "⛔" },
            verdict_words(&r.verdict),
            miss_words(r),
            fmt::mode_badge(r.mode),
            r.nickname.clone().unwrap_or_else(|| fmt::short(&r.wallet)),
            fmt::ago(r.ts_signal),
        ));
    }
    Ok(fmt::clamp(
        out,
        "\n…truncated, the rest is in the dashboard: /app",
    ))
}

/// The registry with each wallet's 24-hour result: a list without it answers what the bot
/// copies but not whether it is worth it.
async fn render_list(db: &Db, wallets: &[Wallet]) -> anyhow::Result<String> {
    if wallets.is_empty() {
        return Ok("The registry is empty. Add one with: /add <address> [nickname]".into());
    }
    let pnl = db.reports().pnl_by_wallet(Period::Day.since()).await?;
    let mut out = format!("👛 {}\n\n", fmt::plural(wallets.len(), "wallet", "wallets"));
    for w in wallets {
        out.push_str(&format!(
            "{} {} · {}\n   {} per trade · slippage {} · 24h {}\n   {}\n\n",
            if w.enabled { "🟢" } else { "⚪" },
            w.display(),
            fmt::mode_words(w.mode),
            fmt::money(w.stake_usd),
            fmt::percent(w.max_slippage_pct),
            fmt::signed_money(pnl.get(&w.address).copied().unwrap_or_default()),
            w.address,
        ));
    }
    Ok(fmt::clamp(
        out,
        "\n…the list is truncated; the full one is in the dashboard: /app",
    ))
}

/// The distribution of the price miss: how far the best ask was above the leader's price.
///
/// It answers the one question a threshold is moved for: do expensive fills lose money.
/// The tail is truncated by the active threshold — no observations more expensive than it
/// exist, because they were refused — and so every bucket shows how many signals were
/// discarded.
async fn render_slippage(db: &Db) -> anyhow::Result<String> {
    let rows = db.reports().slippage_profile().await?;
    if rows.is_empty() {
        return Ok("No prices have accumulated yet: the threshold is tuned from the tail, and that takes days to gather.\n\nChange the threshold with: /slippage <address> <%>"
            .into());
    }
    let mut out = String::from("📐 Price miss\n\n");
    for r in &rows {
        let label = match r.from_pct {
            p if p == Decimal::ZERO => "under 1%".to_string(),
            p if p >= Decimal::new(10, 2) => "10% and above".to_string(),
            p => format!("from {}%", (p * Decimal::ONE_HUNDRED).normalize()),
        };
        out.push_str(&format!(
            "{label}\n   taken {} · skipped {}\n",
            r.copied, r.skipped
        ));
        if r.closed > 0 {
            out.push_str(&format!(
                "   closed {} worth {} · {}",
                r.closed,
                fmt::money(r.cost_usd),
                fmt::signed_money(r.pnl_usd)
            ));
            if let Some(pct) = fmt::roi(r.pnl_usd, r.cost_usd) {
                out.push_str(&format!(" ({pct})"));
            }
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(
        "The threshold binds from above: there are no observations more expensive than it.\n",
    );
    out.push_str("Change it with: /slippage <address> <%>");
    Ok(fmt::clamp(out, "\n…truncated"))
}

async fn render_health(db: &Db) -> anyhow::Result<String> {
    // Feed lag lives in the trading process and is invisible to a separate bot. What is
    // honestly visible from the database is when the last leader trade reached the record.
    // That is the "flow", not "the process is alive" (invariant 11).
    // The circuits are counted separately. On 06.09.2026, on a cold start, health printed
    // "ok" while the socket brought nothing for six minutes: the safety-net poll's flow
    // counted as flow in general. The poll runs every few seconds and always looks alive —
    // it must not close the question about the socket, which is why health is measured by
    // flow at all (invariant 11).
    let rtds: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT max(ts_seen) FROM leader_trades WHERE source = 'rtds'")
            .fetch_one(db.pool())
            .await?;
    let poll: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT max(ts_seen) FROM leader_trades WHERE source = 'poll'")
            .fetch_one(db.pool())
            .await?;
    // The third circuit is counted separately for the same reason as the first two
    // (invariant 31): a circuit whose flow is added to another's can be neither checked nor
    // switched off. The line is not shown until the circuit has brought at least one trade
    // — a disabled circuit must not look broken.
    let chain: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT max(ts_seen) FROM leader_trades WHERE source = 'chain'")
            .fetch_one(db.pool())
            .await?;
    let chain_line = chain.map_or_else(String::new, |t| format!("\nChain logs: {}", fmt::ago(t)));
    let stopped = db.controls().manual_stop().await?;

    // The age of the equity snapshot is the trading process's trace in the database: it is
    // written by a timer, and its absence means the core is not running. The bot cannot ask
    // the core itself: it has neither its memory nor its socket.
    let heartbeat = db.equity().latest(Mode::Shadow).await?;
    let watched = db
        .wallets()
        .list()
        .await?
        .iter()
        .filter(|w| w.enabled)
        .count();
    let live = has_live_wallets(db).await?;

    // A leader merge is their exit on both legs (invariant 40). One seen and not acted upon
    // means the leader left the position while we stayed in it: an empty book, a stop or a
    // resolved market. Staying silent about it is not allowed — the position is held with a
    // `leader_observed_size` that no longer means anything.

    let stuck = db.actions().unhandled_merges(20).await?;
    let merges = if stuck.is_empty() {
        String::new()
    } else {
        format!(
            "\n⚠️ Leader merges with no exit of ours: {} (earliest {})",
            stuck.len(),
            fmt::ago(stuck[0].ts_action)
        )
    };

    Ok(format!(
        "❤️ Health\n\nSocket brought a trade: {}\nSafety-net poll: {}{chain_line}\nCore last reported: {}\nWallets enabled: {watched}{merges}\n\nLive trading: {}",
        rtds.map_or_else(|| "none yet".to_string(), fmt::ago),
        poll.map_or_else(|| "none yet".to_string(), fmt::ago),
        heartbeat.map_or_else(
            || "never yet — the core may not have started".to_string(),
            |s| fmt::ago(s.ts)
        ),
        match (stopped, live) {
            (true, _) => "HALTED by the operator · lift with /resume",
            (false, false) => "permitted, but there are no live wallets",
            (false, true) => "running",
        },
    ))
}

/// Accounts per mode.
///
/// The live block no longer looks like a portfolio when there are no live wallets: that is
/// exactly how the operator concluded on 05.09.2026 that the bot was spending real money.
/// But the figures stay — hiding the account balance would swap one misunderstanding for
/// another.
async fn render_balance(db: &Db) -> anyhow::Result<String> {
    let mut out = String::from("💰 Accounts\n\n");
    let live_exists = has_live_wallets(db).await?;

    for mode in [Mode::Live, Mode::Shadow] {
        out.push_str(fmt::mode_title(mode));
        out.push('\n');
        if mode == Mode::Live && !live_exists {
            out.push_str("   There are no live wallets — no real money is being spent.\n");
        }

        match db.equity().latest(mode).await? {
            None => out.push_str("   There have been no snapshots yet.\n"),
            Some(s) => {
                out.push_str(&format!("   Free {}\n", fmt::money(s.cash_usd)));
                out.push_str(&format!(
                    "   In positions {}\n",
                    fmt::money(s.positions_value)
                ));
                out.push_str(&format!("   Total {}", fmt::money(s.total_usd)));
                // The 24-hour delta: the total by itself does not say which way it is
                // going. The baseline is the earliest snapshot **in the window** rather
                // than exactly 24 hours old: there may have been no snapshots that day,
                // and demanding an exact timestamp means never showing the delta.
                if let Some(old) = db.reports().equity_at(mode, Period::Day.since()).await? {
                    out.push_str(&format!(
                        " · 24h {}",
                        fmt::signed_money(s.total_usd - old.total_usd)
                    ));
                }
                out.push('\n');
                // A position the market gave no price for did not enter the value: the
                // total is understated. Inventing a price is not allowed — but neither is
                // passing an incomplete total off as a complete one.
                if s.unpriced > 0 {
                    out.push_str(&format!(
                        "   ⚠️ {} without a price — \"in positions\" is understated\n",
                        fmt::plural(
                            usize::try_from(s.unpriced).unwrap_or(0),
                            "position",
                            "positions"
                        ),
                    ));
                }
                out.push_str(&format!("   Updated {}\n", fmt::ago(s.ts)));
            }
        }
        out.push('\n');
    }
    Ok(out)
}

/// The quality of copying against the quality of the leader (invariants 49 and 50).
///
/// It answers a question `/pnl` does not: is a loss a bad leader with flawless execution,
/// or a good leader with poor execution. The controls we have (slippage, the slice window,
/// the stake) tune only the latter, and turning them by the absolute result means looking
/// at the wrong instrument.
///
/// Skips are printed on **a separate line** and do not enter the averages: the average gap
/// over the positions taken flatters us by exactly the discarded tail.
async fn render_matchup(db: &Db, period: Period) -> anyhow::Result<String> {
    let mut out = format!("🎯 Copy quality {}\n\n", period.label());

    for mode in [Mode::Live, Mode::Shadow] {
        let rep = db.matchup().report(mode, period.since()).await?;
        out.push_str(fmt::mode_title(mode));
        out.push('\n');

        if rep.rows.is_empty() {
            out.push_str("   Nothing to compare: there were no leader trades in this period.\n\n");
            continue;
        }

        match rep.avg_entry_diff_c {
            // Zero here is a real zero: we entered at the leader's price. The absence of a
            // number is printed in words, not as a zero (invariant 27).
            Some(c) => out.push_str(&format!(
                "   Entry {} than the leader on average\n",
                entry_words(c)
            )),
            None => out.push_str("   Entry: nothing to measure with — not enough prices\n"),
        }
        match rep.avg_gap_pts {
            Some(g) => out.push_str(&format!(
                "   Return gap {} pp{}\n",
                fmt::signed_pts(g),
                if g > Decimal::ZERO {
                    " (they are ahead)"
                } else {
                    ""
                }
            )),
            None => out.push_str("   Return gap: there are no closed positions yet\n"),
        }
        out.push_str(&format!(
            "   Measurable {} · skipped {}\n",
            rep.n_measurable, rep.n_skipped
        ));
        if rep.n_unevaluated > 0 {
            // Not skips: in this mode no decision was taken at all. Adding them to the
            // skips would inflate the tail with somebody else's mode.
            out.push_str(&format!(
                "   Not evaluated in this mode: {}\n",
                rep.n_unevaluated
            ));
        }

        // The worst entries: what the report is opened for.
        let mut worst: Vec<_> = rep
            .rows
            .iter()
            .filter_map(|r| r.entry_diff_c().map(|c| (c, r)))
            .collect();
        worst.sort_by_key(|(c, _)| std::cmp::Reverse(*c));
        for (c, r) in worst.iter().take(3).filter(|(c, _)| *c > Decimal::ZERO) {
            out.push_str(&format!(
                "   · {} {} — {}\n",
                fmt::short(&r.question),
                r.outcome_label,
                entry_words(*c)
            ));
        }
        out.push('\n');
    }

    out.push_str("The gap is computed against the leader's average price since the wallet was\n");
    out.push_str("assigned, and includes fees. Skips do not enter the average.");
    Ok(fmt::clamp(out, "\n…truncated"))
}

/// The entry difference in words. The sign is against us, but the reader wants a direction
/// rather than a sign: "more" and "less" are not confused, "+10" and "-10" are.
fn entry_words(cents: Decimal) -> String {
    let abs = cents.abs().round_dp(2).normalize();
    if cents == Decimal::ZERO {
        "at the price".to_string()
    } else if cents > Decimal::ZERO {
        format!("{abs}¢ more expensive")
    } else {
        format!("{abs}¢ cheaper")
    }
}

/// Declare the intent to close positions (invariant 46).
///
/// By itself it closes nothing: the declaration merely opens the minute during which the
/// phrase is accepted.
async fn declare_flatten(db: &Db, chat_id: i64, mode: Option<&str>) -> anyhow::Result<Reply> {
    use garnet_core::flatten::FlattenMode;

    let Some(raw) = mode else {
        return Ok(Reply::text(FLATTEN_HELP));
    };
    let Some(mode) = FlattenMode::parse(raw) else {
        return Ok(Reply::text(format!(
            "\"{raw}\" is not a mode.\n\n{FLATTEN_HELP}"
        )));
    };

    // Giving up the expected payout is confirmed SEPARATELY rather than inferred from the
    // choice of mode: inferring consent from the mode means not asking for it at all.
    if mode == FlattenMode::Hybrid {
        return Ok(Reply::confirm(
            "hybrid sells winning positions BEFORE resolution.\n\
             That is a deliberate forfeiture of the expected payout on them.\n\n\
             Do you confirm the forfeiture?",
            "do:flatten_ack",
        ));
    }

    db.controls()
        .set_flatten_intent(mode.as_str(), chrono::Utc::now(), false, &actor(chat_id))
        .await?;

    // It is more honest to mention halted trading straight away rather than after the
    // phrase: typing it and being refused spends the very minute this was all for. The
    // check still stays at the gate: trading can be halted between the declaration and the
    // phrase.
    let mut text = flatten_prompt(mode);
    if mode == FlattenMode::Panic && !db.controls().manual_stop().await? {
        text.push_str(
            "\n\n⚠️ Trading is running right now. Use /kill first — otherwise the phrase will be rejected.",
        );
    }
    Ok(Reply::text(text))
}

fn flatten_prompt(mode: garnet_core::flatten::FlattenMode) -> String {
    use garnet_core::flatten::{required_phrase, FlattenMode, INTENT_TTL_SECS};
    let what = match mode {
        FlattenMode::Graceful => {
            "will halt trading and sell NOTHING — the positions live to resolution"
        }
        FlattenMode::Hybrid => "will sell only what is currently in profit",
        FlattenMode::Panic => "will sell EVERYTHING at the best bid",
    };
    format!(
        "⚠️ Emergency close: {}\n{what}.\n\n\
         Type exactly this phrase, as a separate message:\n\
         `{}`\n\n\
         You have {INTENT_TTL_SECS} seconds. Any other text cancels the intent.",
        mode.as_str(),
        required_phrase(mode, garnet_telegram_name()),
    )
}

/// The installation's name. Extracted into a function so that a test can compare the phrase
/// against the same value rather than a hand-copied one.
fn garnet_telegram_name() -> &'static str {
    crate::flatten_name()
}

/// Check the phrase. `None` — nobody was waiting for a phrase, and this is not a reply from
/// the bot.
async fn confirm_flatten(db: &Db, chat_id: i64, text: &str) -> anyhow::Result<Option<Reply>> {
    use garnet_core::flatten::{check, FlattenMode, Gate, Intent};

    let (raw_mode, declared_at, ack) = match db.controls().flatten_intent().await? {
        // Nobody was waiting for a phrase — this is not for us.
        garnet_db::StoredIntent::None => return Ok(None),
        // But this is: the operator declared an intent and is waiting for an answer.
        // Silence here would read as a broken bot.
        garnet_db::StoredIntent::Unreadable(_) => {
            db.controls().clear_flatten_intent().await?;
            return Ok(Some(Reply::text(
                "The intent in the database is unreadable — it has been cleared. Start again: /flatten",
            )));
        }
        garnet_db::StoredIntent::Some {
            mode,
            declared_at,
            acknowledge_forfeit,
        } => (mode, declared_at, acknowledge_forfeit),
    };
    // A corrupted value does not open the gate — it forbids it.
    let Some(mode) = FlattenMode::parse(&raw_mode) else {
        db.controls().clear_flatten_intent().await?;
        return Ok(Some(Reply::text(
            "The intent in the database is unreadable — it has been cleared. Start again: /flatten",
        )));
    };

    let intent = Intent {
        mode,
        declared_at,
        acknowledge_forfeit: ack,
    };
    let stopped = db.controls().manual_stop().await?;

    match check(
        &intent,
        text,
        garnet_telegram_name(),
        chrono::Utc::now(),
        stopped,
    ) {
        Gate::Go(mode) => {
            // The gate is passed. The trading process does the selling: the bot has neither
            // the exchange nor the book. Through the database rather than the bus, for the
            // same reason as `/kill` — an emergency close must work when NATS is
            // unreachable.
            db.controls()
                .approve_flatten(mode.as_str(), &actor(chat_id))
                .await?;
            db.controls().clear_flatten_intent().await?;
            Ok(Some(Reply::text(format!(
                "Accepted: {}. The trading process will execute it on the next tick.\n\
                 What came of it is in /positions and /health.",
                mode.as_str()
            ))))
        }
        Gate::Refuse(r) => {
            // The intent is cleared on any refusal: a second attempt is a second decision,
            // and it has to be taken afresh.
            db.controls().clear_flatten_intent().await?;
            Ok(Some(Reply::text(format!(
                "Refused: {}.\n\nThe intent has been cleared. Start again: /flatten",
                r.why()
            ))))
        }
    }
}

const FLATTEN_HELP: &str = "⚠️ Emergency close of positions

/flatten graceful — halt trading, leave the positions to live out
/flatten hybrid — sell only what is in profit
/flatten panic — sell everything (only while trading is halted)

Every mode requires typing the phrase in full: a button is pressed by accident,
a phrase is not. No mode touches paper positions.";

/// Latency per stage of the signal path (invariant 42).
///
/// Computed from the database rather than from process metrics: those live in its memory,
/// neither the bot nor the operator sees them after a restart — and "before and after" has
/// to be compared precisely across a restart.
///
/// Copying is measured by latency, and speeding it up without seeing it per stage means
/// moving blind. None of the stages makes us faster than the leader, though: we copy what
/// has already executed, and the point is not to lose seconds on top of that.
async fn render_latency(db: &Db, period: Period) -> anyhow::Result<String> {
    let rows = db.reports().latency(period.since()).await?;
    if rows.is_empty() {
        return Ok(format!(
            "⏱ Latency {}\n\nThere were no signals in this period: nothing to measure.",
            period.label()
        ));
    }

    let mut out = format!("⏱ Latency {}\n\n", period.label());
    for r in rows.iter().filter(|r| r.stage != "trade_to_submitted") {
        out.push_str(&format!("{}\n", stage_words(&r.stage)));
        match (r.median_secs, r.p90_secs) {
            (Some(m), Some(p)) => {
                out.push_str(&format!(
                    "   median {} s · 90th percentile {} s · {} samples\n",
                    m.round_dp(2).normalize(),
                    p.round_dp(2).normalize(),
                    r.n
                ));
            }
            // A stage without a number is a stage that did not happen, not an instantaneous
            // one. Zero here would read as "everything is already fast".
            _ => out.push_str("   nothing to measure with\n"),
        }
    }

    // The end-to-end figure is computed per trade rather than by adding medians: the median
    // of a sum does not equal the sum of medians, and the added-up number would resemble
    // the truth without being it.
    if let Some(e2e) = rows.iter().find(|r| r.stage == "trade_to_submitted") {
        match e2e.median_secs {
            Some(m) => out.push_str(&format!(
                "\nFrom the leader's trade to our order: median {} s\n",
                m.round_dp(2).normalize()
            )),
            None => {
                out.push_str("\nFrom the leader's trade to our order: nothing to measure with\n")
            }
        }
    }
    out.push_str("The target is under 2 s (the predecessor's median was 4.12 s).\n\n");
    out.push_str("This does not make us faster than the leader: we copy what has executed.");
    Ok(fmt::clamp(out, "\n…truncated"))
}

/// The stages in words: `signal_to_submitted` says nothing to a reader.
fn stage_words(s: &str) -> &str {
    match s {
        "trade_to_seen" => "① The leader's trade reached us",
        "seen_to_signal" => "② The decision was taken and recorded",
        "signal_to_submitted" => "③ The order was submitted",
        "submitted_to_filled" => "④ The order was filled (live only)",
        other => other,
    }
}

/// The depth of the entry queue per wallet (invariant 44).
///
/// It answers the one question without which `fire_limit` cannot be set: how often the
/// leader fires a burst. The measurement this summary exists for could not be made on
/// Garnet's own production data — the database went down together with the box on
/// 18.09.2026, and the archive that was brought over turned out to be the
/// predecessor's. So it lives here and is computed on whatever data accumulates.
async fn render_fire_rate(db: &Db) -> anyhow::Result<String> {
    let mut out = String::from("🔥 Entry queue per wallet\n\n");

    for window in [30_i64, 60, 300] {
        let rows = db.reports().fire_depth(window).await?;
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("Window {window} s\n"));
        for mode in [Mode::Live, Mode::Shadow] {
            let of_mode: Vec<_> = rows.iter().filter(|r| r.mode == mode).collect();
            if of_mode.is_empty() {
                continue;
            }
            let total: i64 = of_mode.iter().map(|r| r.copies).sum();
            let deep: i64 = of_mode
                .iter()
                .filter(|r| r.depth > 1)
                .map(|r| r.copies)
                .sum();
            let max = of_mode.iter().map(|r| r.depth).max().unwrap_or(0);
            out.push_str(&format!(
                "   {} copies {total} · in bursts {deep} · longest {max}\n",
                fmt::mode_badge(mode)
            ));
        }
        out.push('\n');
    }

    if out.lines().count() <= 2 {
        return Ok("🔥 Entry queue per wallet\n\n\
                   There are no copies yet: the distribution is gathered from the engine's \
                   decisions, and before the first entries it does not exist.\n\n\
                   The threshold is set from it, not from a guess."
            .into());
    }

    // The counterfactual: how much each limit would have refused over history that has
    // already happened. Computed by replaying it through the limiter itself rather than by a
    // formula over depths — a refused entry does not enter the queue and does not deepen
    // the ones that follow, so counting by depths would overstate the refusals.
    let copies = db.reports().copy_times(20_000).await?;
    if !copies.is_empty() {
        out.push_str(&format!(
            "Had the limit been set (30 s window, {} copies):\n",
            copies.len()
        ));
        for limit in [1_usize, 2, 4, 10] {
            let refused = garnet_risk::fire_rate::would_refuse(&copies, 30, limit);
            out.push_str(&format!(
                "   {limit} → refused {refused} ({}%)\n",
                refused * 100 / copies.len().max(1)
            ));
        }
        out.push('\n');
    }

    out.push_str(
        "The `fire_limit` threshold is set from this distribution, not from a guess.\n\
         Zero means disabled: there is no refusal, but the counting continues.",
    );
    Ok(fmt::clamp(out, "\n…truncated"))
}

/// The race between delivery circuits (invariant 43).
///
/// It answers a question the `leader_trades.source` column cannot be asked: that column
/// holds the winner, and whether the safety-net poll pays for itself does not follow from
/// it. The poll costs us backfill duplicates and 69 false `market_not_tradable` refusals;
/// "the unique share" is the answer to whether it pays for anything.
async fn render_sources(db: &Db, period: Period) -> anyhow::Result<String> {
    let rows = db.reports().source_race(period.since()).await?;
    if rows.is_empty() {
        return Ok(format!(
            "🏁 Delivery circuits {}\n\nThere are no sightings yet: there were no leader trades in this period.",
            period.label()
        ));
    }

    let mut out = format!("🏁 Delivery circuits {}\n\n", period.label());
    for r in &rows {
        out.push_str(&format!("{}\n", source_words(&r.source)));
        out.push_str(&format!(
            "   First: {} · confirmed: {}\n",
            r.wins, r.confirmations
        ));
        match r.median_lag_secs {
            Some(l) => out.push_str(&format!("   Median lag: {} s\n", l.round_dp(1).normalize())),
            // Only an empty sample has no median, and an empty one does not reach here.
            // But a zero must not be substituted here: zero means "always first".
            None => out.push_str("   Median lag: nothing to measure with\n"),
        }
        out.push_str(&format!(
            "   Brought by it alone: {} of {}\n\n",
            r.only_source, r.sightings
        ));
    }

    // A circuit without a single unique trade catches nothing that would not be caught
    // without it — that is the grounds for switching it off. But if that is true of ALL the
    // circuits, it only means they duplicate each other: any ONE can be switched off, and
    // under no circumstances all of them. Naming the first on the list in that case would
    // blame a circuit for its place in the alphabet.
    let idle: Vec<&str> = rows
        .iter()
        .filter(|r| r.only_source == 0 && r.sightings > 0)
        .map(|r| source_words(&r.source))
        .collect();
    if !idle.is_empty() {
        if idle.len() == rows.len() {
            out.push_str(
                "⚠️ The circuits see the same thing: none of them has a unique trade.\n\
                 Any ONE can be switched off — but not all at once.\n",
            );
        } else {
            for name in &idle {
                out.push_str(&format!(
                    "⚠️ {name} brought no trade that would have been missing without it.\n"
                ));
            }
        }
    }
    out.push_str("The winner lands in leader_trades.source; the loser only here.");
    Ok(fmt::clamp(out, "\n…truncated"))
}

/// The circuit's label in words: `rtds` and `poll` say nothing to a reader.
fn source_words(s: &str) -> &str {
    match s {
        "rtds" => "🔌 Socket",
        "poll" => "🔁 Safety-net poll",
        "chain" => "⛓ Chain logs",
        other => other,
    }
}

/// The command menu for the Telegram client: what is shown for "/".
///
/// Kept next to [`HELP`] on purpose: two lists of commands living in different places
/// diverge on the very first command added.
pub const MENU: &[(&str, &str)] = &[
    ("/positions", "what we hold right now"),
    ("/pnl", "the result: 24 hours, a week or all time"),
    ("/balance", "accounts: free, in positions, total"),
    ("/wallets", "wallets and their 24-hour result"),
    ("/matchup", "how well we copy: us against the leader"),
    (
        "/sources",
        "which circuit brings trades and which duplicates them",
    ),
    ("/firerate", "how often the leader fires a burst of entries"),
    (
        "/latency",
        "latency per stage: from the leader's trade to our order",
    ),
    ("/signals", "the latest decisions and why we skipped"),
    (
        "/health",
        "is the trade flow running, and is trading halted",
    ),
    ("/wallet", "a wallet card with buttons"),
    ("/add", "add a wallet: /add <address> [name]"),
    (
        "/stake",
        "how much to stake per trade: /stake <address> <$>",
    ),
    (
        "/slippage",
        "the slippage threshold: /slippage <address> <%>",
    ),
    ("/mode", "paper or real money: /mode <address> live|shadow"),
    ("/on", "enable copying for a wallet"),
    ("/off", "disable copying for a wallet"),
    ("/kill", "halt trading with real money"),
    (
        "/flatten",
        "emergency-close positions: graceful | hybrid | panic",
    ),
    ("/resume", "lift the stop"),
    ("/app", "open the dashboard"),
    ("/help", "what the bot can do"),
];

const HELP: &str = "What the bot can do

LOOK
/positions — what we hold right now
/pnl [day|week|all] — the result: staked, returned, total
/balance — accounts per mode
/wallets — wallets and their day
/matchup [day|week|all] — how well we copy, not how much we earned
/sources [day|week|all] — which delivery circuit brings the trades
/firerate — the depth of the entry queue: the limit is set from it
/latency [day|week|all] — latency per stage of the signal path
/signals [N] — the latest decisions and the reasons for skips
/health — is the trade flow running
/app — the dashboard, with every list

CONFIGURE
/add <address> [name] — register a wallet (paper, disabled)
/wallet <address> — a card with buttons
/stake <address> <$> — how much to stake per trade
/slippage <address> <%> — how much worse than the leader's price we accept
/mode <address> live|shadow — real money or paper
/on <address> · /off <address> — enable and disable copying

STOP
/kill — stop spending real money
/flatten [graceful|hybrid|panic] — emergency-close positions
/resume — lift the stop

/kill halts trading and LEAVES the positions. /flatten closes them and requires
typing the phrase in full: a button is pressed by accident, a phrase is not.

Paper — trades are accounted for, but no money is spent.
Real money — orders go to the exchange from the account.";
