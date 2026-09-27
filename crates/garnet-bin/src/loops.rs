//! The background loops: health, settlement, equity snapshots.
//!
//! Each loop is a separate task with its own timer and a shared stop signal. The separation
//! is not cosmetic: the equity snapshot has to keep running while settlement is busy with a
//! long pass — drawdown is measured from equity, and a stale snapshot means a killswitch
//! that fires too late.
//!
//! The decisions these loops carry out do not live here: health is computed by
//! `garnet_risk::health`, what to do about it by `garnet_risk::feed_guard`, and the
//! settlement pass by `garnet_core::settle`. Here there is only timing and wiring.

use crate::app::{App, BookSource};
use crate::auto_payout::AutoPayout;
use garnet_config::Config;
use garnet_core::detect::MarketSource;
use garnet_core::equity::{snapshot_equity, PriceSource};
use garnet_core::execute::ClobExec;
use garnet_core::settle::{settle_once, Redeemer, SettleReport};
use garnet_db::{Db, Mode};
use garnet_feed::FeedClock;
use garnet_risk::feed_guard::{feed_guard, Guard};
use garnet_risk::health::{Health, HealthInput};
use garnet_risk::loss_stop::{loss_guard, LossAction};
use garnet_risk::manual_guard::{manual_guard, Manual};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How often health is computed.
///
/// More often than the thresholds, but not so often that the polling itself becomes load:
/// the thresholds are measured in minutes, and half a minute of lag against them means
/// nothing.
const HEALTH_TICK: Duration = Duration::from_secs(30);

