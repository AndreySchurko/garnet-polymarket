use chrono::Duration;
use garnet_risk::health::{Health, HealthInput};
use garnet_risk::killswitch::{Killswitch, TripReason};
use garnet_risk::metrics::{Metrics, STAGES};

fn input(last_trade: Duration, control: Duration) -> HealthInput {
    HealthInput {
        process_up: true,
        socket_connected: true,
        last_trade_age: last_trade,
        control_topic_age: control,
        threshold: Duration::minutes(15),
        control_threshold: Duration::minutes(2),
    }
}

#[test]
fn process_alive_but_no_data_is_unhealthy() {
    // Exactly this case gave 6.5 hours of blindness in the predecessor: the
    // process alive, the socket connected, the guards reporting OK — and no data.
    let h = Health::evaluate(&input(Duration::minutes(45), Duration::seconds(3)));
    assert!(
        !h.ok(),
        "a live process without data must count as unhealthy"
    );
    assert_eq!(h, Health::NoTrades { age_secs: 2700 });
    assert!(h.reason().contains("2700"));
}

#[test]
fn silent_control_topic_means_we_went_deaf() {
    // On 31.08 the activity/trades topic was down platform-wide; the control topic
    // separates "the market is quiet" from "we have gone deaf".
    let h = Health::evaluate(&input(Duration::minutes(45), Duration::minutes(10)));
    assert_eq!(h, Health::FeedDead { age_secs: 600 });
    assert!(h.reason().contains("feed is dead"));
}

#[test]
fn flowing_data_is_healthy_regardless_of_process_flags() {
    let mut i = input(Duration::minutes(2), Duration::seconds(3));
    i.process_up = false;
    i.socket_connected = false;
    assert!(
        Health::evaluate(&i).ok(),
        "the verdict comes from the flow, not from process flags"
    );
}

#[test]
fn killswitch_stops_live_but_never_shadow() {
    let mut k = Killswitch::default();
    assert!(!k.is_live_blocked());

    k.trip(TripReason::FeedStalled);
    assert!(k.is_live_blocked());
    assert!(
        !k.is_shadow_blocked(),
        "the measuring instrument must keep recording"
    );
    assert_eq!(k.reason(), Some(TripReason::FeedStalled));
}

#[test]
fn first_reason_is_kept_and_reset_clears_it() {
    let mut k = Killswitch::default();
    k.trip(TripReason::BalanceBelowFloor);
    k.trip(TripReason::DbWriteFailed);
    assert_eq!(
        k.reason(),
        Some(TripReason::BalanceBelowFloor),
        "the first reason is the real one"
    );

    k.reset();
    assert!(!k.is_live_blocked());
}

#[test]
fn every_stage_of_the_signal_path_is_measurable() {
    let m = Metrics::new();
    for s in STAGES {
        m.observe(s, 1.0);
    }
    for s in STAGES {
        assert_eq!(m.count_of(s), 1, "stage {s} must be measured");
    }
    assert_eq!(STAGES.len(), 4);
}

#[test]
fn verdicts_and_slippage_are_counted() {
    let m = Metrics::new();
    m.incr("signals_total", &[("verdict", "copy")]);
    m.incr("signals_total", &[("verdict", "copy")]);
    m.incr("signals_total", &[("verdict", "skip:slippage_exceeded")]);

    assert_eq!(m.counter("signals_total", &[("verdict", "copy")]), 2);
    assert_eq!(
        m.counter("signals_total", &[("verdict", "skip:slippage_exceeded")]),
        1
    );
    assert_eq!(
        m.counter("signals_total", &[("verdict", "skip:duplicate")]),
        0
    );

    m.observe("slippage_realized_vs_allowed", 0.4);
    m.observe("slippage_realized_vs_allowed", 0.8);
    m.observe("slippage_realized_vs_allowed", 0.6);
    assert_eq!(m.median("slippage_realized_vs_allowed"), Some(0.6));
}
