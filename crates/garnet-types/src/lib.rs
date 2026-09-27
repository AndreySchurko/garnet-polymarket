//! The shared vocabulary.
//!
//! Only what turned out to be needed by **more than one crate** belongs here.
//! The predecessor's types (signals, events, risk, scoring) are not carried
//! over: Garnet has its own model, and copying someone else's vocabulary for
//! the sake of completeness is a reliable way to drag back the concepts we
//! abandoned.

pub mod market;
pub mod trading;

pub use market::{BookSnapshot, OutcomeIndex, PriceLevel};
pub use trading::{Mode, Side};