/// The health loop: it computes the verdict and presses (or releases) the killswitch.
pub fn spawn_health<M, B, C>(
    app: Arc<App<M, B, C>>,
    db: Db,
    clock: FeedClock,
    alerts: Alerts,
    cfg: &Config,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()>
where
    M: MarketSource + Send + Sync + 'static,
    B: BookSource + Send + Sync + 'static,
    C: ClobExec + Send + Sync + 'static,
{
    let trade_threshold = chrono::Duration::seconds(cfg.feed.stall_threshold_secs);
    let control_threshold = chrono::Duration::seconds(cfg.feed.control_stall_secs);
    let loss_limit = cfg.risk.daily_loss_limit_usd;

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(HEALTH_TICK);
        let mut last_reported: Option<Health> = None;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let health = Health::evaluate(&HealthInput {
                        process_up: true,
                        socket_connected: true,
                        last_trade_age: to_chrono(clock.since_last_trade()),
                        control_topic_age: to_chrono(clock.since_last_frame()),
                        threshold: trade_threshold,
                        control_threshold,
                    });

                    // The state is printed only when it changes: a 30 s loop would otherwise
                    // drown the log in repeats.
                    if last_reported.as_ref() != Some(&health) {
                        println!("health: {}", health.reason());
                        last_reported = Some(health.clone());
                    }

                    match feed_guard(&health, at_risk(&db).await, app.killswitch_reason()) {
                        Guard::Trip(reason) => {
                            app.trip(reason);
                            let _ = garnet_core::reconcile::AlertSink::alert(
                                &alerts,
                                garnet_bus::subjects::ALERT_KILLSWITCH,
                                &format!("killswitch armed: {}", reason.as_str()),
                            )
                            .await;
                        }
                        Guard::Clear => {
                            app.clear_trip();
                            let _ = garnet_core::reconcile::AlertSink::alert(
                                &alerts,
                                garnet_bus::subjects::ALERT_KILLSWITCH,
                                "killswitch cleared: the feed returned",
                            )
                            .await;
                        }
                        Guard::Nothing => {}
                    }

                    // The operator's decision is applied last and overrides the automatics:
                    // the killswitch remembers one reason, and clearing a feed stop must not
                    // resume trading that a human halted. An unreachable database leaves the
                    // stop as it is — we will not silently resume live trading.

                    let stopped = match db.controls().manual_stop().await {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("the manual stop was not read: {e:#}");
                            continue;
                        }
                    };
                    // The loss stop is judged between the feed and the operator: it is
                    // automatic like the feed but latches for the day like a decision. It is
                    // computed from live: the killswitch stops only that (invariant 12), and
                    // a paper loss is beside the point.
                    match loss_action(&db, loss_limit, app.killswitch_reason()).await {
                        Ok(LossAction::Arm) => {
                            let day = chrono::Utc::now().date_naive();
                            if let Err(e) = db.controls().set_loss_stop_day(day, "core").await {
                                // The latch was not recorded — we arm the stop anyway: not
                                // stopping is worse than stopping and forgetting about it by
                                // the next restart.
                                eprintln!("the loss stop latch was not recorded: {e:#}");
                            }
                            app.trip(garnet_risk::killswitch::TripReason::LossLimit);
                            let _ = garnet_core::reconcile::AlertSink::alert(
                                &alerts,
                                garnet_bus::subjects::ALERT_KILLSWITCH,
                                &format!(
                                    "killswitch armed: the daily loss reached the limit of ${loss_limit}"
                                ),
                            )
                            .await;
                        }
                        Ok(LossAction::Keep) => {
                            app.trip(garnet_risk::killswitch::TripReason::LossLimit);
                        }
                        Ok(LossAction::Release) => {
                            app.clear_trip();
                            let _ = garnet_core::reconcile::AlertSink::alert(
                                &alerts,
                                garnet_bus::subjects::ALERT_KILLSWITCH,
                                "killswitch cleared: a new day",
                            )
                            .await;
                        }
                        Ok(LossAction::Nothing) => {}
                        Err(e) => eprintln!("the loss stop was not computed: {e:#}"),
                    }

                    match manual_guard(stopped, app.killswitch_reason()) {
                        Manual::Trip => {
                            app.trip(garnet_risk::killswitch::TripReason::Manual);
                            let _ = garnet_core::reconcile::AlertSink::alert(
                                &alerts,
                                garnet_bus::subjects::ALERT_KILLSWITCH,
                                "killswitch armed: halted by the operator",
                            )
                            .await;
                        }
                        Manual::Clear => {
                            app.clear_trip();
                            let _ = garnet_core::reconcile::AlertSink::alert(
                                &alerts,
                                garnet_bus::subjects::ALERT_KILLSWITCH,
                                "killswitch cleared by the operator",
                            )
                            .await;
                        }
                        Manual::Nothing => {}
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    })
}

/// Whether there is anything to lose: an enabled live wallet or an open live position.
///
/// The suppression rule from the predecessor: a platform-wide feed outage must not silence the
/// measuring instrument when there is nothing to lose — the killswitch is cleared by hand, and
/// a stop on an empty account would cost a day of shadow observations.
async fn at_risk(db: &Db) -> bool {
    let live_wallet = db
        .wallets()
        .list()
        .await
        .map(|ws| ws.iter().any(|w| w.enabled && w.mode == Mode::Live))
        .unwrap_or(false);
    if live_wallet {
        return true;
    }
    db.equity()
        .open_exposure(Mode::Live)
        .await
        .map(|rows| !rows.is_empty())
        .unwrap_or(false)
}

