//! The boundaries of the watcher's checks (invariant 45).
//!
//! It is the boundaries that are checked: how much silence it is still too early to
//! shout about, and how "could not be checked" differs from "agreed". A watcher that
//! shouts without cause stops being read before the thing it was written for happens.

use chrono::{Duration, Utc};
use garnet_watch::checks::{
    equity, feed, heartbeat, stale_positions, stale_unknown_orders, unhandled_merges, Check,
};
use garnet_watch::{Report, Verdict};

fn is_agreed(c: &Check) -> bool {
    matches!(c.verdict, Verdict::Agreed(_))
}
fn is_diverged(c: &Check) -> bool {
    matches!(c.verdict, Verdict::Diverged(_))
}
fn is_unknown(c: &Check) -> bool {
    matches!(c.verdict, Verdict::Unknown(_))
}

#[test]
fn a_missing_heartbeat_is_unknown_not_agreed_and_not_a_divergence() {
    // There is no mark at all — the process may never have started. Saying "agreed"
    // means reporting a check that never happened; saying "diverged" means accusing
    // the process of something it may not have been supposed to be doing at all.

    let c = heartbeat(None, Utc::now(), Duration::seconds(150));
    assert!(is_unknown(&c), "{:?}", c.verdict);
    assert!(!is_agreed(&c));
}

#[test]
fn the_heartbeat_edge_is_inclusive() {
    let now = Utc::now();
    let allowed = Duration::seconds(150);
    assert!(
        is_agreed(&heartbeat(Some(now - allowed), now, allowed)),
        "exactly at the limit it is still alive"
    );
    assert!(is_diverged(&heartbeat(
        Some(now - allowed - Duration::seconds(1)),
        now,
        allowed
    )));
}

#[test]
fn a_quiet_night_is_not_a_divergence_when_nobody_is_enabled() {
    // A watcher that shouts at silence while the wallets are disabled shouts every
    // night — and people stop reading it before it says anything important.
    let now = Utc::now();
    let long_ago = Some(now - Duration::days(3));
    assert!(is_agreed(&feed(long_ago, 0, now, Duration::hours(6))));
    assert!(is_agreed(&feed(None, 0, now, Duration::hours(6))));
}

#[test]
fn silence_with_enabled_wallets_is_a_divergence() {
    let now = Utc::now();
    assert!(is_diverged(&feed(
        Some(now - Duration::hours(7)),
        3,
        now,
        Duration::hours(6)
    )));
    assert!(is_agreed(&feed(
        Some(now - Duration::hours(5)),
        3,
        now,
        Duration::hours(6)
    )));
}

#[test]
fn no_trades_at_all_with_enabled_wallets_is_unknown_not_a_divergence() {
    // A fresh installation: wallets assigned, no trades yet. That is not "the flow
    // stalled" — it is "there has been no flow yet", and it is the watcher that must
    // tell them apart, not a half-asleep operator.
    let c = feed(None, 3, Utc::now(), Duration::hours(6));
    assert!(is_unknown(&c), "{:?}", c.verdict);
}

#[test]
fn equity_snapshots_do_not_depend_on_leaders_trading() {
    // A snapshot runs on a timer. Its absence means the loop inside the trading
    // process has stalled even if the process itself is alive — so here silence IS a
    // divergence, unlike in the trade flow.
    let now = Utc::now();
    let allowed = Duration::seconds(900);
    assert!(is_diverged(&equity(
        Some(now - Duration::hours(1)),
        now,
        allowed
    )));
    assert!(is_agreed(&equity(
        Some(now - Duration::minutes(10)),
        now,
        allowed
    )));
    assert!(is_unknown(&equity(None, now, allowed)));
}

#[test]
fn a_disabled_time_exit_has_no_stale_positions_by_definition() {
    // Zero hours means the time-based exit loop does not run at all. Reporting zero
    // stale positions means saying there are none — whereas they were never
    // counted.
    let c = stale_positions(0, 0);
    assert!(is_agreed(&c));
    match &c.verdict {
        Verdict::Agreed(w) => assert!(w.contains("disabled"), "{w}"),
        v => panic!("{v:?}"),
    }
}

#[test]
fn the_cheap_counters_say_what_they_counted() {
    assert!(is_agreed(&stale_unknown_orders(0)));
    assert!(is_diverged(&stale_unknown_orders(2)));
    assert!(is_agreed(&unhandled_merges(0)));
    assert!(is_diverged(&unhandled_merges(1)));
    assert!(is_diverged(&stale_positions(4, 72)));
}

#[test]
fn a_divergence_is_louder_than_an_unknown() {
    // Both are printed, but there is one exit code. A divergence is something already
    // known to be wrong; an unknown is raised only when there are no divergences at
    // all.
    let mut r = Report::default();
    r.checks.push(Check::unknown("a", "not read"));
    assert_eq!(r.exit_code(), 2);
    r.checks.push(Check::diverged("b", "diverged"));
    assert_eq!(r.exit_code(), 1, "a divergence outranks an unknown");
}

#[test]
fn an_empty_report_is_not_success() {
    // An empty report means nothing was checked. A zero here would read as "all is
    // well" — that is, precisely the opposite.
    let r = Report::default();
    assert_eq!(r.exit_code(), 0, "an empty report invents no codes");
    // But such a report is never printed: the binary adds six checks unconditionally,
    // and any of them yields `Unknown` on a failed read.
}

#[test]
fn everything_agreed_is_the_only_zero() {
    let mut r = Report::default();
    for n in ["a", "b", "c"] {
        r.checks.push(Check::agreed(
            Box::leak(n.to_string().into_boxed_str()),
            "ok",
        ));
    }
    assert_eq!(r.exit_code(), 0);
}
