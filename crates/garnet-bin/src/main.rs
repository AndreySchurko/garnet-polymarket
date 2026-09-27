//! The `garnet-core` binary.
//!
//! Modes: the normal run (the RTDS socket), `--preflight [--live]`, `--migrate-only`,
//! `--trades`, `--settle`, `--reconcile`, `--inject` and `--redeem`. The last two are
//! manual validation tools: they spend real money and require `--yes`.

use garnet_bin::app::App;
use garnet_bin::clob_live::{self, LiveEnv};
use garnet_bin::exec::Exec;
use garnet_bin::inject::{confirm, fresh_tx_hash, synthesize};
use garnet_bin::loops::{self, ChainView};
use garnet_bin::market_source::{HttpMarkets, SharedMarkets};
use garnet_bin::preflight;
use garnet_config::Config;
use garnet_core::detect::Detector;
use garnet_db::{Db, Mode, Side};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;

type Bound = App<SharedMarkets, SharedMarkets, Exec<garnet_clob::GarnetClobClient>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cfg_path = arg_value(&args, "--config").unwrap_or_else(|| "config.toml".to_string());
    let cfg = Config::load(&cfg_path)?;

    // Preflight walks the dependencies itself and must not fail on the first of them, so it
    // comes before connecting to the database.
    if has(&args, "--preflight") {
        let mut checks = preflight::run_base(&cfg).await;
        if has(&args, "--live") {
            checks.extend(preflight::run_live(&cfg, &preflight::Thresholds::default()).await);
            // Only in the live set: in the paper one a loss stop means nothing, the
            // killswitch stops live only (invariant 12).
            checks.push(preflight::loss_stop(cfg.risk.daily_loss_limit_usd));
        }
        for c in &checks {
            println!("{}", c.render());
        }
        std::process::exit(preflight::report(&checks));
    }

    let db = Db::connect(&cfg.database_url).await?;
    db.migrate().await?;

    // `sqlx::migrate!` compiles into the binary, so "deploy migrations only" without a
    // rebuild applies nothing while reporting success. The flag makes this step explicit.

    if has(&args, "--migrate-only") {
        println!("migrations applied");
        return Ok(());
    }

    // No keys — the process starts anyway: shadow is a measuring instrument and does not
    // require a production wallet. But live wallets then get a refusal on every order rather
    // than quietly moving into shadow.
    let exec = match clob_live::resolve(&LiveEnv::from_process())? {
        Some(live) => {
            println!(
                "live path enabled: signature {:?}, funder {}, network {}",
                live.signature_type,
                live.funder
                    .map_or_else(|| "none (EOA)".to_owned(), |f| format!("{f:#x}")),
                live.chain_id
            );
            let settings = garnet_clob::ClobSettings {
                clob_host: cfg.api.clob_host.clone(),
                ..Default::default()
            };
            Exec::live(clob_live::connect(live, settings).await?)
        }
        None => {
            println!("live path disabled: no keys are set, live wallets will be refused");
            Exec::off()
        }
    };
    let live_path = exec.is_live();

    let markets = SharedMarkets::new(HttpMarkets::new(&cfg.api.clob_host, &cfg.api.gamma_host)?);
    let wallets = db.wallets().list().await?;
    println!("wallets under observation: {}", wallets.len());

    let detector = Detector::new(
        db.clone(),
        markets.clone(),
        wallets.iter().map(|w| w.address.clone()),
    );
    // The bus is an addressee for observers, not a participant in a trade: its absence costs
    // events, not trading.
    let events = match garnet_bin::events::Events::connect(&cfg.api.nats_url).await {
        Ok(e) => {
            println!("event bus: {}", cfg.api.nats_url);
            e
        }
        Err(e) => {
            eprintln!("the bus is unreachable ({e}): events will not be published");
            garnet_bin::events::Events::off()
        }
    };

    let alerts = loops::Alerts::new(events.clone());
    let app = Arc::new(App::new(
        db.clone(),
        detector,
        markets.clone(),
        exec,
        &cfg,
        events.clone(),
    ));

    // Live free funds are read from the chain: without that `decide` sees zero and rejects
    // every live order as `insufficient_balance`.
    let live_cash = if live_path {
        match garnet_blockchain::client::GarnetBlockchainClient::from_env() {
            Ok(chain) => {
                let view = std::sync::Arc::new(ChainView::new(std::sync::Arc::new(chain)));
                match view.cash().await {
                    Ok(amount) => {
                        println!("free live collateral: pUSD {amount}");
                        app.set_live_cash(amount);
                    }
                    Err(e) => eprintln!(
                        "the live balance was not read ({e}): live orders will be rejected"
                    ),
                }
                Some(view)
            }
            Err(e) => {
                eprintln!("the chain is unreachable ({e}): live orders will be rejected");
                None
            }
        }
    } else {
        None
    };

    // Diagnostics: what the client sees in its own feed. Read-only.
    if has(&args, "--trades") {
        use garnet_clob::traits::ClobClient;
        let since = chrono::Utc::now() - chrono::Duration::hours(2);
        match exec_client(&app) {
            None => println!("there is no client: no keys are set"),
            Some(client) => match client.get_trades(since).await {
                Err(e) => println!("get_trades: {e}"),
                Ok(trades) => {
                    println!("trades in the last 2 hours: {}", trades.len());
                    for t in trades {
                        println!(
                            "  {} {:?} {} at {} taker={} makers={:?}",
                            t.match_time,
                            t.side,
                            t.size,
                            t.price,
                            t.taker_order_id,
                            t.maker_order_ids
                        );
                    }
                }
            },
        }
        return Ok(());
    }

    // Latency per stage of the signal path (invariant 42).
    if has(&args, "--latency") {
        let period = arg_value(&args, "--latency").unwrap_or_else(|| "day".into());
        let since = match period.as_str() {
            "all" => None,
            "week" => Some(chrono::Utc::now() - chrono::Duration::days(7)),
            "day" => Some(chrono::Utc::now() - chrono::Duration::days(1)),
            other => anyhow::bail!("\"{other}\" is not it: --latency [day|week|all]"),
        };
        let rows = db.reports().latency(since).await?;
        if rows.is_empty() {
            println!("there were no signals in this period: nothing to measure");
            return Ok(());
        }
        println!("{:<22} {:>7} {:>10} {:>10}", "stage", "n", "median", "p90");
        for r in &rows {
            let f = |v: Option<rust_decimal::Decimal>| {
                v.map_or_else(|| "—".to_string(), |x| format!("{:.2}", x))
            };
            println!(
                "{:<22} {:>7} {:>10} {:>10}",
                r.stage,
                r.n,
                f(r.median_secs),
                f(r.p90_secs)
            );
        }
        return Ok(());
    }

    // The depth of the entry queue per wallet and the threshold's counterfactual
    // (invariant 44).
    if has(&args, "--firerate") {
        let window: i64 = arg_value(&args, "--window")
            .map_or(Ok(30), |v| v.parse())
            .map_err(|_| {
                anyhow::anyhow!("the window is a number of seconds: --firerate --window 30")
            })?;
        let rows = db.reports().fire_depth(window).await?;
        if rows.is_empty() {
            println!(
                "there are no copies yet: the distribution is gathered from the engine's decisions"
            );
            return Ok(());
        }
        println!("window {window} s");
        println!("{:<8} {:>8} {:>8}", "mode", "depth", "copies");
        for r in &rows {
            println!("{:<8} {:>8} {:>8}", r.mode.as_str(), r.depth, r.copies);
        }

        let copies = db.reports().copy_times(20_000).await?;
        println!(
            "\nthe threshold's counterfactual over {} copies:",
            copies.len()
        );
        for limit in [1_usize, 2, 4, 10] {
            let refused = garnet_risk::fire_rate::would_refuse(&copies, window, limit);
            println!(
                "  fire_limit = {limit:<3} refused {refused} ({}%)",
                refused * 100 / copies.len().max(1)
            );
        }
        println!(
            "\nnow: fire_limit = {}, window {} s",
            cfg.risk.fire_limit, cfg.risk.fire_window_secs
        );
        return Ok(());
    }

    // The emergency close of positions (invariant 46).
    //
    // A phrase rather than a `--yes` flag: a button is pressed by accident, a phrase is not.
    // The intent is written to `controls` BEFORE the check — it has to stay in the trace even
    // when the gate rejected it: a rejected attempt to close everything is exactly what the
    // operator will later want to know about.
    if has(&args, "--flatten") {
        use garnet_core::flatten::{check, required_phrase, FlattenMode, Gate, Intent};

        let raw = require(&args, "--flatten")?;
        let Some(mode) = FlattenMode::parse(&raw) else {
            anyhow::bail!("\"{raw}\" is not a mode: --flatten [graceful|hybrid|panic]");
        };
        let name = &cfg.flatten.name;
        let phrase = arg_value(&args, "--phrase").unwrap_or_default();
        let ack = has(&args, "--acknowledge-forfeit");
        let now = chrono::Utc::now();

        db.controls()
            .set_flatten_intent(mode.as_str(), now, ack, "cli")
            .await?;

        let intent = Intent {
            mode,
            declared_at: now,
            acknowledge_forfeit: ack,
        };
        let stopped = db.controls().manual_stop().await?;

        match check(&intent, &phrase, name, now, stopped) {
            Gate::Refuse(r) => {
                println!("refused: {}", r.why());
                println!("the phrase is required: {}", required_phrase(mode, name));
                if mode == FlattenMode::Hybrid && !ack {
                    println!("and the --acknowledge-forfeit flag");
                }
                db.controls().clear_flatten_intent().await?;
                // The exit code distinguishes a refusal from a success: a script for which
                // "refused" and "closed" are the same would close everything silently.
                std::process::exit(1);
            }
            Gate::Go(mode) => {
                let report = app.flatten(mode, "cli").await?;
                db.controls().clear_flatten_intent().await?;
                println!(
                    "emergency close {}: considered {}, sold {}, left to live out {}",
                    mode.as_str(),
                    report.considered,
                    report.sold,
                    report.kept_to_live
                );
                if report.kept_losing > 0 || report.kept_unmeasurable > 0 {
                    println!(
                        "  not in profit {} · nothing to measure with {}",
                        report.kept_losing, report.kept_unmeasurable
                    );
                }
                for (token, why) in &report.failed {
                    println!("  NOT CLOSED {token}: {why}");
                }
                if !report.failed.is_empty() {
                    std::process::exit(1);
                }
            }
        }
        return Ok(());
    }

    // The race between delivery circuits: who brings trades and who duplicates them.
    if has(&args, "--sources") {
        let period = arg_value(&args, "--sources").unwrap_or_else(|| "day".into());
        let since = match period.as_str() {
            "all" => None,
            "week" => Some(chrono::Utc::now() - chrono::Duration::days(7)),
            "day" => Some(chrono::Utc::now() - chrono::Duration::days(1)),
            other => anyhow::bail!("\"{other}\" is not it: --sources [day|week|all]"),
        };
        let rows = db.reports().source_race(since).await?;
        if rows.is_empty() {
            println!("there are no sightings: there were no leader trades in this period");
            return Ok(());
        }
        println!(
            "{:<8} {:>7} {:>14} {:>16} {:>10} {:>10}",
            "circuit", "first", "confirmed", "median lag, s", "only it", "total"
        );
        for r in &rows {
            let lag = match r.median_lag_secs {
                Some(l) => format!("{:.1}", l),
                None => "—".to_string(),
            };
            println!(
                "{:<8} {:>7} {:>14} {:>16} {:>10} {:>10}",
                r.source, r.wins, r.confirmations, lag, r.only_source, r.sightings
            );
        }
        return Ok(());
    }

    // The quality of copying: our half against the leader's.
    //
    // A subcommand of its own rather than a line in `--settle`: this report answers not
    // "how much did we earn" but "how well do we copy", and mixing it with the absolute
    // result would conflate them again (invariant 49).
    if has(&args, "--matchup") {
        let period = arg_value(&args, "--matchup").unwrap_or_else(|| "day".into());
        let since = match period.as_str() {
            "all" => None,
            "week" => Some(chrono::Utc::now() - chrono::Duration::days(7)),
            "day" => Some(chrono::Utc::now() - chrono::Duration::days(1)),
            other => anyhow::bail!("\"{other}\" is not it: --matchup [day|week|all]"),
        };
        for mode in [Mode::Live, Mode::Shadow] {
            let rep = db.matchup().report(mode, since).await?;
            println!("\n=== {} ===", mode.as_str());
            if rep.rows.is_empty() {
                println!("  nothing to compare: there are no leader trades in this period");
                continue;
            }
            match rep.avg_entry_diff_c {
                Some(c) => println!("  average entry: {c:.2}¢ against us"),
                None => println!("  average entry: nothing to measure with"),
            }
            match rep.avg_gap_pts {
                Some(g) => println!("  average gap: {g:.1} pp"),
                None => println!("  average gap: there are no closed positions"),
            }
            println!(
                "  measurable {} · skipped {}",
                rep.n_measurable, rep.n_skipped
            );
            if rep.n_unevaluated > 0 {
                println!("  not evaluated in this mode: {}", rep.n_unevaluated);
            }
            for r in &rep.rows {
                let entry = match r.entry_diff_c() {
                    Some(c) => format!("{c:.2}¢"),
                    None => "—".to_string(),
                };
                let gap = match r.gap() {
                    Some(g) => format!("{g:.1}"),
                    None => "—".to_string(),
                };
                println!(
                    "  {:<14} {:<10} ours {:.4} theirs {:.4} entry {:<8} gap {:<7} {:?}",
                    clip(&r.question, 14),
                    r.outcome_label,
                    r.our_avg,
                    r.his_avg_since,
                    entry,
                    gap,
                    r.status
                );
            }
        }
        return Ok(());
    }

    // A manual settlement pass. The redeemer is chosen by the wallet's mode: on a proxy
    // account the platform credits the payout, and there must be no transaction.
    if has(&args, "--settle") {
        let report = loops::settle_and_announce(
            &db,
            &markets,
            &garnet_bin::auto_payout::AutoPayout,
            cfg.settlement.batch,
            &events,
        )
        .await?;
        println!(
            "checked {}, resolved {}, closed {}, not read {}",
            report.checked,
            report.resolved,
            report.settled.len(),
            report.failed.len()
        );
        for s in &report.settled {
            println!(
                "  {} {}: {} — payout {}, total {}",
                s.wallet,
                s.token_id,
                if s.won { "a win" } else { "a loss" },
                s.payout_usd,
                s.pnl_usd()
            );
        }
        for (token, why) in &report.failed {
            println!("  {token}: {why}");
        }
        return Ok(());
    }

    // A manual reconciliation pass: our ledger against what the chain sees.
    if has(&args, "--reconcile") {
        let Some(chain) = live_cash.as_ref() else {
            println!("reconciliation is impossible: there are no keys, there is no on-chain leg");
            return Ok(());
        };
        let report = garnet_core::reconcile::reconcile_once(
            &db,
            chain.as_ref(),
            &markets,
            &loops::Alerts::new(garnet_bin::events::Events::off()),
            cfg.reconcile.in_flight_window_secs,
        )
        .await?;
        println!(
            "reconciliation: checked {}, divergences {}, awaiting settlement {}, explained by an order {}, not read {}",
            report.checked,
            report.divergences.len(),
            report.awaiting_settlement,
            report.explained,
            report.unreadable.len()
        );
        for d in &report.divergences {
            println!(
                "  ALARM {}: {} in the database, {} on chain",
                d.token_id, d.in_db, d.on_chain
            );
        }
        // The unreadable is printed on a line of its own: a reconciler that failed to read a
        // balance has to say "the check could not be performed" rather than stay silent
        // (invariant 47).
        for t in &report.unreadable {
            println!("  not read {t}: the check could not be performed");
        }
        return Ok(());
    }

    if has(&args, "--inject") {
        return inject_once(&app, &db, &args).await;
    }
    if has(&args, "--redeem") {
        return redeem_once(&db, &markets, &args).await;
    }

    run_feed(&cfg, app, db, markets, live_cash, alerts, events).await
}

