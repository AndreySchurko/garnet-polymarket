//! What to do about the health of the feed.
//!
//! The decision is separated from the loop that carries it out: the loop itself is
//! a timer and two calls, whereas the rules cost the predecessor an investigation
//! of their own.
//!
//! The chief rule is suppression. On 31.08.2026 the `activity/trades` topic went
//! down platform-wide for hours. A killswitch on a stalled feed at that moment
//! would have halted the shadow run long after the feed came back: it is lifted by
//! hand. The predecessor learned not to arm it when there is nothing to lose, and
//! that rule moves across here.

use garnet_risk::feed_guard::{feed_guard, Guard};
use garnet_risk::health::Health;
use garnet_risk::killswitch::TripReason;

#[test]
fn a_dead_feed_with_money_at_risk_trips_the_switch() {
    let g = feed_guard(&Health::FeedDead { age_secs: 400 }, true, None);
    assert_eq!(g, Guard::Trip(TripReason::FeedStalled));
}

#[test]
fn a_dead_feed_with_nothing_at_risk_does_not_trip() {
    // A platform-wide outage must not silence the measuring instrument: there is
    // nothing to lose, and the killswitch would have to be cleared by hand.
    let g = feed_guard(&Health::FeedDead { age_secs: 4000 }, false, None);
    assert_eq!(g, Guard::Nothing);
}

#[test]
fn quiet_leaders_are_not_a_reason_to_stop() {
    // The control topic is alive: the socket is delivering, it is simply that
    // nobody is trading. That is a state of the market, not a failure of ours.
    let g = feed_guard(&Health::NoTrades { age_secs: 3600 }, true, None);
    assert_eq!(g, Guard::Nothing);
}

#[test]
fn a_recovered_feed_clears_the_switch_it_set_itself() {
    let g = feed_guard(&Health::Ok, true, Some(TripReason::FeedStalled));
    assert_eq!(g, Guard::Clear);
}

#[test]
fn a_recovered_feed_never_clears_someone_elses_stop() {
    // The operator stopped trading by hand — the return of the feed does not
    // cancel that decision. Otherwise the bot would lift a manual stop by itself.
    for reason in [
        TripReason::Manual,
        TripReason::BalanceBelowFloor,
        TripReason::DbWriteFailed,
        TripReason::ConsecutiveOrderFailures(5),
    ] {
        assert_eq!(
            feed_guard(&Health::Ok, true, Some(reason)),
            Guard::Nothing,
            "reason {reason:?} is cleared only by whoever set it"
        );
    }
}

#[test]
fn an_already_tripped_feed_switch_is_not_tripped_twice() {
    let g = feed_guard(
        &Health::FeedDead { age_secs: 900 },
        true,
        Some(TripReason::FeedStalled),
    );
    assert_eq!(g, Guard::Nothing, "re-arming changes nothing");
}

#[test]
fn a_healthy_feed_with_no_stop_does_nothing() {
    assert_eq!(feed_guard(&Health::Ok, true, None), Guard::Nothing);
}