/// The settlement loop.
///
/// The redeemer is [`AutoPayout`]: on a proxy account with auto-payout enabled the platform
/// credits the winnings, and our job is to record them rather than send a transaction.
/// A settlement pass, announcing the outcomes on the bus.
///
/// A resolution is the only outcome of the trading path, and until now it was the only event
/// the bus stayed silent about: `settle_once` returned only counters, and the dashboard and
/// Telegram saw the entry without seeing how it ended.
/// The core deliberately knows nothing about the bus — `bin` publishes, on top of the rows
/// returned.
///
/// # Errors
///
/// The settlement pass failed. A publish is not an error: the bus is not a precondition of
/// trading and still less of accounting.
pub async fn settle_and_announce<M: MarketSource, R: Redeemer>(
    db: &Db,
    markets: &M,
    redeemer: &R,
    batch: i64,
    events: &crate::events::Events,
) -> anyhow::Result<SettleReport> {
    let report = settle_once(db, markets, redeemer, batch).await?;

    for s in &report.settled {
        events
            .emit(
                garnet_bus::subjects::POSITION_SETTLED,
                &serde_json::json!({
                    "position_id": s.position_id,
                    "wallet": s.wallet,
                    "token_id": s.token_id,
                    "mode": s.mode.as_str(),
                    "resolved_outcome": s.resolved_outcome,
                    "won": s.won,
                    "size": s.size.to_string(),
                    "payout_usd": s.payout_usd.to_string(),
                    "cost_usd": s.cost_usd.to_string(),
                    "proceeds_usd": s.proceeds_usd.to_string(),
                    "fees_usd": s.fees_usd.to_string(),
                    "pnl_usd": s.pnl_usd().to_string(),
                    "tx_hash": s.tx_hash,
                }),
            )
            .await;
    }

    Ok(report)
}

pub fn spawn_settlement<M>(
    db: Db,
    markets: M,
    cfg: &Config,
    events: crate::events::Events,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()>
where
    M: MarketSource + Send + Sync + 'static,
{
    let period = Duration::from_secs(cfg.settlement.hot_interval_secs);
    let batch = cfg.settlement.batch;

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    match settle_and_announce(&db, &markets, &AutoPayout, batch, &events).await {
                        Ok(r) => {
                            if r.resolved > 0 {
                                println!(
                                    "settlement: checked {}, resolved {}, closed {}",
                                    r.checked, r.resolved, r.settled.len()
                                );
                            }
                            // Invariant 13: an unread market does not vanish silently — a
                            // payout may already be sitting on it.
                            for (token, why) in &r.failed {
                                eprintln!("settlement: market {token} was not read: {why}");
                            }
                        }
                        Err(e) => eprintln!("settlement: {e:#}"),
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    })
}

/// An equity snapshot of both modes.
///
/// The live cash is read from the chain, shadow's from the virtual ledger. The two modes are
/// written by one task on purpose: comparing them makes sense only when the snapshots are
/// taken at the same moment.
pub fn spawn_equity<M, B, C, P>(
    app: Arc<App<M, B, C>>,
    db: Db,
    prices: P,
    live_cash: Option<Arc<ChainView>>,
    cfg: &Config,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()>
where
    M: MarketSource + Send + Sync + 'static,
    B: BookSource + Send + Sync + 'static,
    C: ClobExec + Send + Sync + 'static,
    P: PriceSource + Send + Sync + 'static,
{
    let period = Duration::from_secs(cfg.equity.snapshot_interval_secs);

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Some(cash) = &live_cash {
                        match cash.cash().await {
                            Ok(amount) => {
                                // The live cash also refreshes what `decide` measures the
                                // sufficiency of funds by.
                                app.set_live_cash(amount);
                                report("live", snapshot_equity(&db, Mode::Live, amount, &prices).await);
                            }
                            Err(e) => eprintln!("equity live: the balance was not read: {e:#}"),
                        }
                    }
                    match app.shadow_cash().await {
                        Ok(cash) => {
                            report("shadow", snapshot_equity(&db, Mode::Shadow, cash, &prices).await)
                        }
                        Err(e) => eprintln!("equity shadow: the account was not read: {e:#}"),
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    })
}

fn report(mode: &str, r: anyhow::Result<garnet_core::equity::Equity>) {
    match r {
        Ok(e) if e.unpriced > 0 => println!(
            "equity {mode}: ${} (positions without a price: {})",
            e.total_usd.round_dp(2),
            e.unpriced
        ),
        Ok(e) => println!("equity {mode}: ${}", e.total_usd.round_dp(2)),
        Err(e) => eprintln!("equity {mode}: {e:#}"),
    }
}

