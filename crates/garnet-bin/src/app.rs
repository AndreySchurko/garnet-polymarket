//! The wiring: RTDS frame -> leader trade -> decision -> execution -> position.
//!
//! Here and only here does time live: the other crates are pure functions and queries. The
//! order of the steps is fixed, because each one has to leave a trace even when the decision
//! is negative.

use garnet_config::Config;
use garnet_copy_engine::exit::{
    apply_exit, apply_exit_shadow, carried_fraction, carried_shares, exit_fraction, exit_request,
    ExitOutcome, ExitShape,
};
use garnet_copy_engine::{buy_limit, decide_with, Quote, SkipReason, Verdict};
use garnet_core::book::Book;
use garnet_core::detect::{Detector, MarketSource};
use garnet_core::execute::{order_size, submit_ioc, ClobExec, OrderOutcome, OrderRequest};
use garnet_core::market_meta::{snap_price_down, taker_fee, MarketMeta};
use garnet_core::shadow::{simulate_fill, Fill, FillSource};
use garnet_db::{Db, LeaderTrade, Mode, Side, Wallet};
use garnet_risk::killswitch::{Killswitch, TripReason};
use garnet_risk::metrics::Metrics;
use rust_decimal::Decimal;
use std::sync::Mutex;

/// The source of the order book.
pub trait BookSource {
    fn book(
        &self,
        token_id: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Book>> + Send;
}

/// Whether to respect the operator's stop when selling.
///
/// By default, yes: a sale is a live order whoever decided it (invariants 12, 23), and on
/// 05.09.2026 the stop did not touch it, that is, `/kill` halted entries and left exits
/// trading.
///
/// The only exception is the emergency close. `panic` is permitted **only** while trading is
/// halted (invariant 46), and a stop forbidding it to sell would make the mode impossible by
/// construction: the operator would halt trading, type the phrase and sell nothing. The
/// phrase is stronger than the automatic stop here, because it is spoken after it and about
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Honor,
    Override,
}

/// What the emergency close did.
#[derive(Debug, Clone, Default)]
pub struct FlattenReport {
    pub mode: Option<garnet_core::flatten::FlattenMode>,
    pub considered: usize,
    pub sold: usize,
    /// `hybrid`: the position is not in profit — the operator allowed taking a profit, not
    /// locking in a loss.
    pub kept_losing: usize,
    /// `hybrid`: there is nothing to measure whether it is in profit with.
    pub kept_unmeasurable: usize,
    /// Left to live out: all of `graceful` plus resolved markets.
    pub kept_to_live: usize,
    /// What could not be closed. The operator asked to close everything and has to learn that
    /// not everything closed.
    pub failed: Vec<(String, String)>,
}

/// The difference between timestamps, in seconds. A negative one is returned as zero: the
/// database's clock and the exchange's drift apart, and a negative latency in a histogram is
/// not "faster than instantaneous" but a clock mismatch, which has no place in the
/// distribution.
fn secs_between(from: chrono::DateTime<chrono::Utc>, to: chrono::DateTime<chrono::Utc>) -> f64 {
    ((to - from).num_milliseconds().max(0) as f64) / 1000.0
}

/// The entry queue's key: the wallet and the mode. Paper and real are different pools of
/// capital, and one's queue must not shut the door on the other's entry.
fn fire_key(wallet: &str, mode: Mode) -> String {
    format!("{wallet}:{}", mode.as_str())
}

pub struct App<M: MarketSource, B: BookSource, C: ClobExec> {
    events: crate::events::Events,
    pub db: Db,
    pub detector: Detector<M>,
    pub books: B,
    pub clob: C,
    pub metrics: Metrics,
    /// The paper mode's starting capital. The account itself lives in the database: state in
    /// the process does not survive a restart.
    shadow_initial: Decimal,
    killswitch: Mutex<Killswitch>,
    /// Live free funds. The value is supplied from outside and read from the chain once the
    /// blockchain client is wired in.
    live_cash: Mutex<Decimal>,
    /// The window for collapsing slices of one leader order, in seconds. Zero means no
    /// collapsing.
    slice_window_secs: i64,
    /// The ceiling on open exposure per event. Zero means no ceiling.
    market_cap_usd: Decimal,
    /// The entry rate limit per wallet (invariant 44). Asked **last** of all the refusal
    /// reasons — see `handle_buy`.
    fire: garnet_risk::fire_rate::FireRate,
}

impl<M: MarketSource, B: BookSource, C: ClobExec> App<M, B, C> {
    pub fn new(
        db: Db,
        detector: Detector<M>,
        books: B,
        clob: C,
        cfg: &Config,
        events: crate::events::Events,
    ) -> Self {
        Self {
            db,
            detector,
            books,
            clob,
            metrics: Metrics::new(),
            shadow_initial: cfg.shadow.initial_capital_usd,
            killswitch: Mutex::new(Killswitch::default()),
            live_cash: Mutex::new(Decimal::ZERO),
            slice_window_secs: cfg.copy.slice_window_secs,
            market_cap_usd: cfg.risk.per_market_cap_usd,
            fire: garnet_risk::fire_rate::FireRate::new(
                cfg.risk.fire_window_secs,
                cfg.risk.fire_limit,
            ),
            events,
        }
    }

    /// An event to the bus. A failed publish does not interrupt trading: the bus is an
    /// addressee for observers, not a participant in a trade.
    async fn emit(&self, subject: &str, payload: serde_json::Value) {
        self.events.emit(subject, &payload).await;
    }

    /// The executor — for diagnostics and readiness checks.
    pub fn clob(&self) -> &C {
        &self.clob
    }

    pub fn set_live_cash(&self, amount: Decimal) {
        *self.live_cash.lock().unwrap() = amount;
    }

    /// The paper mode's virtual account — from the database, not from memory.
    ///
    /// It also accounts for resolution payouts, which the in-memory account never saw at all:
    /// that one changed only on fills.
    pub async fn shadow_cash(&self) -> anyhow::Result<Decimal> {
        self.db.equity().shadow_cash(self.shadow_initial).await
    }

