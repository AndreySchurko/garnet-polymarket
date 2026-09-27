//! The killswitch stops **live only**.
//!
//! Shadow keeps recording whatever the trigger: it is a measuring instrument, and
//! losing its records at precisely the moment of an emergency would deprive us of
//! data exactly where it matters most.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TripReason {
    FeedStalled,
    ConsecutiveOrderFailures(u32),
    BalanceBelowFloor,
    DbWriteFailed,
    /// The daily loss has reached its limit. It latches for the day and lives in
    /// the database: a stop that a restart undoes is not a stop.
    LossLimit,
    Manual,
}

impl TripReason {
    pub fn as_str(self) -> &'static str {
        match self {
            TripReason::FeedStalled => "the feed stalled",
            TripReason::ConsecutiveOrderFailures(_) => "a run of order failures",
            TripReason::BalanceBelowFloor => "balance below the floor",
            TripReason::DbWriteFailed => "a database write failed",
            TripReason::LossLimit => "the daily loss reached its limit",
            TripReason::Manual => "stopped by the operator",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Killswitch {
    tripped: Option<TripReason>,
}

impl Killswitch {
    pub fn trip(&mut self, reason: TripReason) {
        if self.tripped.is_none() {
            self.tripped = Some(reason);
        }
    }

    pub fn reset(&mut self) {
        self.tripped = None;
    }

    pub fn is_live_blocked(&self) -> bool {
        self.tripped.is_some()
    }

    /// Always false. The method exists so that this rule is visible in the code
    /// and cannot be changed "by accident" at the call site.
    pub fn is_shadow_blocked(&self) -> bool {
        false
    }

    pub fn reason(&self) -> Option<TripReason> {
        self.tripped
    }
}
