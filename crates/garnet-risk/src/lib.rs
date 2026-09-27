//! The risk layer: only the parts that do not cut signals.
//!
//! What remains from the predecessor is the killswitch, the feed watchdog and
//! feed lag. `exposure`, `circuit_breaker` and `triggers` are not carried over:
//! those are indirect filters, and Garnet copies unconditionally.

pub mod feed_guard;
pub mod fire_rate;
pub mod health;
pub mod killswitch;
pub mod loss_stop;
pub mod manual_guard;
pub mod metrics;