/// A look into the chain: free funds and outcome-token balances.
///
/// A type of its own so that the loops do not depend on the blockchain client directly and
/// build without keys.
pub struct ChainView {
    chain: Arc<garnet_blockchain::client::GarnetBlockchainClient>,
}

impl ChainView {
    #[must_use]
    pub fn new(chain: Arc<garnet_blockchain::client::GarnetBlockchainClient>) -> Self {
        Self { chain }
    }

    /// The trading collateral is pUSD: an order stands only on wrapped funds.
    pub async fn cash(&self) -> anyhow::Result<Decimal> {
        use garnet_blockchain::traits::BlockchainClient;
        Ok(self.chain.balance_pusd().await?)
    }
}

impl garnet_core::reconcile::ChainBalances for ChainView {
    async fn balance_of(&self, token_id: &str) -> anyhow::Result<Decimal> {
        use garnet_blockchain::traits::BlockchainClient;
        Ok(self.chain.balance_of_outcome(token_id).await?)
    }
}

/// Alerts to the operator: to the bus and to the log.
///
/// A divergence in the ledger is never fixed silently — only an alert (invariant 13).
/// It is always written to the log: the bus can be unreachable at precisely the moment the
/// alert matters most.
#[derive(Clone)]
pub struct Alerts {
    events: crate::events::Events,
}

impl Alerts {
    #[must_use]
    pub fn new(events: crate::events::Events) -> Self {
        Self { events }
    }
}

impl garnet_core::reconcile::AlertSink for Alerts {
    async fn alert(&self, subject: &str, text: &str) -> anyhow::Result<()> {
        eprintln!("ALERT {subject}: {text}");
        self.events
            .emit(subject, &serde_json::json!({ "text": text }))
            .await;
        Ok(())
    }
}

/// The reconciliation loop: our ledger against what the chain sees.
///
/// One day showed why it is needed: three fills stayed outside the ledger because the write
/// failed after the money had already been spent, and they had to be recovered by hand from
/// the exchange's feed.
pub fn spawn_reconcile<M>(
    db: Db,
    markets: M,
    chain: Arc<ChainView>,
    alerts: Alerts,
    cfg: &Config,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()>
where
    M: MarketSource + Send + Sync + 'static,
{
    let period = Duration::from_secs(cfg.reconcile.interval_secs);
    let window = cfg.reconcile.in_flight_window_secs;

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    match garnet_core::reconcile::reconcile_once(
                        &db, chain.as_ref(), &markets, &alerts, window,
                    ).await {
                        // The unreadable is printed too: "the check could not be performed"
                        // is not "agreed" (invariant 47), and silence about it would be
                        // indistinguishable from a successful reconciliation.
                        Ok(r) if !r.divergences.is_empty() || !r.unreadable.is_empty() => println!(
                            "reconciliation: checked {}, divergences {}, awaiting settlement {}, \
                             explained by an order {}, not read {}",
                            r.checked, r.divergences.len(), r.awaiting_settlement,
                            r.explained, r.unreadable.len()
                        ),
                        Ok(_) => {}
                        Err(e) => eprintln!("reconciliation: {e:#}"),
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    })
}

/// The liveness mark for the watcher (invariant 45).
///
/// Written more often than the watcher reads it: a mark updated less often than the check
/// produces a false alarm on every other one.
///
/// This is the only trace distinguishing "the leaders are quiet" from "there is nobody to
/// trade": the age of the trades answers the first question and not the second.
pub fn spawn_heartbeat(
    db: Db,
    period: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = db.controls().beat("garnet-core").await {
                        eprintln!("heartbeat: {e:#}");
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    })
}

