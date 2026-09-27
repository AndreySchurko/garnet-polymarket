//! The operator's manual stop against the automatics.
//!
//! There is one rule and it is not obvious: **the return of the feed does not
//! cancel the operator's decision**. The killswitch remembers one reason, and
//! without this rule the sequence "feed stalled -> operator pressed /kill -> feed
//! came back" would quietly resume live trading.

use garnet_risk::killswitch::TripReason;
use garnet_risk::manual_guard::{manual_guard, Manual};

#[test]
fn a_stop_is_raised_when_nothing_holds_it_yet() {
    assert_eq!(manual_guard(true, None), Manual::Trip);
}

#[test]
fn a_stop_is_released_only_if_it_was_ours() {
    assert_eq!(manual_guard(false, Some(TripReason::Manual)), Manual::Clear);
    // A feed stop is cleared by the feed, not by the lifting of an operator's stop.
    assert_eq!(
        manual_guard(false, Some(TripReason::FeedStalled)),
        Manual::Nothing
    );
    assert_eq!(manual_guard(false, None), Manual::Nothing);
}

#[test]
fn the_operator_outlives_an_automatic_reason() {
    // We are stopped by the feed, the operator presses /kill. The reason must
    // become the operator's: otherwise `feed_guard` will clear it itself as soon as
    // the feed returns, and live trading resumes without a single human decision.
    assert_eq!(
        manual_guard(true, Some(TripReason::FeedStalled)),
        Manual::Trip
    );
}

#[test]
fn a_standing_manual_stop_is_left_alone() {
    // Re-tripping every 30 seconds would mean an alert every 30 seconds.
    assert_eq!(
        manual_guard(true, Some(TripReason::Manual)),
        Manual::Nothing
    );
}
