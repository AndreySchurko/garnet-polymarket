//! Error types for the CLOB v2 client.

use thiserror::Error;

/// All errors that can originate from `garnet-clob`.
#[derive(Debug, Error)]
pub enum ClobError {
    /// HTTP response carried a non-success status code.
    #[error("CLOB HTTP {status}: {message}")]
    Http { status: u16, message: String },

    /// Authentication or credential setup failed.
    #[error("CLOB auth: {0}")]
    Auth(String),

    /// WebSocket transport error.
    #[error("CLOB WebSocket: {0}")]
    WebSocket(String),

    /// JSON serialisation / deserialisation failed.
    #[error("CLOB JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// Underlying `reqwest` network error.
    #[error("CLOB network: {0}")]
    Network(#[from] reqwest::Error),

    /// Requested order or resource was not found.
    #[error("CLOB not found: {0}")]
    NotFound(String),

    /// Orderbook snapshot is too old to be used for trading.
    ///
    /// Triggered when `now − book.timestamp > max_ms`.
    #[error("stale orderbook for {token_id}: {age_ms}ms old (max {max_ms}ms)")]
    StaleQuote {
        token_id: String,
        age_ms: u64,
        max_ms: u64,
    },

    /// CLOB returned HTTP 429 — explicit rate-limit variant for clarity.
    #[error("CLOB rate limited (429)")]
    RateLimited {
        /// Seconds the server asked us to wait, from its `Retry-After` header.
        /// `None` when it sent no header (then back off with full jitter).
        retry_after_secs: Option<f64>,
    },

    /// Cloudflare Bot-Management blocked the request (403/503 + cf-ray, non-JSON
    /// body). Distinct from [`ClobError::Http`]: the endpoint is up, we are
    /// blocked — retrying harder worsens the ban. Arms `KS_EGRESS_BLOCKED`.
    #[error("CLOB egress blocked by Cloudflare: {0}")]
    EgressBlocked(String),

    /// Configuration or environment variable problem.
    #[error("CLOB config: {0}")]
    Config(String),

    /// WebSocket has been disconnected longer than the kill-switch threshold.
    #[error("CLOB WS disconnected for {duration_secs}s — kill-switch threshold reached")]
    WsTimeout { duration_secs: u64 },

    /// Internal async channel was closed unexpectedly.
    #[error("internal channel closed")]
    ChannelClosed,

    /// NATS bus publish failed.
    #[error("NATS bus error: {0}")]
    Bus(#[from] garnet_bus::BusError),

    /// An error surfaced by the official `polymarket_client_sdk_v2` order
    /// build/sign/post path.
    ///
    /// Deliberately **not** retryable: a failed order submission must not be
    /// re-sent automatically, since the original may have reached the exchange
    /// (double-submit risk). The caller re-evaluates on the next signal.
    #[error("CLOB SDK: {0}")]
    Sdk(String),

    /// A field from the CLOB wire format could not be parsed.
    ///
    /// Distinct from [`Self::Json`] (whole-message deserialise failure):
    /// `Parse` is for individual `Decimal` / timestamp fields that arrived as
    /// strings and turned out to be malformed. Returned instead of silently
    /// substituting `Decimal::ZERO`, which would let mis-priced quotes feed
    /// downstream slippage checks.
    #[error("CLOB parse: {0}")]
    Parse(String),
}

impl ClobError {
    /// Returns `true` if the error is transient and the operation **may** be retried.
    ///
    /// Permanent errors (400, 401, 403, 404) return `false`; the caller should
    /// not retry without fixing the request.
    ///
    /// # Examples
    ///
    /// ```
    /// use garnet_clob::error::ClobError;
    ///
    /// assert!(ClobError::RateLimited { retry_after_secs: Some(7.0) }.is_retryable());
    /// assert!(ClobError::Http { status: 500, message: "internal".into() }.is_retryable());
    /// assert!(ClobError::Http { status: 429, message: "rate".into() }.is_retryable());
    /// assert!(!ClobError::Http { status: 400, message: "bad request".into() }.is_retryable());
    /// assert!(!ClobError::Auth("wrong key".into()).is_retryable());
    /// assert!(!ClobError::NotFound("order_123".into()).is_retryable());
    /// ```
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http { status, .. } => *status == 429 || *status >= 500,
            Self::Network(_) | Self::RateLimited { .. } => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_http_429() {
        assert!(ClobError::Http {
            status: 429,
            message: "rate".into()
        }
        .is_retryable());
    }

    #[test]
    fn retryable_http_500() {
        assert!(ClobError::Http {
            status: 500,
            message: "err".into()
        }
        .is_retryable());
    }

    #[test]
    fn retryable_503() {
        assert!(ClobError::Http {
            status: 503,
            message: "down".into()
        }
        .is_retryable());
    }

    #[test]
    fn not_retryable_400() {
        assert!(!ClobError::Http {
            status: 400,
            message: "bad".into()
        }
        .is_retryable());
    }

    #[test]
    fn not_retryable_401() {
        assert!(!ClobError::Http {
            status: 401,
            message: "unauth".into()
        }
        .is_retryable());
    }

    #[test]
    fn not_retryable_404() {
        assert!(!ClobError::Http {
            status: 404,
            message: "nf".into()
        }
        .is_retryable());
    }

    #[test]
    fn not_retryable_auth() {
        assert!(!ClobError::Auth("wrong key".into()).is_retryable());
    }

    #[test]
    fn not_retryable_not_found() {
        assert!(!ClobError::NotFound("order_id".into()).is_retryable());
    }

    #[test]
    fn retryable_rate_limited_variant() {
        assert!(ClobError::RateLimited {
            retry_after_secs: None
        }
        .is_retryable());
    }

    #[test]
    fn error_display_not_empty() {
        let e = ClobError::Config("POLY_ADDRESS not set".into());
        assert!(!e.to_string().is_empty());
    }
}
