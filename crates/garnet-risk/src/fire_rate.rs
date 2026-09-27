//! The entry rate limit per wallet (invariant 44).
//!
//! Paid for by a measurement on 06.09.2026: the bucket "11+ entries per wallet at
//! once" — 24 positions, half of the capital deployed and **94% of the entire
//! loss**, ROI -15.74% against +3.48% for the "1 entry" bucket over 244 closed
//! positions.
//!
//! Neither the slice window nor the exposure ceiling catches this, and neither
//! can: the slice window trims slices of **one order in one market**, the ceiling
//! counts money **within one event**, while here the leader fires a burst across
//! different markets — and passes both.
//!
//! The window closes **by our clock**, not by the leader's. That is the
//! difference from the slice window (invariant 29): there we interpret their
//! behaviour — one order or separate decisions — and measuring that by our clock
//! would mean measuring our own delivery latency. Here what is limited is **our
//! own rate of spending capital**, and that is measured in our time.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

pub struct FireRate {
    window_secs: i64,
    /// Zero **disables** the refusal. A fresh installation that refuses the very
    /// first signal is the most expensive failure in this project (invariant 32;
    /// the predecessor rejected 35 signals out of 35).
    limit: usize,
    seen: Mutex<HashMap<String, VecDeque<i64>>>,
}

impl FireRate {
    #[must_use]
    pub fn new(window_secs: i64, limit: usize) -> Self {
        Self {
            window_secs: window_secs.max(0),
            limit,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Whether the refusal is disabled. The counting happens regardless — see
    /// [`Self::record`].
    #[must_use]
    pub fn disabled(&self) -> bool {
        self.limit == 0 || self.window_secs == 0
    }

    /// How many entries the wallet has within the window as of `now`.
    #[must_use]
    pub fn count(&self, wallet: &str, now: i64) -> usize {
        let mut seen = self.seen.lock().unwrap();
        Self::prune(&mut seen, wallet, now, self.window_secs);
        seen.get(wallet).map_or(0, VecDeque::len)
    }

    /// Record an entry. `true` — the limit is reached, **this entry is not made**.
    ///
    /// The counting is kept even while the refusal is disabled. This is not a
    /// forgotten branch: a threshold is assigned from a measurement, not from a
    /// guess (invariant 33), and the queue depth has to be observed **before** the
    /// refusal is switched on. A disabled limiter that counts nothing leaves the
    /// operator exactly where they were — with a threshold there is nowhere to
    /// get.
    pub fn record(&self, wallet: &str, now: i64) -> bool {
        let mut seen = self.seen.lock().unwrap();
        Self::prune(&mut seen, wallet, now, self.window_secs);
        let q = seen.entry(wallet.to_string()).or_default();

        if self.limit > 0 && self.window_secs > 0 && q.len() >= self.limit {
            // A refused entry spends no capital, and it has no place in the queue:
            // otherwise one burst would lock the wallet for a whole window,
            // counting its own refusals as expenditure.
            return true;
        }
        q.push_back(now);
        false
    }

    /// Drop everything that has left the window.
    ///
    /// The boundary is **exclusive**: an entry exactly `window_secs` ago is
    /// already outside. An inclusive boundary would make the window a second
    /// longer than advertised, and a threshold set from a measurement would then
    /// mean something other than what was measured.
    fn prune(seen: &mut HashMap<String, VecDeque<i64>>, wallet: &str, now: i64, window: i64) {
        let Some(q) = seen.get_mut(wallet) else {
            return;
        };
        let edge = now - window;
        while q.front().is_some_and(|&t| t <= edge) {
            q.pop_front();
        }
        if q.is_empty() {
            seen.remove(wallet);
        }
    }
}

/// How many entries a limit of `limit` would have refused over history that has
/// already happened.
///
/// Computed **by running it through the limiter itself**, not by the formula
/// "depth greater than the limit". The difference is not cosmetic: a refused
/// entry does not enter the queue, so it does not deepen the ones that follow,
/// and counting by depths systematically overstates the refusals. A replay gives
/// the exact answer and by construction cannot diverge from production behaviour
/// — it is the same code.
///
/// `entries` must be **ordered by time**: a queue is a history, and a shuffled
/// history answers a different question.
#[must_use]
pub fn would_refuse(entries: &[(String, i64)], window_secs: i64, limit: usize) -> usize {
    let fr = FireRate::new(window_secs, limit);
    entries
        .iter()
        .filter(|(key, ts)| fr.record(key, *ts))
        .count()
}
