//! Counters and histograms.
//!
//! What gets measured is what decisions are made from: latency per stage of the
//! signal path, verdicts, order statuses, and actual slippage against the
//! permitted amount.

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct Metrics {
    counters: Mutex<HashMap<String, u64>>,
    histograms: Mutex<HashMap<String, Vec<f64>>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn incr(&self, name: &str, labels: &[(&str, &str)]) {
        let key = key_of(name, labels);
        *self.counters.lock().unwrap().entry(key).or_insert(0) += 1;
    }

    pub fn observe(&self, name: &str, value: f64) {
        self.histograms
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .push(value);
    }

    pub fn counter(&self, name: &str, labels: &[(&str, &str)]) -> u64 {
        *self
            .counters
            .lock()
            .unwrap()
            .get(&key_of(name, labels))
            .unwrap_or(&0)
    }

    pub fn count_of(&self, histogram: &str) -> usize {
        self.histograms
            .lock()
            .unwrap()
            .get(histogram)
            .map_or(0, Vec::len)
    }

    /// The median — what we compare latency against the 2 s target with.
    pub fn median(&self, histogram: &str) -> Option<f64> {
        let guard = self.histograms.lock().unwrap();
        let mut v = guard.get(histogram)?.clone();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Some(v[v.len() / 2])
    }
}

fn key_of(name: &str, labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        return name.to_string();
    }
    let mut s = String::from(name);
    for (k, v) in labels {
        s.push_str(&format!("|{k}={v}"));
    }
    s
}

/// The stages of the signal path. There are four, and all of them must be
/// measured.
pub const STAGES: [&str; 4] = [
    "trade_to_seen",
    "seen_to_signal",
    "signal_to_submitted",
    "submitted_to_filled",
];
