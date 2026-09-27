//! Full-jitter exponential backoff (AWS-style) for every Polymarket-bound
//! retry in the Rust stack.
//!
//! The delay is uniform in `[0, min(cap, base * 2^attempt)]`. Randomising the
//! WHOLE window (not "exponent ± a bit") is the point: a deterministic retry
//! ladder is itself a fingerprint, and two processes that catch the same 429
//! must not retry in lock-step (spec §A3).

use std::time::Duration;

/// The un-jittered ceiling for `attempt`: `min(cap, base * 2^attempt)`.
///
/// Separated from the random draw so it can be asserted exactly in tests.
#[must_use]
pub fn jitter_window(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let factor = 1_u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
    base.saturating_mul(factor).min(cap)
}

/// A uniformly random delay in `[0, `[`jitter_window`]`]`.
#[must_use]
pub fn full_jitter(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let window = jitter_window(attempt, base, cap);
    window.mul_f64(rand::random::<f64>())
}

#[cfg(test)]
mod tests {
    use super::{full_jitter, jitter_window};
    use std::time::Duration;

    #[test]
    fn window_doubles_then_caps() {
        let base = Duration::from_millis(500);
        let cap = Duration::from_secs(30);
        assert_eq!(jitter_window(0, base, cap), Duration::from_millis(500));
        assert_eq!(jitter_window(3, base, cap), Duration::from_secs(4));
        assert_eq!(jitter_window(10, base, cap), cap);
        // Must not panic or wrap on an absurd attempt count.
        assert_eq!(jitter_window(u32::MAX, base, cap), cap);
    }

    #[test]
    fn full_jitter_stays_inside_the_window() {
        let base = Duration::from_millis(500);
        let cap = Duration::from_secs(30);
        for attempt in 0..8 {
            for _ in 0..200 {
                assert!(full_jitter(attempt, base, cap) <= jitter_window(attempt, base, cap));
            }
        }
    }

    #[test]
    fn full_jitter_actually_varies() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        let draws: Vec<Duration> = (0..50).map(|_| full_jitter(5, base, cap)).collect();
        // A deterministic ladder would return 50 identical values.
        assert!(draws.iter().any(|d| *d != draws[0]));
    }
}
