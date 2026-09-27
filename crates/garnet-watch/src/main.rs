//! `garnet-watch` — the watcher (invariant 45).
//!
//! `Type=oneshot`: started by a timer, it checks, prints and exits. There are three
//! exit codes — `0` agreed, `1` a divergence, `2` could not be checked — and the third
//! exists because "could not" is not "agreed".
//!
//! It does not restart or stop the trading process. It does not write to the trading
//! tables at all: a watcher that can repair what it watches will sooner or later
//! repair it wrongly, and by then there will be nobody left to explain the divergence.

use chrono::{Duration, Utc};
use garnet_config::Config;
use garnet_db::Db;
use garnet_watch::{checks, Report, Verdict};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cfg_path = arg(&args, "--config").unwrap_or_else(|| "config.toml".to_string());
    let cfg = Config::load(&cfg_path)?;

    // The connection is the one thing the watcher does not survive: without the
    // database it can say neither "agreed" nor "diverged". That is exit code 2, not a
    // panic with an unintelligible trace.
    let db = match Db::connect(&cfg.database_url).await {
        Ok(db) => db,
        Err(e) => {
            println!("NOT CHECKED  the database is unreachable: {e:#}");
            std::process::exit(2);
        }
    };

    let now = Utc::now();
    let mut report = Report::default();

    // The heartbeat. The threshold is three times the write period: a mark updated
    // less often than the check produces a false alarm on every other run.
    report.checks.push(match db.controls().last_beat().await {
        Ok(v) => checks::heartbeat(v, now, Duration::seconds(150)),
        Err(e) => checks::Check::unknown("process", format!("not read: {e}")),
    });

    // Equity snapshots: the loop runs on a timer and does not depend on trading.
    let equity_allowed =
        Duration::seconds(i64::try_from(cfg.equity.snapshot_interval_secs).unwrap_or(300) * 3);
    report
        .checks
        .push(match db.equity().latest(garnet_db::Mode::Shadow).await {
            Ok(v) => checks::equity(v.map(|s| s.ts), now, equity_allowed),
            Err(e) => checks::Check::unknown("equity snapshots", format!("not read: {e}")),
        });

    // The trade flow. It raises an alarm only when wallets are enabled: a watcher
    // shouting at a quiet night is the first to stop being read.
    let enabled = match db.wallets().list().await {
        Ok(ws) => Some(ws.iter().filter(|w| w.enabled).count()),
        Err(_) => None,
    };
    let last_trade: Result<Option<chrono::DateTime<Utc>>, _> =
        sqlx::query_scalar("SELECT max(ts_seen) FROM leader_trades")
            .fetch_one(db.pool())
            .await;
    report.checks.push(match (enabled, last_trade) {
        (Some(n), Ok(t)) => checks::feed(t, n, now, Duration::hours(6)),
        _ => checks::Check::unknown("trade flow", "the registry or the trades were not read"),
    });

    // Orders without an outcome that have aged past the explanation window
    // (invariant 48).
    let window = cfg.reconcile.in_flight_window_secs.max(60);
    let stale_unknown: Result<i64, _> = sqlx::query_scalar(
        "SELECT count(*) FROM orders
          WHERE status = 'unknown' AND mode = 'live'
            AND ts_submitted < now() - make_interval(secs => $1::double precision)",
    )
    .bind(window as f64)
    .fetch_one(db.pool())
    .await;
    report.checks.push(match stale_unknown {
        Ok(n) => checks::stale_unknown_orders(n),
        Err(e) => checks::Check::unknown("orders without an outcome", format!("not read: {e}")),
    });

    // Leader merges we never exited on (invariant 40).
    report
        .checks
        .push(match db.actions().unhandled_merges(1000).await {
            Ok(v) => checks::unhandled_merges(i64::try_from(v.len()).unwrap_or(i64::MAX)),
            Err(e) => checks::Check::unknown("leader merges", format!("not read: {e}")),
        });

    // Stale positions: the time-based exit loop should have closed them.
    let hold = cfg.copy.max_hold_hours;
    report.checks.push(if hold <= 0 {
        checks::stale_positions(0, hold)
    } else {
        match db.positions().stale_open(hold, 1000).await {
            Ok(v) => checks::stale_positions(i64::try_from(v.len()).unwrap_or(i64::MAX), hold),
            Err(e) => checks::Check::unknown("stale positions", format!("not read: {e}")),
        }
    });

    for c in &report.checks {
        let (mark, what) = match &c.verdict {
            Verdict::Agreed(w) => ("agreed     ", w),
            Verdict::Diverged(w) => ("DIVERGED   ", w),
            Verdict::Unknown(w) => ("NOT CHECKED", w),
        };
        println!("{mark} {:<26} {what}", c.name);
    }

    let code = report.exit_code();
    println!(
        "\nresult: {}",
        match code {
            0 => "agreed",
            1 => "a divergence exists",
            _ => "could not be checked",
        }
    );
    std::process::exit(code);
}

fn arg(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}
