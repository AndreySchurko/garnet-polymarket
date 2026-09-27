//! The event bus, on NATS.
//!
//! Carried over from the predecessor without changing the logic. The boundary
//! of responsibility in Garnet is stricter than it was: **NATS carries events**
//! (`signal.detected`, `order.submitted`, `order.filled`, `position.settled`,
//! `alert.*`), **Redis holds state** (dedup, limits, cache). One event is never
//! written to both buses — in the predecessor their roles blurred, and that
//! cost us the ability to know which of them delivered what.
//!
//! The format is JSON; Decimal is serialised as a string.

pub mod client;
pub mod error;
pub mod subjects;

pub use client::Bus;
pub use error::BusError;
