//! Optional application-level egress proxy for Polymarket-bound traffic.
//!
//! This is the third egress lane and is **off by default** (ADR-0001 rev.2):
//! the base defence is Cloudflare hygiene (shared rate limiting, `Retry-After`,
//! jitter), and the network-level escape hatch is `GARNET_EGRESS_MODE=proxy|wg`
//! which routes the whole `garnet` uid and needs no cooperation from this
//! module. Keep it for the case where only the HTTP/WS lane should be diverted.
//!
//! It cannot be a guarantee on its own: the pinned SDK builds its own
//! `reqwest` client and has no proxy field, so order submission never passes
//! through here. Preflight therefore measures the observed egress via
//! `check_geoblock()` instead of trusting this variable.
//!
//! Configuration is a single env var, `POLYMARKET_PROXY_URL`. It must be a
//! SOCKS5 URL (`socks5://` or `socks5h://`) so the same endpoint serves both
//! the `reqwest` HTTP clients and the raw WebSocket dial. When unset, all
//! clients connect directly. Setting it together with
//! `GARNET_EGRESS_MODE=proxy` is a double hop and preflight rejects it.

use crate::error::ClobError;

/// Env var holding the Polymarket egress proxy URL (SOCKS5), or unset/empty.
const PROXY_ENV: &str = "POLYMARKET_PROXY_URL";

/// Return the configured Polymarket proxy URL, or `None` when unset/blank.
#[must_use]
pub fn proxy_url() -> Option<String> {
    match std::env::var(PROXY_ENV) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_owned()),
        _ => None,
    }
}

/// Apply the Polymarket proxy to a `reqwest` client builder when configured.
///
/// # Errors
///
/// Returns [`ClobError::Config`] if `POLYMARKET_PROXY_URL` is set but cannot be
/// parsed as a proxy URL by `reqwest`.
pub(crate) fn apply_reqwest_proxy(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::ClientBuilder, ClobError> {
    match proxy_url() {
        Some(url) => {
            let proxy = reqwest::Proxy::all(&url)
                .map_err(|e| ClobError::Config(format!("invalid {PROXY_ENV} '{url}': {e}")))?;
            Ok(builder.proxy(proxy))
        }
        None => Ok(builder),
    }
}

// The SOCKS part for WebSocket is not carried over along with `orderbook_ws`: Garnet has
// no maker track, and tokio-tungstenite and tokio-socks leave with it.