/// The normal mode: the RTDS socket and the background loops until the process stops.
async fn run_feed(
    cfg: &Config,
    app: Arc<Bound>,
    db: Db,
    markets: SharedMarkets,
    live_cash: Option<Arc<ChainView>>,
    alerts: loops::Alerts,
    // Alerts and events travel on one bus, but these are different roles: `Alerts` is a shout
    // to the operator, `Events` is the trading path. They must not be mixed.
    events: garnet_bin::events::Events,
) -> anyhow::Result<()> {
    // Shutdown is a shared signal: the loops finish what they started rather than tearing off
    // in the middle of a write.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let feed = garnet_feed::Feed::new(cfg.api.rtds_url.clone());
    let clock = feed.clock();

    let tasks = vec![
        loops::spawn_health(
            app.clone(),
            db.clone(),
            clock,
            alerts.clone(),
            cfg,
            shutdown_rx.clone(),
        ),
        loops::spawn_settlement(
            db.clone(),
            markets.clone(),
            cfg,
            events.clone(),
            shutdown_rx.clone(),
        ),
        loops::spawn_activity_poll(
            app.clone(),
            db.clone(),
            garnet_bin::activity::Activity::new(&cfg.api.data_host)?,
            cfg,
            shutdown_rx.clone(),
        ),
        loops::spawn_equity(
            app.clone(),
            db.clone(),
            markets.clone(),
            live_cash.clone(),
            cfg,
            shutdown_rx.clone(),
        ),
    ];

    // The time-based exit starts only when enabled: a loop that reads the database every ten
    // minutes for zero positions is noise in the log and extra load on a path that is busy
    // enough without it.
    let tasks = match loops::spawn_stale_exit(
        app.clone(),
        db.clone(),
        cfg.copy.max_hold_hours,
        std::time::Duration::from_secs(600),
        shutdown_rx.clone(),
    ) {
        Some(t) => {
            let mut v = tasks;
            v.push(t);
            v
        }
        None => tasks,
    };

    // The retry of a deferred exit always runs: it does not decide to trade but carries
    // through a decision the leader has already made, which would otherwise wait for their
    // next sale — and that may never come. Two minutes: the fraction is small, and it can run
    // into a single level of the book, which lives for minutes.

    let tasks = {
        let mut v = tasks;
        v.push(loops::spawn_pending_exit(
            app.clone(),
            db.clone(),
            std::time::Duration::from_secs(120),
            shutdown_rx.clone(),
        ));
        v
    };

    // The heartbeat always runs, and first: the watcher tells "the leaders are quiet" from
    // "there is nobody to trade" only by it.
    let tasks = {
        let mut v = tasks;
        v.push(loops::spawn_heartbeat(
            db.clone(),
            std::time::Duration::from_secs(30),
            shutdown_rx.clone(),
        ));
        v
    };

    // The executor of the emergency close always runs: the gate is passed by the bot or the
    // CLI, while the selling is done by the exchange, which exists only here. A loop disabled
    // by config would mean an installation where `/flatten` silently does not work — the
    // worst kind of broken emergency button.
    let tasks = {
        let mut v = tasks;
        v.push(loops::spawn_flatten_watch(
            app.clone(),
            db.clone(),
            shutdown_rx.clone(),
        ));
        v
    };

    // Reconciliation runs only when there is something to reconcile against: without keys
    // there is no on-chain leg at all.
    let tasks = match live_cash {
        Some(chain) => {
            let mut t = tasks;
            t.push(loops::spawn_reconcile(
                db.clone(),
                markets.clone(),
                chain,
                alerts.clone(),
                cfg,
                shutdown_rx.clone(),
            ));
            t
        }
        None => tasks,
    };

    // Parsing and the database write do not sit on the receive path: in the predecessor a
    // write on the receiving side blocked the socket and lost trades. The receiver only puts a
    // frame into the channel.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1024);

    let worker = {
        let app = app.clone();
        tokio::spawn(async move {
            while let Some(raw) = rx.recv().await {
                match serde_json::from_str::<serde_json::Value>(&raw) {
                    Ok(frame) => {
                        if let Err(e) = app.on_frame(&frame).await {
                            eprintln!("frame handling: {e}");
                        }
                    }
                    Err(e) => eprintln!("an unreadable frame: {e}"),
                }
            }
        })
    };

    // The third circuit: Polygon logs. An empty `polygon_ws_url` disables it entirely — it
    // needs a node of your own, and an installation that demands one on the first run does
    // not start at all.
    let chain_task = if cfg.feed.polygon_ws_url.is_empty() {
        println!("the chain-log circuit is disabled (feed.polygon_ws_url is empty)");
        None
    } else if cfg.feed.polygon_rpc_url.is_empty() {
        // Managing silently is not allowed: without block timestamps the circuit would record
        // trades with our time instead of the leader's.
        anyhow::bail!(
            "feed.polygon_ws_url is set while feed.polygon_rpc_url is empty:              there is nowhere to get block timestamps from"
        );
    } else {
        let exchanges: Vec<String> = [
            "POLYGON_CTF_EXCHANGE_V2_ADDRESS",
            "POLYGON_NEG_RISK_CTF_EXCHANGE_V2_ADDRESS",
        ]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .filter(|v| !v.is_empty())
        .collect();
        if exchanges.is_empty() {
            anyhow::bail!("the chain-log circuit is enabled while the V2 exchange addresses are not set in the environment");
        }
        // The list is read BEFORE the lock is taken: a `std::sync::Mutex` held across an
        // `await` blocks the executor and can deadlock — and there is no reason to hold it
        // here, the value is ready in advance.
        let initial: Vec<String> = db
            .wallets()
            .list()
            .await?
            .into_iter()
            .map(|x| x.address)
            .collect();
        let watched = std::sync::Arc::new(std::sync::Mutex::new(initial));
        let clock = std::sync::Arc::new(garnet_feed::chain::BlockClock::new(
            cfg.feed.polygon_rpc_url.clone(),
        )?);
        println!(
            "chain-log circuit: {} ({} exchanges)",
            cfg.feed.polygon_ws_url,
            exchanges.len()
        );

        let feed = garnet_feed::chain::ChainFeed::new(
            cfg.feed.polygon_ws_url.clone(),
            exchanges,
            watched.clone(),
        );
        let app_c = app.clone();
        let db_c = db.clone();
        // The wallet list is re-read on a tick of its own: the filter lives on the node's
        // side, and an assigned wallet is invisible until the subscription is
        // re-established.
        let watched_c = watched.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                ticker.tick().await;
                if let Ok(ws) = db_c.wallets().list().await {
                    let fresh: Vec<String> = ws.into_iter().map(|x| x.address).collect();
                    *watched_c.lock().unwrap() = fresh;
                }
            }
        });
        Some(tokio::spawn(async move {
            let (tx, mut rx) =
                tokio::sync::mpsc::channel::<(garnet_feed::chain::ChainFill, u64)>(1024);
            let clock_w = clock.clone();
            tokio::spawn(async move {
                while let Some((fill, block)) = rx.recv().await {
                    // The block timestamp is the only thing a log does not carry. A node
                    // failure is not substituted with our clock: a trade with an invented
                    // time would pass the assignment threshold and the slice window by the
                    // wrong clock.
                    match clock_w.timestamp(block).await {
                        Ok(secs) => match chrono::DateTime::from_timestamp(secs, 0) {
                            Some(ts) => {
                                if let Err(e) = app_c.on_chain_fill(&fill, ts).await {
                                    eprintln!("chain circuit: {e:#}");
                                }
                            }
                            None => eprintln!(
                                "chain circuit: the timestamp of block {block} is unreadable"
                            ),
                        },
                        Err(e) => eprintln!("chain circuit: the timestamp of block {block}: {e:#}"),
                    }
                }
            });
            feed.run_forever(move |log| {
                if let Some(fill) = garnet_feed::chain::parse_log(&log) {
                    let block = fill.block_number;
                    if tx.try_send((fill, block)).is_err() {
                        eprintln!("chain circuit: the queue is full, a log was lost");
                    }
                }
            })
            .await
        }))
    };

    println!("connecting to {}", cfg.api.rtds_url);
    let feed_task = tokio::spawn(async move {
        feed.run_forever(move |raw| {
            // A full channel means processing is falling behind: losing frames silently is not
            // allowed, so it is visible in the log.
            if tx.try_send(raw.to_string()).is_err() {
                eprintln!("the frame queue is full, a frame was lost");
            }
        })
        .await
    });

    tokio::signal::ctrl_c().await?;
    println!("shutting down: stopping the loops");
    let _ = shutdown_tx.send(true);
    for t in tasks {
        let _ = t.await;
    }

    // The feed cannot stop by itself — nor should it: while the process lives its job is to
    // reconnect. We stop it together with the process, having let the worker finish what is
    // already in the channel.
    feed_task.abort();
    if let Some(t) = chain_task {
        t.abort();
    }
    drop(worker);
    Ok(())
}

