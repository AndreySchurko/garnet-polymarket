//! What to do about the health of the feed.
//!
//! The decision is separated from the loop that carries it out: the loop is a
//! timer and two calls, while the rules cost the predecessor an investigation
//! of their own.
//!
//! The suppression rule was paid for on 31.08.2026: the `activity/trades` topic
//! went down platform-wide for hours, from every IP at once. A killswitch on a
//! stalled feed at that moment would have halted the shadow run long after the
//! feed came back — it is lifted by hand. If there is nothing to lose, silence on
//! the feed is not grounds for a stop.

use crate::health::Health;
use crate::killswitch::TripReason;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Arm the killswitch. It stops live only (invariant 12).
    Trip(TripReason),
    /// Clear it — but only the stop the feed itself set.
    Clear,
    Nothing,
}

/// The decision for the current health.
///
/// `at_risk` — whether there is anything to lose: an enabled live wallet or an
/// open live position. `tripped_by` — what the killswitch is armed by right now.
#[must_use]
pub fn feed_guard(health: &Health, at_risk: bool, tripped_by: Option<TripReason>) -> Guard {
    match health {
        // We have gone deaf: the control topic is silent along with the trades.
        Health::FeedDead { .. } => {
            if tripped_by.is_some() || !at_risk {
                Guard::Nothing
            } else {
                Guard::Trip(TripReason::FeedStalled)
            }
        }
        // The socket is delivering, but the leaders are quiet. That is a state of
        // the market, not a failure of ours, and it does not stop trading.
        Health::NoTrades { .. } => Guard::Nothing,
        // Only our own stop is cleared: the return of the feed does not cancel the
        // operator's manual stop.
        Health::Ok => match tripped_by {
            Some(TripReason::FeedStalled) => Guard::Clear,
            _ => Guard::Nothing,
        },
    }
}
