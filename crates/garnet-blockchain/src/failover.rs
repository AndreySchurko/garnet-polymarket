//! RPC endpoint failover with per-URL health tracking.
//!
//! On first error from an endpoint that endpoint is marked unhealthy for
//! [`UNHEALTHY_DURATION_SECS`] seconds.  The next healthy URL in the list is
//! returned.  When *all* URLs are unhealthy, [`BcError::AllRpcsUnhealthy`] is
//! returned so the caller can emit `polygon.rpc.all_lost`.

use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::error::BcError;

/// How long a failing RPC endpoint stays out of rotation (§7.6.3).
const UNHEALTHY_DURATION_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Per-URL health state
// ---------------------------------------------------------------------------

struct UrlSlot {
    url: reqwest::Url,
    /// `Some(instant)` → unhealthy until that instant; `None` → healthy.
    unhealthy_until: Option<Instant>,
}

// ---------------------------------------------------------------------------
// RpcFailover
// ---------------------------------------------------------------------------

/// Round-robin RPC failover across a list of provider URLs.
///
/// All methods are async-safe via a `tokio::sync::Mutex`.
pub struct RpcFailover {
    slots: Mutex<Vec<UrlSlot>>,
}

impl std::fmt::Debug for RpcFailover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcFailover").finish_non_exhaustive()
    }
}

impl RpcFailover {
    /// Construct from a comma-separated list of RPC URLs.
    ///
    /// # Errors
    ///
    /// Returns [`BcError::Config`] if the list is empty or any URL fails to parse.
    pub fn from_csv(csv: &str) -> Result<Self, BcError> {
        let slots: Result<Vec<_>, _> = csv
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<reqwest::Url>()
                    .map(|url| UrlSlot {
                        url,
                        unhealthy_until: None,
                    })
                    .map_err(|e| BcError::Config(format!("invalid RPC URL '{s}': {e}")))
            })
            .collect();

        let slots = slots?;
        if slots.is_empty() {
            return Err(BcError::Config("POLYGON_RPC_URLS is empty".into()));
        }
        Ok(Self {
            slots: Mutex::new(slots),
        })
    }

    /// Return the first currently-healthy URL.
    ///
    /// A URL that was marked unhealthy and whose timeout has since expired is
    /// automatically re-admitted to the pool.
    ///
    /// # Errors
    ///
    /// Returns [`BcError::AllRpcsUnhealthy`] when every URL is still within its
    /// cool-down window.
    pub async fn current_url(&self) -> Result<reqwest::Url, BcError> {
        let now = Instant::now();
        let mut slots = self.slots.lock().await;

        for slot in slots.iter_mut() {
            match slot.unhealthy_until {
                None => return Ok(slot.url.clone()),
                Some(until) if until <= now => {
                    // Cool-down elapsed — re-admit.
                    slot.unhealthy_until = None;
                    debug!(url = %slot.url, "RPC endpoint re-admitted after cool-down");
                    return Ok(slot.url.clone());
                }
                _ => {}
            }
        }

        warn!("all {} RPC endpoints are unhealthy", slots.len());
        Err(BcError::AllRpcsUnhealthy)
    }

    /// Mark `url` as unhealthy for [`UNHEALTHY_DURATION_SECS`] seconds.
    pub async fn mark_unhealthy(&self, url: &reqwest::Url) {
        let until = Instant::now() + Duration::from_secs(UNHEALTHY_DURATION_SECS);
        let mut slots = self.slots.lock().await;
        for slot in slots.iter_mut() {
            if slot.url == *url {
                slot.unhealthy_until = Some(until);
                warn!(url = %url, "RPC endpoint marked unhealthy for {UNHEALTHY_DURATION_SECS}s");
                return;
            }
        }
    }

    /// Number of configured URLs.
    pub async fn len(&self) -> usize {
        self.slots.lock().await.len()
    }

    /// Returns `true` when no URLs are configured.
    pub async fn is_empty(&self) -> bool {
        self.slots.lock().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_failover(urls: &[&str]) -> RpcFailover {
        let csv = urls.join(",");
        RpcFailover::from_csv(&csv).unwrap()
    }

    #[tokio::test]
    async fn first_url_returned_when_healthy() {
        let fo = make_failover(&["http://rpc1.example.com", "http://rpc2.example.com"]);
        let url = fo.current_url().await.unwrap();
        assert_eq!(url.as_str(), "http://rpc1.example.com/");
    }

    #[tokio::test]
    async fn falls_back_after_mark_unhealthy() {
        let fo = make_failover(&["http://rpc1.example.com", "http://rpc2.example.com"]);
        let first = fo.current_url().await.unwrap();
        fo.mark_unhealthy(&first).await;
        let second = fo.current_url().await.unwrap();
        assert!(second.as_str().contains("rpc2"));
    }

    #[tokio::test]
    async fn all_unhealthy_returns_error() {
        let fo = make_failover(&["http://rpc1.example.com"]);
        let url = fo.current_url().await.unwrap();
        fo.mark_unhealthy(&url).await;
        let err = fo.current_url().await.unwrap_err();
        assert!(matches!(err, BcError::AllRpcsUnhealthy));
    }

    #[test]
    fn empty_csv_returns_config_error() {
        let err = RpcFailover::from_csv("").unwrap_err();
        assert!(matches!(err, BcError::Config(_)));
    }

    #[test]
    fn invalid_url_returns_config_error() {
        let err = RpcFailover::from_csv("not-a-url").unwrap_err();
        assert!(matches!(err, BcError::Config(_)));
    }
}
