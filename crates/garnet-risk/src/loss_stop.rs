//! The daily loss stop.
//!
//! Until 06.09.2026 the killswitch's trip reasons were all mechanical: the feed,
//! a run of exchange failures, the balance floor, a failed write, the manual
//! stop. Not one of them was about money — a bot that merely loses did not stop
//! on its own.
//!
//! Two things here are not obvious, and both were paid for by earlier defects.
//!
//! **A latch for the day, not a "current loss".** A stop that lifts on the first
//! winning position resumes trading on precisely the day it was decided to stop.
//!
//! **The latch lives in the database**, not in process memory (invariants 20 and
//! 24): a restart would lift it silently, and a restart after a bad day is the
//! most likely event of that day.

use crate::killswitch::TripReason;
use chrono::NaiveDate;
use rust_decimal::Decimal;

/// What to do about the loss stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossAction {
    /// Arm it and record today's latch.
    Arm,
    /// The latch is today's: stay armed. Separate from [`Arm`] because there is
    /// no point writing it a second time, while arming after a restart is
    /// necessary.
    Keep,
    /// The day has changed: clear our own stop. Anyone else's is left alone.
    Release,
    Nothing,
}

/// `limit` — the positive magnitude of the acceptable daily loss; zero disables
/// the stop. `pnl_today` — the realised live result over the same day: closed
/// positions, without revaluing open ones. Revaluation depends on the mid of the
/// book, which some positions simply do not have, and a stop triggered by a
/// missing price is the worst kind of false positive.
#[must_use]
pub fn loss_guard(
    pnl_today: Decimal,
    limit: Decimal,
    latched_on: Option<NaiveDate>,
    today: NaiveDate,
    tripped_by: Option<TripReason>,
) -> LossAction {
    if limit <= Decimal::ZERO {
        return LossAction::Nothing;
    }
    if latched_on == Some(today) {
        return LossAction::Keep;
    }
    if pnl_today <= -limit {
        return LossAction::Arm;
    }
    // The latch is not today's, yet the stop is still standing — and it is ours.
    if tripped_by == Some(TripReason::LossLimit) {
        return LossAction::Release;
    }
    LossAction::Nothing
}
