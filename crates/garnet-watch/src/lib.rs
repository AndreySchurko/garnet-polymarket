//! The watcher over the trading process (invariant 45).
//!
//! Five background loops live inside `garnet-core`. A runtime hang, a deadlock on a
//! `Mutex` or an exhausted connection pool take out **both the trading and the
//! observation of trading** at once: invariant 11 says health is measured by flow
//! rather than by whether the process is alive — but it is the process itself doing
//! the measuring.
//!
//! Hence the rule: **the watcher does not live inside the process it watches**. A
//! separate binary, a separate unit, a separate exit code. It does not restart or stop
//! the trading process — it **reports**.

pub mod checks;

pub use checks::{Check, Report, Verdict};