    /// What the killswitch is armed by right now. Read by the health loop.
    pub fn killswitch_reason(&self) -> Option<TripReason> {
        self.killswitch.lock().unwrap().reason()
    }

    /// Clear the stop. The decision about **whose** stop may be cleared is made by
    /// `feed_guard`: the return of the feed does not cancel the operator's manual stop.
    pub fn clear_trip(&self) {
        self.killswitch.lock().unwrap().reset();
    }

    pub fn trip(&self, reason: TripReason) {
        self.killswitch.lock().unwrap().trip(reason);
    }

    /// A frame from the socket: parsed, deduplicated and carried through to a position.
    pub async fn on_frame(&self, frame: &serde_json::Value) -> anyhow::Result<()> {
        let got = self.detector.on_frame(frame).await?;
        for act in &got.actions {
            self.handle_action(act).await?;
        }
        for trade in got.trades {
            self.metrics.observe(
                "trade_to_seen",
                (trade.ts_seen - trade.ts_trade).num_milliseconds() as f64 / 1000.0,
            );
            self.handle(trade).await?;
        }
        Ok(())
    }

    /// A fill lifted from a Polygon log (the third circuit, invariant 41).
    ///
    /// `ts_trade` comes **from the block**, not from our clock: the slice window closes by it
    /// (invariant 29), and substituting our own time would turn it into a measurement of our
    /// own latency.
    pub async fn on_chain_fill(
        &self,
        fill: &garnet_feed::chain::ChainFill,
        ts_trade: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()> {
        let raw = garnet_core::detect::RawTrade {
            wallet: fill.maker.clone(),
            tx_hash: fill.tx_hash.clone(),
            token_id: fill.token_id.clone(),
            side: match fill.side {
                garnet_feed::chain::Side::Buy => garnet_db::Side::Buy,
                garnet_feed::chain::Side::Sell => garnet_db::Side::Sell,
            },
            price: fill.price,
            size: fill.size,
            ts_trade,
        };
        let got = self
            .detector
            .ingest_chain(vec![garnet_core::detect::LeaderAction::Trade(raw)])
            .await?;
        for trade in got.trades {
            self.metrics.observe(
                "trade_to_seen",
                (trade.ts_seen - trade.ts_trade).num_milliseconds() as f64 / 1000.0,
            );
            self.handle(trade).await?;
        }
        Ok(())
    }

    /// Replace the list of watched wallets: the operator assigns them on the fly.
    pub fn set_watched(&self, wallets: impl IntoIterator<Item = String>) {
        self.detector.set_watched(wallets);
    }

    /// Rows from the safety-net `/activity` poll.
    ///
    /// The same path as a socket frame: parsing, dedup, decision, order. The difference
    /// between the circuits ends at the means of delivery — otherwise the bot would trade
    /// differently in an incident than it does normally.
    pub async fn on_activity(&self, rows: &serde_json::Value) -> anyhow::Result<()> {
        let parsed = garnet_core::detect::parse_activity(rows);
        let got = self.detector.ingest_poll(parsed).await?;
        for act in &got.actions {
            self.handle_action(act).await?;
        }
        for trade in got.trades {
            self.metrics.observe(
                "trade_to_seen",
                (trade.ts_seen - trade.ts_trade).num_milliseconds() as f64 / 1000.0,
            );
            self.handle(trade).await?;
        }
        Ok(())
    }

    /// A leader action that is not a trade (invariant 40).
    ///
    /// It has already been recorded by the detector — here it is decided what to do with it.
    /// The absence of a branch for a split and a redeem is not an oversight: both
    /// **deliberately do nothing**, and the row in the registry is their entire effect.
    async fn handle_action(&self, act: &garnet_db::LeaderActionRow) -> anyhow::Result<()> {
        let Some(wallet) = self.db.wallets().get(&act.wallet).await? else {
            return Ok(());
        };

        // The same assignment threshold as for trades, and for the same reason:
        // `/activity?limit=20` on a quiet wallet returns weeks of history, and on the first
        // tick all of it looks like news. A month-old merge acted upon today would sell a
        // position that did not exist back then.
        let watched_since = chrono::DateTime::from_timestamp(wallet.created_at.timestamp(), 0)
            .unwrap_or(wallet.created_at);
        if act.ts_action < watched_since {
            return Ok(());
        }

        match act.kind.as_str() {
            "merge" => self.handle_leader_merge(act, &wallet).await,
            // A split is an entry into both legs at once. Whether it is copied is **a
            // decision of the operator, not a refactor**: a split buys a pair for $1, that
            // is, it is not a bet on an outcome but a placement of capital, and copying it
            // with a `stake_usd` stake would mean betting twice. It is not copied by
            // default; the row in `leader_actions` is there, and that is enough for the
            // decision to be made from data.
            //
            // A leader's redemption does not touch our position at all: our own settlement
            // closes it, with its own source of truth — `tokens[].winner`, not somebody
            // else's transaction.
            "split" | "redeem" => Ok(()),
            _ => Ok(()),
        }
    }

    /// The leader merged a pair — that is, exited at $1 **on both legs**.
    ///
    /// We sell the same fraction of our position on each leg as they merged of theirs. The
    /// price comes from the best bid: there is no leader's price here, a merge is not traded
    /// at a price (invariant 34).
    ///
    /// We cannot merge ourselves yet (that requires a path through the Safe, which does not
    /// exist), so both legs are sold. When it arrives, a pair with both legs has to be closed
    /// by a merge rather than by two sales: half a pair is directional risk where there was
    /// none (invariant 38).
    async fn handle_leader_merge(
        &self,
        act: &garnet_db::LeaderActionRow,
        wallet: &Wallet,
    ) -> anyhow::Result<()> {
        let legs = self
            .db
            .positions()
            .open_in_condition(&act.wallet, &act.condition_id, wallet.mode)
            .await?;
        if legs.is_empty() {
            // The leader merged something we do not hold. We mark it handled: there is
            // nothing to wait for, and an unhandled merge is an alarm for the watcher, which
            // must not be lied to in either direction.
            self.db.actions().mark_handled(act.id).await?;
            return Ok(());
        }

        let mut touched = 0usize;
        for pos in &legs {
            let meta = self.detector.market(&pos.token_id).await?;
            if meta.closed || meta.resolved_outcome.is_some() {
                // The market resolved — settlement will return the money, there is nothing
                // to sell.
                continue;
            }
            let book = self.books.book(&pos.token_id).await?;
            // The fraction is computed against what we saw of theirs: buys before the
            // assignment are by construction invisible to us (invariant 25).
            let fraction = exit_fraction(act.size, pos.leader_observed_size);
            if self
                .exit_at_bid(
                    pos,
                    wallet,
                    &meta,
                    &book,
                    fraction,
                    "a leader merge",
                    Stop::Honor,
                )
                .await?
            {
                touched += 1;
            }
        }

        // A merge counts as handled once we have exited at least one leg. A merge seen with
        // an empty book or under a stop deliberately stays unhandled: that is exactly the
        // case the table has `handled_at` for.

        if touched > 0 {
            self.db.actions().mark_handled(act.id).await?;
        }
        Ok(())
    }

    async fn handle(&self, trade: LeaderTrade) -> anyhow::Result<()> {
        let Some(wallet) = self.db.wallets().get(&trade.wallet).await? else {
            return Ok(());
        };

        // The operator asks us to copy what the wallet does **from the moment of
        // assignment**, not what it did before. `/activity?limit=20` on a quiet wallet
        // returns weeks of history, and on the first tick all of it looks like news: on
        // 05.09.2026 that is how 69 `market_not_tradable` refusals and 28 copies of trades
        // up to 3.4 days old were born — one of them filled at 0.001 against the leader's
        // 0.260.
        //
        // This is not a sixth skip reason: there is no skipped signal here, because there is
        // no signal at all. The trade is already recorded, though — otherwise the dedup
        // would forget it and the next tick would bring the same history again.
        //
        // The leader's observed position is not tracked either: exits compute a fraction of
        // what we saw, and buys before the assignment are by construction invisible.
        // The comparison is by the second: RTDS timestamps trades to the second while
        // `created_at` is stored with microseconds, and comparing different resolutions would
        // discard the first second of observation — precisely the moment a wallet is added
        // for.
        let watched_since = chrono::DateTime::from_timestamp(wallet.created_at.timestamp(), 0)
            .unwrap_or(wallet.created_at);
        if trade.ts_trade < watched_since {
            self.metrics.incr("trades_before_watch_total", &[]);
            return Ok(());
        }

        // Invariant 42: on the hot path there are no sequential waits that could run in
        // parallel. `book` does not use `meta` — they waited for each other for no reason,
        // and that was an extra network round trip added to the latency the whole point of
        // copying is measured by.
        let (meta, book) = tokio::try_join!(
            self.detector.market(&trade.token_id),
            self.books.book(&trade.token_id),
        )?;

        match trade.side.as_str() {
            "buy" => self.handle_buy(&trade, &wallet, &meta, &book).await,
            "sell" => self.handle_sell(&trade, &wallet, &meta, &book).await,
            _ => Ok(()),
        }
    }

    async fn handle_buy(
        &self,
        trade: &LeaderTrade,
        wallet: &Wallet,
        meta: &MarketMeta,
        book: &Book,
    ) -> anyhow::Result<()> {
        // Invariant 42. The four database calls below are independent of each other, and
        // until 19.09.2026 they waited in turn — on a path where milliseconds are counted.
        // The leader's observation is written, the other three read, and none uses another's
        // result.
        //
        // The order of writes does not change: `signals` is written below, after all four,
        // and `orders` after `signals`.
        let observe = async {
            self.db
                .positions()
                .observe_leader(&wallet.address, &trade.token_id, wallet.mode, trade.size)
                .await
        };
        // No price is substituted: `unwrap_or(1.0)` made an empty book indistinguishable from
        // real slippage, and the entire statistic of price refusals was a mixture of two
        // different outcomes.
        let best_ask = book.best_ask();
        let best_bid = book.best_bid();
        let quote = Quote {
            leader_price: trade.price,
            best_ask,
        };

        // One leader order arrives as as many frames as the reserves it consumed in the book,
        // and each has its own `tx_hash` — the dedup does not catch them. The wave closes by
        // the leader's clock: our clock would measure delivery latency rather than their
        // behaviour.
        let wave = async {
            if self.slice_window_secs == 0 {
                return Ok(None);
            }
            self.db
                .signals()
                .last_copied_buy(&wallet.address, &trade.token_id, wallet.mode)
                .await
        };

        // Open exposure is computed per event rather than per token: the outcomes of one
        // condition are correlated, and a per-token ceiling is bypassed by buying the
        // neighbouring outcome. The query runs only when the ceiling is enabled — otherwise
        // it is an extra trip to the database on the signal path, where we count
        // milliseconds.
        let exposure = async {
            if self.market_cap_usd <= Decimal::ZERO {
                return Ok(Decimal::ZERO);
            }
            self.db
                .positions()
                .open_cost_in_condition(&meta.condition_id, wallet.mode)
                .await
        };

        let (_, prev_buy, market_open) = tokio::try_join!(observe, wave, exposure)?;
        let already_in_wave = prev_buy
            .is_some_and(|prev| (trade.ts_trade - prev).num_seconds() <= self.slice_window_secs);

        let balance = match wallet.mode {
            Mode::Live => *self.live_cash.lock().unwrap(),
            Mode::Shadow => self.shadow_cash().await?,
        };

        let verdict = decide_with(
            wallet,
            meta,
            quote,
            balance,
            already_in_wave,
            market_open,
            self.market_cap_usd,
        );

        // The rate limit is asked LAST, and only of what would otherwise become an entry.
        // The reason is the same as for judging the ceiling last: an order that would not
        // have been taken on price or on funds anyway must neither report itself as refused
        // by the rate limit nor take a place in the queue — otherwise the queue would consist
        // of refusals.
        //
        // The clock is OURS, not the leader's (invariant 44): what is limited here is our own
        // rate of spending capital rather than an interpretation of their behaviour — that is
        // the difference from the slice window, which is measured by their clock.
        //
        // The mode is part of the key: paper and real are different pools of capital, and
        // one's queue must not shut the door on the other's entry. But the rule applies to
        // both, otherwise shadow would be measuring a different strategy from the one live
        // trades.
        let verdict = match verdict {
            Verdict::Copy { .. }
                if self.fire.record(
                    &fire_key(&wallet.address, wallet.mode),
                    chrono::Utc::now().timestamp(),
                ) =>
            {
                Verdict::Skip(SkipReason::RateLimited)
            }
            v => v,
        };

        self.metrics
            .incr("signals_total", &[("verdict", verdict.as_str())]);

        // The ceiling is recorded even where it fired as a refusal: the miss is
        // `best_ask - limit_price`, and without both halves there is nothing to tune the
        // slippage threshold from. On other refusals the ceiling was not the constraint, and
        // recording it would mean inventing a reason.
        let (target, limit) = match verdict {
            Verdict::Copy {
                size_usd,
                limit_price,
            } => (size_usd, Some(limit_price)),
            Verdict::Skip(SkipReason::SlippageExceeded) => (
                Decimal::ZERO,
                Some(buy_limit(trade.price, wallet.max_slippage_pct)),
            ),
            Verdict::Skip(_) => (Decimal::ZERO, None),
        };

        let signal = self
            .db
            .signals()
            .record(
                trade.id,
                &wallet.address,
                wallet.mode,
                verdict.as_str(),
                target,
                limit,
                best_ask,
                best_bid,
            )
            .await?;

        // Stage 2 of four: from the moment the trade reached us to the decision taken and
        // recorded. Until 19.09.2026 it was declared in `STAGES` and never written — that is,
        // the hot path that the work set out to speed up had nothing to measure it with.

        self.metrics.observe(
            "seen_to_signal",
            secs_between(trade.ts_seen, signal.ts_signal),
        );

        self.emit(
            garnet_bus::subjects::SIGNAL_DETECTED,
            serde_json::json!({
                "wallet": wallet.address,
                "nickname": wallet.display(),
                "mode": wallet.mode.as_str(),
                "verdict": verdict.as_str(),
                "token_id": trade.token_id,
                "market": meta.question,
                "outcome": meta.outcome_label,
                "leader_price": trade.price.to_string(),
                "target_size_usd": target.to_string(),
            }),
        )
        .await;

        let Verdict::Copy {
            size_usd,
            limit_price,
        } = verdict
        else {
            return Ok(());
        };

        // The price has to lie on the market's grid: the SDK's builder rejects everything
        // else locally, and the order never reaches the exchange at all. Downwards, so as not
        // to pay more than the slippage the operator permitted.
        let limit_price = snap_price_down(limit_price, meta.tick);
        // The shape of the order is three exchange constraints at once: the lot, the minimum
        // notional and the market's minimum. An impossible order is not sent but leaves a row
        // with a reason: silence in place of a reason reads as a breakage.
        let size_shares = match order_size(size_usd, limit_price, meta.min_order_size) {
            Ok(size) => size,
            Err(reason) => {
                self.db
                    .signals()
                    .record_order(
                        Some(signal.id),
                        &trade.token_id,
                        wallet.mode,
                        "buy",
                        limit_price,
                        size_usd,
                        "rejected",
                        Some(&reason),
                        0,
                    )
                    .await?;
                return Ok(());
            }
        };

        // The killswitch is an operator's stop, not a refusal reason: the signal stays `copy`,
        // and the execution that did not happen is visible in the order.
        let blocked = wallet.mode == Mode::Live && {
            let k = self.killswitch.lock().unwrap();
            k.is_live_blocked()
        };
        if blocked {
            let reason = self
                .killswitch
                .lock()
                .unwrap()
                .reason()
                .map_or("killswitch", TripReason::as_str)
                .to_string();
            self.db
                .signals()
                .record_order(
                    Some(signal.id),
                    &trade.token_id,
                    wallet.mode,
                    "buy",
                    limit_price,
                    size_usd,
                    "rejected",
                    Some(&format!("killswitch: {reason}")),
                    0,
                )
                .await?;
            return Ok(());
        }

        let (fill, status, error) = match wallet.mode {
            Mode::Shadow => {
                let f = simulate_fill(book, size_usd, limit_price, &meta.fee);
                let status = if f.is_empty() { "rejected" } else { "filled" };
                (Some(f), status, None)
            }
            Mode::Live => {
                let req = OrderRequest {
                    token_id: trade.token_id.clone(),
                    side: Side::Buy,
                    limit_price,
                    size_shares,
                    neg_risk: meta.neg_risk,
                };
                match submit_ioc(&self.clob, &req).await {
                    OrderOutcome::Filled(f) => (Some(self.priced(f, meta)), "filled", None),
                    OrderOutcome::Partial(f) => (Some(self.priced(f, meta)), "partial", None),
                    OrderOutcome::Rejected(e) => (None, "rejected", Some(e)),
                    // The position may have been taken on without being visible to us: there
                    // is nothing to book, and silence is not allowed — the reconciler
                    // resolves the divergence.
                    OrderOutcome::Unknown(e) => {
                        self.metrics.incr("orders_unknown_total", &[]);
                        (None, "unknown", Some(e))
                    }
                }
            }
        };

        let order = self
            .db
            .signals()
            .record_order(
                Some(signal.id),
                &trade.token_id,
                wallet.mode,
                "buy",
                limit_price,
                size_usd,
                status,
                error.as_deref(),
                1,
            )
            .await?;

        // Stage 3 of four: from the decision to the submitted order. The second half of what
        // the work speeds up, and until 19.09.2026 it was not written either. It is measured
        // by the order's timestamp rather than by our clock at this line: `ts_submitted` is
        // what anyone who opens the registry later will see, and a divergence between them is
        // news in itself.
        self.metrics.observe(
            "signal_to_submitted",
            secs_between(signal.ts_signal, order.ts_submitted),
        );

        self.emit(
            garnet_bus::subjects::ORDER_SUBMITTED,
            serde_json::json!({
                "order_id": order.id,
                "wallet": wallet.address,
                "mode": wallet.mode.as_str(),
                "side": "buy",
                "token_id": trade.token_id,
                "limit_price": limit_price.to_string(),
                "size_usd": size_usd.to_string(),
                "status": status,
                "error": error,
            }),
        )
        .await;

        if status == "rejected" {
            self.emit(
                garnet_bus::subjects::ALERT_ORDER_REJECTED,
                serde_json::json!({
                    "order_id": order.id,
                    "token_id": trade.token_id,
                    "mode": wallet.mode.as_str(),
                    "reason": error,
                }),
            )
            .await;
        }

        if let Some(f) = fill.filter(|f| !f.is_empty()) {
            self.book_fill(
                order.id,
                &wallet.address,
                wallet.mode,
                &trade.token_id,
                &f,
                true,
            )
            .await?;
            self.emit(
                garnet_bus::subjects::ORDER_FILLED,
                serde_json::json!({
                    "order_id": order.id,
                    "wallet": wallet.address,
                    "mode": wallet.mode.as_str(),
                    "side": "buy",
                    "token_id": trade.token_id,
                    "size": f.size.to_string(),
                    "avg_price": f.avg_price.to_string(),
                    "notional": f.notional.to_string(),
                    "fee_usd": f.fee_usd.to_string(),
                }),
            )
            .await;
        }
        Ok(())
    }

    /// The emergency close (invariant 46).
    ///
    /// The gate is checked **before** the call: what arrives here is an intent already
    /// approved. The separation is not cosmetic — checking the phrase is pure and is verified
    /// by tests without a network, while the selling is networked and is tested with stubs.
    ///
    /// It touches **live positions only**. Paper ones are not money: there is nothing to save
    /// in them, and closing them would destroy the only comparison shadow exists for.
    ///
    /// We have no merge of our own, so a YES+NO pair is closed by two sales. When merging
    /// arrives, it has to be closed by a merge: two orders can each fill halfway, and half a
    /// pair is directional risk where there was none (invariant 38).
    pub async fn flatten(
        &self,
        mode: garnet_core::flatten::FlattenMode,
        actor: &str,
    ) -> anyhow::Result<FlattenReport> {
        use garnet_core::flatten::{in_profit, FlattenMode};

        let mut report = FlattenReport {
            mode: Some(mode),
            ..FlattenReport::default()
        };

        // Every mode halts trading. Closing positions while continuing to buy new ones is not
        // a close but a swap; for `panic` the stop is already in place (the gate requires it),
        // for the others it is set here.
        self.db.controls().set_manual_stop(true, actor).await?;
        {
            let mut k = self.killswitch.lock().unwrap();
            k.trip(TripReason::Manual);
        }

        if mode == FlattenMode::Graceful {
            // We sell nothing — we count how many were left to live out.
            report.considered = self.db.equity().open_exposure(Mode::Live).await?.len();
            report.kept_to_live = report.considered;
            return Ok(report);
        }

        for pos in self.db.positions().open_live().await? {
            report.considered += 1;
            let Some(wallet) = self.db.wallets().get(&pos.wallet).await? else {
                report.failed.push((
                    pos.token_id.clone(),
                    "the wallet is not in the registry".into(),
                ));
                continue;
            };
            let meta = match self.detector.market(&pos.token_id).await {
                Ok(m) => m,
                Err(e) => {
                    report
                        .failed
                        .push((pos.token_id.clone(), format!("metadata: {e}")));
                    continue;
                }
            };
            if meta.closed || meta.resolved_outcome.is_some() {
                // The market resolved: settlement will return the money, there is nothing to
                // sell.
                report.kept_to_live += 1;
                continue;
            }
            let book = match self.books.book(&pos.token_id).await {
                Ok(b) => b,
                Err(e) => {
                    report
                        .failed
                        .push((pos.token_id.clone(), format!("the book: {e}")));
                    continue;
                }
            };

            if mode == FlattenMode::Hybrid {
                match in_profit(pos.cost_usd, pos.fees_usd, pos.size_bought, book.best_bid()) {
                    Some(true) => {}
                    Some(false) => {
                        report.kept_losing += 1;
                        continue;
                    }
                    // Selling something whose profitability is unknown means going beyond
                    // what the operator agreed to: they allowed taking a profit, not selling
                    // blind (invariant 27).
                    None => {
                        report.kept_unmeasurable += 1;
                        continue;
                    }
                }
            }

            match self
                .exit_at_bid(
                    &pos,
                    &wallet,
                    &meta,
                    &book,
                    Decimal::ONE,
                    &format!("emergency close: {}", mode.as_str()),
                    Stop::Override,
                )
                .await
            {
                Ok(true) => report.sold += 1,
                // Failing to sell is not a failure of the mechanism but a state of the market,
                // and the position stays ours. Staying silent about it is not allowed: the
                // operator asked to close everything and has to learn that not everything
                // closed.
                Ok(false) => report.failed.push((
                    pos.token_id.clone(),
                    "the sale did not go through: see the order registry".into(),
                )),
                Err(e) => report.failed.push((pos.token_id.clone(), format!("{e:#}"))),
            }
        }

        Ok(report)
    }

    /// Close a stale position: gather the wallet, the metadata and the book.
    ///
    /// Separate from [`exit_stale`](Self::exit_stale), which already receives all of it: the
    /// loop must know nothing about the network, and the exit tests must not go to the API for
    /// metadata.
    pub async fn close_stale(&self, pos: &garnet_db::Position) -> anyhow::Result<bool> {
        let Some(wallet) = self.db.wallets().get(&pos.wallet).await? else {
            return Ok(false);
        };
        // A disabled wallet stops buying, but its position stays ours: abandoning it open
        // would mean punishing it for being disabled.
        let meta = self.detector.market(&pos.token_id).await?;
        if meta.closed || meta.resolved_outcome.is_some() {
            // The market resolved — settlement will return the money, there is nothing to
            // sell.
            return Ok(false);
        }
        let book = self.books.book(&pos.token_id).await?;
        self.exit_stale(pos, &wallet, &meta, &book).await
    }

    /// The time-based exit: sell a stale position into the book.
    ///
    /// The decision is ours, the leader plays no part — which is why the order has no
    /// `signal_id` rather than an invented one. The price is anchored to the best bid: there
    /// is no leader's price to anchor to here, and taking it from an old trade would mean
    /// aiming at a price that left the market long ago.
    ///
    /// The killswitch stops this exit too (invariants 12, 23): a sale is a live order whoever
    /// decided it.
    pub async fn exit_stale(
        &self,
        pos: &garnet_db::Position,
        wallet: &Wallet,
        meta: &MarketMeta,
        book: &Book,
    ) -> anyhow::Result<bool> {
        self.exit_at_bid(
            pos,
            wallet,
            meta,
            book,
            Decimal::ONE,
            "a time-based exit",
            Stop::Honor,
        )
        .await
    }

    /// An exit at the best bid for a given fraction of the position.
    ///
    /// The shared path for two decisions that have no leader's price: the time-based exit
    /// (the fraction is the whole position) and a leader merge (the fraction is the one they
    /// merged). Neither has a leader's price to anchor to, and taking it from an old trade
    /// would mean aiming at a price that left the market long ago (invariant 34). The order's
    /// `signal_id` is `None` rather than invented.
    #[allow(clippy::too_many_arguments)]
    pub async fn exit_at_bid(
        &self,
        pos: &garnet_db::Position,
        wallet: &Wallet,
        meta: &MarketMeta,
        book: &Book,
        fraction: Decimal,
        label: &str,
        stop: Stop,
    ) -> anyhow::Result<bool> {
        // The mode belongs to the position, not to the wallet. A wallet is switched between
        // shadow and live while holding open positions, and the mode is part of the position's
        // key: a paper position must not be sold on the exchange merely because its wallet
        // has since been moved to live. Decisions about new entries still follow the wallet's
        // mode — there the mode is the decision.
        let mode = pos.mode;
        let blocked = stop == Stop::Honor && mode == Mode::Live && {
            let k = self.killswitch.lock().unwrap();
            k.is_live_blocked()
        };
        if blocked {
            return Ok(false);
        }
        // No bid means there is nobody to sell to. Silently: this is a state of the market,
        // not a refusal, and an order row on every tick would clutter the order registry.
        let Some(bid) = book.best_bid() else {
            return Ok(false);
        };

        let outcome = match mode {
            Mode::Shadow => apply_exit_shadow(
                book,
                pos,
                fraction,
                bid,
                wallet.max_slippage_pct,
                meta.tick,
                &meta.fee,
            ),
            Mode::Live => {
                apply_exit(
                    &self.clob,
                    pos,
                    fraction,
                    bid,
                    wallet.max_slippage_pct,
                    meta.neg_risk,
                    meta.tick,
                )
                .await
            }
        };

        match outcome {
            ExitOutcome::Sold(f) => {
                let f = self.priced(f, meta);
                let order = self
                    .db
                    .signals()
                    .record_order(
                        None,
                        &pos.token_id,
                        mode,
                        "sell",
                        f.avg_price,
                        f.notional,
                        "filled",
                        Some(label),
                        1,
                    )
                    .await?;
                self.book_fill(order.id, &wallet.address, mode, &pos.token_id, &f, false)
                    .await?;
                self.emit(
                    garnet_bus::subjects::ORDER_FILLED,
                    serde_json::json!({
                        "order_id": order.id,
                        "wallet": wallet.address,
                        "mode": mode.as_str(),
                        "side": "sell",
                        "reason": label,
                        "token_id": pos.token_id,
                        "size": f.size.to_string(),
                        "avg_price": f.avg_price.to_string(),
                        "notional": f.notional.to_string(),
                    }),
                )
                .await;
                Ok(true)
            }
            ExitOutcome::HeldToResolution { reason } => {
                self.db
                    .signals()
                    .record_order(
                        None,
                        &pos.token_id,
                        mode,
                        "sell",
                        Decimal::ZERO,
                        Decimal::ZERO,
                        "rejected",
                        Some(&format!("{label}: {reason}")),
                        0,
                    )
                    .await?;
                Ok(false)
            }
            // The exchange accepted the order, the outcome is unknown: we neither touch the
            // position nor retry (invariant 16), the reconciler resolves the divergence.
            ExitOutcome::Unknown {
                reason,
                size_shares,
                limit_price,
            } => {
                self.db
                    .signals()
                    .record_order(
                        None,
                        &pos.token_id,
                        mode,
                        "sell",
                        limit_price,
                        size_shares * limit_price,
                        "unknown",
                        Some(&format!("{label}: {reason}")),
                        1,
                    )
                    .await?;
                Ok(false)
            }
            ExitOutcome::Nothing => Ok(false),
        }
    }

    async fn handle_sell(
        &self,
        trade: &LeaderTrade,
        wallet: &Wallet,
        meta: &MarketMeta,
        book: &Book,
    ) -> anyhow::Result<()> {
        let Some(pos) = self
            .db
            .positions()
            .get(&wallet.address, &trade.token_id, wallet.mode)
            .await?
        else {
            return Ok(()); // the leader is selling something we never bought
        };

        // The leader's fraction adds to what was deferred earlier. They trim the position by
        // percentages, our share comes out in cents, and every such sale was refused by the
        // exchange's $1 minimum — 63% of exit attempts at a $25 stake, and at $10 it would
        // have been almost a hundred. A refusal on each means we copy the leader's entry and
        // do not copy their exit, that is, we trade a different strategy from the one we
        // measure.
        let leader_fraction = exit_fraction(trade.size, pos.leader_observed_size);
        let want_shares = carried_shares(&pos, leader_fraction, pos.pending_exit_shares);
        let fraction = carried_fraction(&pos, want_shares);
        let signal = self
            .db
            .signals()
            .record(
                trade.id,
                &wallet.address,
                wallet.mode,
                "copy",
                Decimal::ZERO,
                None,
                book.best_ask(),
                book.best_bid(),
            )
            .await?;

        // The killswitch stops live only, and a sale is just as much a live order as a buy.
        // Until 05.09.2026 the stop did not touch it: `/kill` halted entries and left exits
        // trading. An armed stop always returns here — falling through below is not allowed
        // in any branch.
        // The lock is taken as a separate expression rather than inside the condition: it is
        // taken a second time below, and the lifetime of a temporary lock inside a condition
        // is not a property worth staking a deadlock on.
        let blocked = wallet.mode == Mode::Live && {
            let k = self.killswitch.lock().unwrap();
            k.is_live_blocked()
        };
        if blocked {
            // There is nothing to sell — the stop does not invent an event: without it there
            // would be no order row either.
            if let ExitShape::Order(req) = exit_request(
                &pos,
                fraction,
                trade.price,
                wallet.max_slippage_pct,
                meta.neg_risk,
                meta.tick,
            ) {
                let reason = self
                    .killswitch
                    .lock()
                    .unwrap()
                    .reason()
                    .map_or("killswitch", TripReason::as_str)
                    .to_string();
                self.db
                    .signals()
                    .record_order(
                        Some(signal.id),
                        &trade.token_id,
                        wallet.mode,
                        "sell",
                        req.limit_price,
                        Decimal::ZERO,
                        "rejected",
                        Some(&format!("killswitch: {reason}")),
                        0,
                    )
                    .await?;
            }
            // The debt to the leader does not vanish because of the stop: it halted our
            // trading, it did not cancel their sale. Without this record an armed stop would
            // silently wipe out what had accumulated, and the exit a retry would later
            // execute would never be copied.
            self.db
                .positions()
                .set_pending_exit(pos.id, want_shares, trade.price)
                .await?;
            return Ok(());
        }

        // The only point where the modes diverge is the same as on a buy: instead of
        // submitting an order we walk the ladder of bids.
        let outcome = match wallet.mode {
            Mode::Shadow => apply_exit_shadow(
                book,
                &pos,
                fraction,
                trade.price,
                wallet.max_slippage_pct,
                meta.tick,
                &meta.fee,
            ),
            Mode::Live => {
                apply_exit(
                    &self.clob,
                    &pos,
                    fraction,
                    trade.price,
                    wallet.max_slippage_pct,
                    meta.neg_risk,
                    meta.tick,
                )
                .await
            }
        };

        match outcome {
            ExitOutcome::Sold(f) => {
                let f = self.priced(f, meta);
                let order = self
                    .db
                    .signals()
                    .record_order(
                        Some(signal.id),
                        &trade.token_id,
                        wallet.mode,
                        "sell",
                        f.avg_price,
                        f.notional,
                        "filled",
                        None,
                        1,
                    )
                    .await?;
                self.book_fill(
                    order.id,
                    &wallet.address,
                    wallet.mode,
                    &trade.token_id,
                    &f,
                    false,
                )
                .await?;
                self.emit(
                    garnet_bus::subjects::ORDER_FILLED,
                    serde_json::json!({
                        "order_id": order.id,
                        "wallet": wallet.address,
                        "mode": wallet.mode.as_str(),
                        "side": "sell",
                        "token_id": trade.token_id,
                        "size": f.size.to_string(),
                        "avg_price": f.avg_price.to_string(),
                        "notional": f.notional.to_string(),
                        "fee_usd": f.fee_usd.to_string(),
                    }),
                )
                .await;
                // Part of the accumulated amount was sold — the remainder keeps waiting.
                self.db
                    .positions()
                    .set_pending_exit(pos.id, want_shares - f.size, trade.price)
                    .await?;
            }
            ExitOutcome::HeldToResolution { reason } => {
                // The exit did not go through — the fraction is not lost but waits for the
                // leader's next sale. That is the accumulation; without it a refusal would
                // mean part of their exit was never copied at all.
                self.db
                    .positions()
                    .set_pending_exit(pos.id, want_shares, trade.price)
                    .await?;
                self.db
                    .signals()
                    .record_order(
                        Some(signal.id),
                        &trade.token_id,
                        wallet.mode,
                        "sell",
                        Decimal::ZERO,
                        Decimal::ZERO,
                        "rejected",
                        Some(&reason),
                        2,
                    )
                    .await?;
            }
            ExitOutcome::Unknown {
                reason,
                size_shares,
                limit_price,
            } => {
                self.metrics.incr("orders_unknown_total", &[]);
                self.db
                    .signals()
                    .record_order(
                        Some(signal.id),
                        &trade.token_id,
                        wallet.mode,
                        "sell",
                        limit_price,
                        size_shares * limit_price,
                        "unknown",
                        Some(&reason),
                        1,
                    )
                    .await?;
            }
            // The fraction did not even reach the size step and rounded to zero. There is no
            // order, but the debt to the leader exists: without this record the accumulated
            // amount would be zeroed by every such sale and would never reach the minimum.
            ExitOutcome::Nothing => {
                self.db
                    .positions()
                    .set_pending_exit(pos.id, want_shares, trade.price)
                    .await?;
            }
        }
        Ok(())
    }

    /// Retries a deferred exit against a fresh book.
    ///
    /// What accumulated waited for the leader's next sale — and only for that. A leader who
    /// exited in full sells no more: on 06.09.2026 a position sat with a deferred exit
    /// covering its whole size and never repeated once during the run, until it sat all the
    /// way to resolution. There is no decision here: the size and the floor are the ones the
    /// leader's sale decided, only the book changes.
    pub async fn retry_pending(&self, pos: &garnet_db::Position) -> anyhow::Result<bool> {
        let Some(wallet) = self.db.wallets().get(&pos.wallet).await? else {
            return Ok(false);
        };
        let meta = self.detector.market(&pos.token_id).await?;
        if meta.closed || meta.resolved_outcome.is_some() {
            // The market is closed: an exit is no longer an exit, settlement will return the
            // money. We clear the deferral, otherwise the loop would come here forever.
            self.db
                .positions()
                .set_pending_exit(pos.id, Decimal::ZERO, Decimal::ZERO)
                .await?;
            return Ok(false);
        }
        // The killswitch stops the retry too: a sale is a live order whoever decided it and
        // whenever (invariants 12, 23).
        // The mode belongs to the position, not to the wallet: see `exit_stale`.
        let mode = pos.mode;
        let blocked = mode == Mode::Live && {
            let k = self.killswitch.lock().unwrap();
            k.is_live_blocked()
        };
        if blocked {
            return Ok(false);
        }

        let book = self.books.book(&pos.token_id).await?;
        let want_shares = pos.pending_exit_shares.min(pos.open_size());
        let fraction = carried_fraction(pos, want_shares);
        let outcome = match mode {
            Mode::Shadow => apply_exit_shadow(
                &book,
                pos,
                fraction,
                pos.pending_exit_price,
                wallet.max_slippage_pct,
                meta.tick,
                &meta.fee,
            ),
            Mode::Live => {
                apply_exit(
                    &self.clob,
                    pos,
                    fraction,
                    pos.pending_exit_price,
                    wallet.max_slippage_pct,
                    meta.neg_risk,
                    meta.tick,
                )
                .await
            }
        };

        match outcome {
            ExitOutcome::Sold(f) => {
                let f = self.priced(f, &meta);
                let order = self
                    .db
                    .signals()
                    .record_order(
                        None,
                        &pos.token_id,
                        mode,
                        "sell",
                        f.avg_price,
                        f.notional,
                        "filled",
                        Some("a deferred exit"),
                        1,
                    )
                    .await?;
                self.book_fill(order.id, &wallet.address, mode, &pos.token_id, &f, false)
                    .await?;
                self.emit(
                    garnet_bus::subjects::ORDER_FILLED,
                    serde_json::json!({
                        "order_id": order.id,
                        "wallet": wallet.address,
                        "mode": mode.as_str(),
                        "side": "sell",
                        "reason": "a deferred exit",
                        "token_id": pos.token_id,
                        "size": f.size.to_string(),
                        "avg_price": f.avg_price.to_string(),
                        "notional": f.notional.to_string(),
                        "fee_usd": f.fee_usd.to_string(),
                    }),
                )
                .await;
                self.db
                    .positions()
                    .set_pending_exit(pos.id, want_shares - f.size, pos.pending_exit_price)
                    .await?;
                Ok(true)
            }
            // The exchange accepted the order, the outcome is unknown: repeating it would mean
            // selling twice (invariant 16). We clear the deferral, the reconciler resolves the
            // divergence.
            ExitOutcome::Unknown {
                reason,
                size_shares,
                limit_price,
            } => {
                self.metrics.incr("orders_unknown_total", &[]);
                self.db
                    .signals()
                    .record_order(
                        None,
                        &pos.token_id,
                        mode,
                        "sell",
                        limit_price,
                        size_shares * limit_price,
                        "unknown",
                        Some(&format!("a deferred exit: {reason}")),
                        1,
                    )
                    .await?;
                self.db
                    .positions()
                    .set_pending_exit(pos.id, Decimal::ZERO, Decimal::ZERO)
                    .await?;
                Ok(false)
            }
            // It did not go through — silently and without a record: a refusal row on every
            // retry tick would clutter the order registry with refusals nobody decided. The
            // position keeps the same debt until the next tick.
            ExitOutcome::HeldToResolution { .. } | ExitOutcome::Nothing => Ok(false),
        }
    }

    /// Sets the fee from the market's schedule.
    ///
    /// The CLOB does not return the fee in an order response, while shadow computes it itself.
    /// Without this step live would be systematically better than shadow by exactly the fee,
    /// and the comparison between the modes would be measuring our own undercount instead of
    /// the quality of execution.
    fn priced(&self, mut f: Fill, meta: &MarketMeta) -> Fill {
        if f.fee_usd.is_zero() {
            f.fee_usd = taker_fee(&meta.fee, f.avg_price, f.size);
        }
        f
    }

    /// The mode is taken as a separate argument rather than from the wallet.
    ///
    /// A wallet is switched between shadow and live while holding open positions, and the mode
    /// is part of the position's key: a fill on a paper position has to land in the paper row,
    /// even if the wallet has already been moved to live.
    async fn book_fill(
        &self,
        order_id: i64,
        wallet: &str,
        mode: Mode,
        token_id: &str,
        f: &Fill,
        is_buy: bool,
    ) -> anyhow::Result<()> {
        self.db
            .signals()
            .record_fill(
                order_id,
                mode,
                f.source.as_str(),
                f.size,
                f.avg_price,
                f.notional,
                f.fee_usd,
            )
            .await?;

        if is_buy {
            self.db
                .positions()
                .apply_buy(wallet, token_id, mode, f.size, f.notional, f.fee_usd)
                .await?;
        } else {
            self.db
                .positions()
                .apply_sell(wallet, token_id, mode, f.size, f.notional, f.fee_usd)
                .await?;
        }

        self.metrics.incr(
            "fills_total",
            &[("mode", mode.as_str()), ("source", f.source.as_str())],
        );
        // The fourth stage: from the submitted order to its fill. Until 19.09.2026 there was a
        // **constant zero** here — that is, the histogram counted observations and carried not
        // one number, and a distribution of zeros is indistinguishable from instantaneous
        // execution. It is measured by the registry's timestamps: the same ones anyone who
        // opens `orders` later will see.
        //
        // A paper fill deliberately does not enter here: it is simulated against the book
        // within the same task, its "latency" is identically zero, and mixing it in with the
        // live one would understate the median by exactly shadow's share.
        if f.source == FillSource::Clob {
            if let Some(o) = self.db.signals().order_by_id(order_id).await? {
                let filled = o.ts_filled.unwrap_or_else(chrono::Utc::now);
                self.metrics
                    .observe("submitted_to_filled", secs_between(o.ts_submitted, filled));
            }
        }
        Ok(())
    }
}
