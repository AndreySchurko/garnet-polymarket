//! The loss stop.
//!
//! The killswitch's five trip reasons were all mechanical: the feed, a run of
//! exchange failures, the balance floor, a failed write, the manual stop. Not one
//! of them was about money — a bot that merely loses did not stop on its own.
//!
//! A latch for the day, not a "current loss": a stop that lifts on the very first
//! winning position resumes trading on precisely the day it was decided to stop.
//! And the latch lives in the database (invariants 20 and 24): a restart would
//! lift it silently, and a restart after an incident is the most likely event of
//! the day.

use chrono::NaiveDate;
use garnet_risk::killswitch::TripReason;
use garnet_risk::loss_stop::{loss_guard, LossAction};
use rust_decimal_macros::dec;

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 9, 6).unwrap()
}

fn yesterday() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 9, 5).unwrap()
}

/// A zero limit means "there is no stop", not "stop at the first cent".
#[test]
fn a_zero_limit_is_off() {
    assert_eq!(
        loss_guard(dec!(-9999), dec!(0), None, today(), None),
        LossAction::Nothing
    );
}

#[test]
fn a_loss_beyond_the_limit_arms_the_stop() {
    assert_eq!(
        loss_guard(dec!(-50.01), dec!(50), None, today(), None),
        LossAction::Arm
    );
}

#[test]
fn a_loss_within_the_limit_does_nothing() {
    assert_eq!(
        loss_guard(dec!(-49.99), dec!(50), None, today(), None),
        LossAction::Nothing
    );
}

/// The limit itself is already the limit: "no more than fifty" includes fifty.
#[test]
fn the_limit_itself_arms() {
    assert_eq!(
        loss_guard(dec!(-50), dec!(50), None, today(), None),
        LossAction::Arm
    );
}

/// Today's latch — hold, even if the day recovered. Otherwise the very first
/// winning position would bring trading back on the day it was stopped.
#[test]
fn a_recovery_does_not_release_the_same_day() {
    assert_eq!(
        loss_guard(
            dec!(120),
            dec!(50),
            Some(today()),
            today(),
            Some(TripReason::LossLimit)
        ),
        LossAction::Keep
    );
}

/// A restart wipes the process's memory but not the latch: the stop has to be
/// armed again.
#[test]
fn a_restart_finds_the_latch_and_trips_again() {
    assert_eq!(
        loss_guard(dec!(-70), dec!(50), Some(today()), today(), None),
        LossAction::Keep
    );
}

/// A new day, a new count. Yesterday's latch clears the stop.
#[test]
fn a_new_day_releases_the_stop() {
    assert_eq!(
        loss_guard(
            dec!(0),
            dec!(50),
            Some(yesterday()),
            today(),
            Some(TripReason::LossLimit)
        ),
        LossAction::Release
    );
}

/// We do not clear someone else's stop: the operator's manual stop survives the
/// turn of the day.
#[test]
fn a_new_day_does_not_release_somebody_elses_stop() {
    assert_eq!(
        loss_guard(
            dec!(0),
            dec!(50),
            Some(yesterday()),
            today(),
            Some(TripReason::Manual)
        ),
        LossAction::Nothing
    );
    assert_eq!(
        loss_guard(
            dec!(0),
            dec!(50),
            Some(yesterday()),
            today(),
            Some(TripReason::FeedStalled)
        ),
        LossAction::Nothing
    );
}

/// A profitable day arms nothing.
#[test]
fn a_profitable_day_is_quiet() {
    assert_eq!(
        loss_guard(dec!(15), dec!(50), None, today(), None),
        LossAction::Nothing
    );
}
