//! Subject names. The only producer of events is `garnet-core`.
/// A leader trade has been seen and recorded.
pub const SIGNAL_DETECTED: &str = "signal.detected";
/// An order has been submitted.
pub const ORDER_SUBMITTED: &str = "order.submitted";
/// An order has been filled, fully or partially.
pub const ORDER_FILLED: &str = "order.filled";
/// A position has been resolved and paid out.
pub const POSITION_SETTLED: &str = "position.settled";

/// Alerts to the operator.
pub const ALERT_KILLSWITCH: &str = "alert.killswitch";
pub const ALERT_FEED_STALLED: &str = "alert.feed_stalled";
pub const ALERT_RECONCILE_DIVERGENCE: &str = "alert.reconcile_divergence";
/// A token balance could not be read (invariant 47).
///
/// A subject separate from the divergence, deliberately: "the chain says
/// something other than the ledger" and "the chain could not be looked at" are
/// different news, and neither cancels the other. Silence about the second would
/// read as a successful check.
pub const ALERT_RECONCILE_UNREADABLE: &str = "alert.reconcile_unreadable";
pub const ALERT_LOW_BALANCE: &str = "alert.low_balance";
pub const ALERT_ORDER_REJECTED: &str = "alert.order_rejected";