/// The executor of the emergency close (invariant 46).
///
/// The gate is passed by whoever accepted the phrase — the bot or the CLI; this loop does the
/// selling, because only the trading process has the exchange and the book. The link goes
/// through the database rather than the bus, for the same reason as `/kill`: an emergency
/// close must work when NATS is unreachable.
///
/// The tick is short: this is an emergency path, and a minute of waiting here is a minute in
/// which the operator stares at an unchanged `/positions` and concludes the bot did not hear
/// them.
pub fn spawn_flatten_watch<M, B, C>(
    app: Arc<App<M, B, C>>,
    db: Db,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()>
where
    M: MarketSource + Send + Sync + 'static,
    B: BookSource + Send + Sync + 'static,
    C: ClobExec + Send + Sync + 'static,
{
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let approved = match db.controls().flatten_approved().await {
                        Ok(v) => v,
                        Err(e) => { eprintln!("emergency close: {e:#}"); continue; }
                    };
                    let Some((raw, actor)) = approved else { continue };
                    let Some(mode) = garnet_core::flatten::FlattenMode::parse(&raw) else {
                        eprintln!("emergency close: \"{raw}\" is not a mode, the approval has been cleared");
                        let _ = db.controls().clear_flatten_approved().await;
                        continue;
                    };

                    // The approval is cleared BEFORE the sale. A close that failed halfway
                    // must not start again from the beginning: a second attempt would sell
                    // what the first had already sold.
                    if let Err(e) = db.controls().clear_flatten_approved().await {
                        eprintln!("emergency close: the approval was not cleared ({e:#}) — not executing");
                        continue;
                    }

                    match app.flatten(mode, &actor).await {
                        Ok(r) => {
                            println!(
                                "emergency close {} by decision of {}: considered {}, sold {}, left {}",
                                mode.as_str(), actor, r.considered, r.sold, r.kept_to_live
                            );
                            for (token, why) in &r.failed {
                                println!("  NOT CLOSED {token}: {why}");
                            }
                        }
                        Err(e) => eprintln!("emergency close {}: {e:#}", mode.as_str()),
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    })
}

fn to_chrono(d: Duration) -> chrono::Duration {
    // `Duration::MAX` means "there were no frames at all"; it does not fit in chrono, and it
    // has to be turned into "a very long time ago" rather than into zero.
    chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::days(3650))
}

/// The safety-net `/activity` poll and the refresh of the wallet list.
///
/// Two jobs in one loop on purpose: both go for the same list, and separating them would mean
/// reading the registry twice. Assigning a wallet takes effect from the next tick, without a
/// restart of the process.
pub fn spawn_activity_poll<M, B, C>(
    app: Arc<App<M, B, C>>,
    db: Db,
    activity: crate::activity::Activity,
    cfg: &Config,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()>
where
    M: MarketSource + Send + Sync + 'static,
    B: BookSource + Send + Sync + 'static,
    C: ClobExec + Send + Sync + 'static,
{
    let period = Duration::from_secs(cfg.feed.poll_interval_secs);

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        let mut known = 0usize;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let wallets = match db.wallets().list().await {
                        Ok(ws) => ws,
                        Err(e) => {
                            eprintln!("poll: the wallet registry is unavailable: {e:#}");
                            continue;
                        }
                    };

                    let addresses: Vec<String> =
                        wallets.iter().map(|w| w.address.clone()).collect();
                    if addresses.len() != known {
                        println!("wallets under observation: {}", addresses.len());
                        known = addresses.len();
                    }
                    app.set_watched(addresses.clone());

                    for address in &addresses {
                        // The 20 most recent actions: the poll is a safety net, not a history
                        // — depth is covered by the socket.
                        match activity.recent(address, 20).await {
                            Ok(rows) => {
                                if let Err(e) = app.on_activity(&rows).await {
                                    eprintln!("poll {address}: {e:#}");
                                }
                            }
                            Err(e) => eprintln!("poll {address}: {e:#}"),
                        }
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    })
}

