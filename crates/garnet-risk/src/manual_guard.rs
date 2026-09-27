//! What to do about the operator's manual stop.
//!
//! Separated from the loop for the same reason as [`crate::feed_guard`]: the rule
//! is short but easy to break, and the price of a mistake is live trading
//! continuing after a human stopped it.
//!
//! The killswitch remembers **one** reason, and the order of application
//! matters: the automatics first ([`crate::feed_guard`]), then this rule.
//! Otherwise the sequence "feed stalled -> operator pressed /kill -> feed came
//! back" would end with `feed_guard` clearing its own stop and trading resuming.

use crate::killswitch::TripReason;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manual {
    /// Arm the stop as the operator's.
    Trip,
    /// Clear it — but only the operator's.
    Clear,
    Nothing,
}

/// The decision about the manual stop.
///
/// `stopped` — what is recorded in `controls.manual_stop`; `tripped_by` — what
/// the killswitch is armed by right now.
#[must_use]
pub fn manual_guard(stopped: bool, tripped_by: Option<TripReason>) -> Manual {
    match (stopped, tripped_by) {
        // Already stopped by the operator's decision: repeating would mean an
        // alert on every tick.
        (true, Some(TripReason::Manual)) => Manual::Nothing,
        // A human's decision overrides the automatic reason and outlives it.
        (true, _) => Manual::Trip,
        (false, Some(TripReason::Manual)) => Manual::Clear,
        (false, _) => Manual::Nothing,
    }
}