/// `--inject`: a hand-made leader trade through the whole pipeline.
async fn inject_once(app: &Bound, db: &Db, args: &[String]) -> anyhow::Result<()> {
    let wallet = require(args, "--wallet")?;
    let token = require(args, "--token")?;
    let price = Decimal::from_str(&require(args, "--price")?)?;
    let size = Decimal::from_str(&require(args, "--size")?)?;
    let side = match arg_value(args, "--side")
        .unwrap_or_else(|| "buy".into())
        .to_lowercase()
        .as_str()
    {
        "buy" => Side::Buy,
        "sell" => Side::Sell,
        other => anyhow::bail!("--side must be buy|sell, got `{other}`"),
    };

    // The wallet has to be in the registry: the detector looks only at addresses the operator
    // assigned, and an injection for another address would silently do nothing.
    let row = db
        .wallets()
        .get(&wallet)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the wallet {wallet} is not in the registry"))?;

    confirm(row.mode == Mode::Live, has(args, "--yes"))?;

    let tx_hash = fresh_tx_hash();
    println!(
        "injecting: {} {:?} {} at {} shares {} (mode {:?}, hash {tx_hash})",
        row.display(),
        side,
        token,
        price,
        size,
        row.mode
    );

    let frame = synthesize(&wallet, &token, side, price, size, &tx_hash);
    app.on_frame(&frame).await?;

    for s in db.signals().recent(3).await? {
        println!(
            "signal #{} {} {:?} verdict {} for ${}",
            s.id, s.wallet, s.mode, s.verdict, s.target_size_usd
        );
    }
    match db.positions().get(&wallet, &token, row.mode).await? {
        Some(p) => println!(
            "position #{}: bought {}, sold {}, cost ${}, fees ${}",
            p.id, p.size_bought, p.size_sold, p.cost_usd, p.fees_usd
        ),
        None => println!("there is no position: see the reason in the signal and order rows"),
    }
    Ok(())
}