/// What to do about the loss stop right now.
///
/// The realised live result over the UTC day: closed positions, without revaluing open ones.
/// Revaluation depends on the mid of the book, which some positions do not have at all, and a
/// stop that fires because a price is missing is the worst kind of false alarm.
async fn loss_action(
    db: &Db,
    limit: rust_decimal::Decimal,
    tripped_by: Option<garnet_risk::killswitch::TripReason>,
) -> anyhow::Result<LossAction> {
    if limit <= rust_decimal::Decimal::ZERO {
        return Ok(LossAction::Nothing);
    }
    let now = chrono::Utc::now();
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map_or(now, |d| d.and_utc());
    let pnl = db
        .reports()
        .realised_pnl(Some(midnight))
        .await?
        .into_iter()
        .find(|r| r.mode == Mode::Live)
        .map_or(rust_decimal::Decimal::ZERO, |r| r.pnl_usd);
    let latched = db.controls().loss_stop_day().await?;
    Ok(loss_guard(
        pnl,
        limit,
        latched,
        now.date_naive(),
        tripped_by,
    ))
}

/// The time-based exit: once per period we sell positions that have sat around longer than
/// permitted.
///
/// A rule about capital, not about returns. On a prediction market the position will reach
/// zero or one anyway, and dumping into a thin book is usually worse than waiting; the point
/// is that money should not stand idle, not that we should lose less. Zero hours disables the
/// loop entirely — it does not even start.
pub fn spawn_stale_exit<M, B, C>(
    app: Arc<App<M, B, C>>,
    db: Db,
    max_hold_hours: i64,
    period: std::time::Duration,
    mut shutdown: watch::Receiver<bool>,
) -> Option<JoinHandle<()>>
where
    M: MarketSource + Send + Sync + 'static,
    B: BookSource + Send + Sync + 'static,
    C: ClobExec + Send + Sync + 'static,
{
    if max_hold_hours <= 0 {
        return None;
    }
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let stale = match db.positions().stale_open(max_hold_hours, 50).await {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("time-based exit: the positions were not read: {e:#}");
                            continue;
                        }
                    };
                    // A position past the limit that could not be sold stays open forever in
                    // silence: a delisted market has an empty book, and a resolution may be
                    // weeks away. One line per pass is not a refusal but a state: the 72-hour
                    // rule does not work on such positions, and that has to be visible
                    // without turning into a line on every tick.

                    let mut held = 0usize;
                    for pos in stale {
                        match app.close_stale(&pos).await {
                            Ok(true) => println!(
                                "time-based exit: {} closed", pos.token_id
                            ),
                            Ok(false) => held += 1,
                            Err(e) => eprintln!(
                                "time-based exit: {}: {e:#}", pos.token_id
                            ),
                        }
                    }
                    if held > 0 {
                        println!(
                            "time-based exit: {held} positions older than {max_hold_hours} h have nobody to sell to — awaiting settlement"
                        );
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    }))
}

/// The retry of a deferred exit.
///
/// A deferred fraction waited for the leader's next sale — and only for that. A leader who
/// exited in full sells no more: on 06.09.2026 a position sat with a deferred exit covering
/// its whole size and never repeated once, while next to it two closed positions carried ten
/// unexecuted shares each away with them. The loop retries the same order against a fresh
/// book; it contains no decision, so it records no refusals.
pub fn spawn_pending_exit<M, B, C>(
    app: Arc<App<M, B, C>>,
    db: Db,
    period: std::time::Duration,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()>
where
    M: MarketSource + Send + Sync + 'static,
    B: BookSource + Send + Sync + 'static,
    C: ClobExec + Send + Sync + 'static,
{
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let pending = match db.positions().pending_open(50).await {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("deferred exit: the positions were not read: {e:#}");
                            continue;
                        }
                    };
                    for pos in pending {
                        match app.retry_pending(&pos).await {
                            Ok(true) => println!(
                                "deferred exit: {} executed", pos.token_id
                            ),
                            Ok(false) => {}
                            Err(e) => eprintln!(
                                "deferred exit: {}: {e:#}", pos.token_id
                            ),
                        }
                    }
                }
                _ = shutdown.changed() => return,
            }
        }
    })
}