/// `--redeem`: a manual redemption of one position.
async fn redeem_once(db: &Db, markets: &SharedMarkets, args: &[String]) -> anyhow::Result<()> {
    use garnet_core::detect::MarketSource;
    use garnet_core::settle::Redeemer;

    let wallet = require(args, "--wallet")?;
    let token = require(args, "--token")?;
    confirm(true, has(args, "--yes"))?;

    let position = db
        .positions()
        .get(&wallet, &token, Mode::Live)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{wallet} has no live position in {token}"))?;
    let size = position.open_size();
    anyhow::ensure!(
        size > Decimal::ZERO,
        "the position is empty, there is nothing to redeem"
    );

    let meta = markets.get(&token).await?;
    let chain = garnet_blockchain::client::GarnetBlockchainClient::from_env()?;
    let redeemer = garnet_bin::chain_redeemer::ChainRedeemer::new(chain);

    match redeemer.redeem(&meta, size).await? {
        Some(tx) => println!("redemption sent: {tx}"),
        None => println!("no redemption was required"),
    }
    Ok(())
}

/// The live client from the wired-up application, if there is one.
fn exec_client(app: &Bound) -> Option<&garnet_clob::GarnetClobClient> {
    app.clob().client()
}

/// Trim a market's question to the column width. Measured in characters rather than bytes:
/// questions contain multi-byte characters, and a byte slice tears a letter in half.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

fn require(args: &[String], name: &str) -> anyhow::Result<String> {
    arg_value(args, name).ok_or_else(|| anyhow::anyhow!("the argument {name} is required"))
}
