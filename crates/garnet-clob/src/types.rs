//! Internal types used by the CLOB v2 client.
//!
//! `BookSnapshot` and `PriceLevel` live in `garnet-types`; this module holds
//! CLOB-specific request / response types and credential helpers.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::ClobError;

/// Side of a CLOB order.
///
/// Maps to the integer field `side` in the Polymarket CLOB v2 order struct
/// (0 = BUY, 1 = SELL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Side {
    /// Buy (taker of asks).  Always used for NO purchases in Garnet.
    Buy,
    /// Sell (taker of bids).  Used only for manual position closes.
    Sell,
}

impl Side {
    /// Integer representation used in the CLOB v2 order struct.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Buy => 0,
            Self::Sell => 1,
        }
    }
}

/// Status of a CLOB order returned by the REST API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClobOrderStatus {
    /// Open in the book.
    Open,
    /// Matched; awaiting on-chain settlement.
    Matched,
    /// Delayed by the exchange.
    Delayed,
    /// Cancelled.
    Cancelled,
    /// Expired (time-in-force elapsed).
    Expired,
}

/// How long an order lives.
///
/// The predecessor could only do `Gtc` — such an order rests in the book and
/// makes us a maker, which is the opposite of copying the leader's taker entry.
/// Copying uses `Fak` only: take whatever is on offer right now, cancel the
/// remainder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderKind {
    /// Good-til-cancelled: rests in the book.
    Gtc,
    /// Fill-and-kill: immediate execution, the remainder is cancelled.
    Fak,
}

impl OrderKind {
    /// The string for the CLOB request body.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            OrderKind::Gtc => "GTC",
            OrderKind::Fak => "FAK",
        }
    }
}

/// Response from the CLOB when an order is successfully created.
///
/// # Examples
///
/// ```
/// use chrono::Utc;
/// use rust_decimal_macros::dec;
/// use garnet_clob::types::{ClobOrderStatus, OrderResponse, Side};
///
/// let resp = OrderResponse {
///     order_id:   "ord_abc123".into(),
///     status:     ClobOrderStatus::Open,
///     token_id:   "0xno_token".into(),
///     price:      dec!(0.62),
///     size:       dec!(100.0),
///     side:       Side::Buy,
///     created_at: Utc::now(),
/// };
/// assert_eq!(resp.order_id, "ord_abc123");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResponse {
    /// Exchange-assigned order ID.
    pub order_id: String,
    /// Current status immediately after placement.
    pub status: ClobOrderStatus,
    /// ERC-1155 token ID that was ordered.
    pub token_id: String,
    /// Limit price. Serialised as string.
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    /// Order size in shares. Serialised as string.
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    /// Order side.
    pub side: Side,
    /// UTC creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Full information about an existing CLOB order, returned by `GET /order/{id}`.
///
/// # Examples
///
/// ```
/// use chrono::Utc;
/// use rust_decimal_macros::dec;
/// use garnet_clob::types::{ClobOrderStatus, OrderInfo, Side};
///
/// let info = OrderInfo {
///     order_id:       "ord_abc123".into(),
///     token_id:       "0xno_token".into(),
///     price:          dec!(0.62),
///     original_size:  dec!(100.0),
///     size_matched:   dec!(40.0),
///     size_remaining: dec!(60.0),
///     side:           Side::Buy,
///     status:         ClobOrderStatus::Open,
///     created_at:     Utc::now(),
///     updated_at:     Utc::now(),
/// };
/// assert_eq!(info.size_matched + info.size_remaining, info.original_size);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderInfo {
    /// Exchange-assigned order ID.
    pub order_id: String,
    /// ERC-1155 token ID.
    pub token_id: String,
    /// Limit price. Serialised as string.
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    /// Total size at order creation. Serialised as string.
    #[serde(with = "rust_decimal::serde::str")]
    pub original_size: Decimal,
    /// Shares filled so far. Serialised as string.
    #[serde(with = "rust_decimal::serde::str")]
    pub size_matched: Decimal,
    /// Shares still open. Serialised as string.
    #[serde(with = "rust_decimal::serde::str")]
    pub size_remaining: Decimal,
    /// Order side.
    pub side: Side,
    /// Current lifecycle status.
    pub status: ClobOrderStatus,
    /// UTC creation timestamp.
    pub created_at: DateTime<Utc>,
    /// UTC last-updated timestamp.
    pub updated_at: DateTime<Utc>,
}

/// One of our own executed trades, as `GET /data/trades` reports it.
///
/// The endpoint is authenticated and scoped to us, so every trade here involved
/// an order of ours — but which one is not a single field: we are the taker in
/// [`taker_order_id`](Self::taker_order_id) and one of the makers in
/// [`maker_order_ids`](Self::maker_order_ids), which also lists the other side's
/// orders. Reconciliation therefore asks whether *any* of those ids is one we
/// hold a row for; a trade where none is, is a fill nobody booked (X-2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeInfo {
    /// Exchange-assigned trade ID. Stable, so an alert can be raised once.
    pub trade_id: String,
    /// The taker order of the match.
    pub taker_order_id: String,
    /// Every maker order in the match — ours and the counterparties'.
    pub maker_order_ids: Vec<String>,
    /// Condition ID of the market.
    pub market: String,
    /// ERC-1155 token ID.
    pub token_id: String,
    /// Side of the trade as reported for the taker.
    pub side: Side,
    /// Shares traded. Serialised as string.
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    /// Traded price. Serialised as string.
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    /// UTC match time.
    pub match_time: DateTime<Utc>,
}

impl TradeInfo {
    /// Every order id this trade could have been ours through.
    pub fn order_ids(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.taker_order_id.as_str())
            .chain(self.maker_order_ids.iter().map(String::as_str))
            .filter(|id| !id.is_empty())
    }
}

/// Credentials required to authenticate with the Polymarket CLOB v2 REST API.
///
/// Loaded from process environment (see `.env.example`).  API key credentials
/// (`api_key`, `api_secret`, `passphrase`) are obtained after completing L1
/// authentication with the private key.  For connectivity smoke tests, only
/// `address` and the API key triple are required.
///
/// # Security
///
/// `api_secret` and `passphrase` are used in HMAC computation and must **never**
/// be logged.  Redact them as `***…XXXX` in any diagnostic output.
#[derive(Clone)]
pub struct ClobCredentials {
    /// Ethereum address of the trading account (0x-prefixed hex).
    pub address: String,
    /// CLOB v2 API key (UUID format).
    pub api_key: String,
    /// CLOB v2 API secret (base64-encoded bytes used as HMAC key).
    pub api_secret: String,
    /// CLOB v2 API passphrase.
    pub passphrase: String,
    /// Optional builder attribution code (`BUILDER_CODE` env var).
    pub builder_code: Option<String>,
}

impl std::fmt::Debug for ClobCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClobCredentials")
            .field("address", &self.address)
            .field("api_key", &self.api_key)
            .field("api_secret", &"***REDACTED***")
            .field("passphrase", &"***REDACTED***")
            .field("builder_code", &self.builder_code)
            .finish()
    }
}

impl ClobCredentials {
    /// Load API key credentials from environment variables.
    ///
    /// Required: `POLY_ADDRESS`, `POLY_API_KEY`, `POLY_API_SECRET`,
    /// `POLY_API_PASSPHRASE`.  Optional: `BUILDER_CODE`.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Config`] if any required variable is missing or empty.
    pub fn from_env() -> Result<Self, ClobError> {
        let get = |var: &str| -> Result<String, ClobError> {
            std::env::var(var)
                .map_err(|_| ClobError::Config(format!("{var} is not set")))
                .and_then(|v| {
                    if v.is_empty() {
                        Err(ClobError::Config(format!("{var} is empty")))
                    } else {
                        Ok(v)
                    }
                })
        };

        let address = get("POLY_ADDRESS")?;
        let api_key = get("POLY_API_KEY")?;
        let api_secret = get("POLY_API_SECRET")?;
        let passphrase = get("POLY_API_PASSPHRASE")?;
        let builder_code = std::env::var("BUILDER_CODE").ok().filter(|s| !s.is_empty());

        Ok(Self {
            address,
            api_key,
            api_secret,
            passphrase,
            builder_code,
        })
    }

    /// Compute the HMAC-SHA256 signature for a CLOB v2 API request.
    ///
    /// Message = `{timestamp}{method_upper}{request_path}{body}`, where `body`
    /// is the exact request body string for `POST`/`PUT` requests (omitted for
    /// `GET`/`DELETE`).  The body **must** be byte-identical to what is sent on
    /// the wire, otherwise the server-side HMAC check fails.
    ///
    /// This mirrors the official Polymarket reference implementations
    /// (`clob-client-v2/src/signing/hmac.ts`, `py-clob-client-v2/.../hmac.py`):
    /// the secret is **URL-safe** base64-decoded before use and the resulting
    /// signature is **URL-safe** base64-encoded.  Using the standard alphabet
    /// here would corrupt secrets containing `-`/`_` and emit signatures the
    /// CLOB rejects.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Auth`] if the `api_secret` cannot be base64-decoded.
    pub fn sign(
        &self,
        timestamp: &str,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String, ClobError> {
        use base64::Engine as _;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let secret_bytes = base64::engine::general_purpose::URL_SAFE
            .decode(self.api_secret.trim())
            .map_err(|e| ClobError::Auth(format!("base64-decode api_secret: {e}")))?;

        let mut message = format!("{}{}{}", timestamp, method.to_uppercase(), path);
        if let Some(body) = body {
            message.push_str(body);
        }

        let mut mac = Hmac::<Sha256>::new_from_slice(&secret_bytes)
            .map_err(|e| ClobError::Auth(format!("HMAC init: {e}")))?;
        mac.update(message.as_bytes());
        let result = mac.finalize().into_bytes();

        Ok(base64::engine::general_purpose::URL_SAFE.encode(result))
    }
}

/// Whether a market has settled, and which outcome it settled on.
///
/// Read from `GET /markets/{condition_id}`, which is the only source in the
/// stack that answers the question directly. Gamma cannot: `iter_active_markets`
/// asks it for `closed=false`, so a resolved market simply stops appearing and
/// its `closed` flag is `false` in every row we ever see. `end_date` cannot
/// either — Polymarket pads it, and an MLB market whose game starts in 71
/// minutes carries an `endDate` a full week out.
/// Market metadata from `GET /markets/{condition_id}`, for the copy path.
///
/// Exists because the copy engine rejects a leader's trade outright when it has
/// no row for the market: measured 2026-08-26 that was **409 signals against
/// 363 copies**, more trades lost to not knowing a market than were copied.
/// 124 of the 137 markets involved were in our table by the time we looked, so
/// it is a race against the scanner's sweep rather than a gap in coverage --
/// the metadata arrives, just after the trade did.
///
/// **`category` and `event_id` are deliberately absent.** The CLOB has no
/// market-level source for either; they come from the events sweep. That has a
/// consequence worth naming: `event_id` is the per-event exposure cap's key, so
/// a copy sized off this metadata is capped per *market* but not grouped under
/// its event until the sweep catches up. That is the trade -- a bounded gap in
/// one cap against a trade lost outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClobMarketMeta {
    /// `conditionId`, echoed back so a caller can tell a real answer from an
    /// empty body.
    pub condition_id: String,
    /// Scheduled resolution time. Padded by Polymarket, so an upper bound.
    pub end_date: Option<DateTime<Utc>>,
    /// Scheduled start of the underlying event, when it has one.
    pub game_start_time: Option<DateTime<Utc>>,
    /// Whether the market has already closed.
    pub closed: bool,
    /// The market's legs, as `(token_id, outcome)`.
    ///
    /// Carried for one reason: RTDS can deliver a trade whose `outcome` is
    /// blank (see `Engine::handle`), and this is the only place that maps the
    /// token the trade *does* name back onto the leg it belongs to. Empty when
    /// the endpoint named no tokens, which callers must read as "unknown".
    pub tokens: Vec<(String, String)>,
}

impl ClobMarketMeta {
    /// The outcome `token_id` belongs to, if this market names it.
    #[must_use]
    pub fn outcome_for(&self, token_id: &str) -> Option<&str> {
        self.tokens
            .iter()
            .find(|(id, outcome)| id == token_id && !outcome.is_empty())
            .map(|(_, outcome)| outcome.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketResolution {
    /// The market has settled. Its book is gone, so nothing may depend on one.
    pub closed: bool,
    /// Outcome name that paid out, when the market has settled.
    pub winner: Option<String>,
}

impl MarketResolution {
    /// A market still trading.
    #[must_use]
    pub fn open() -> Self {
        Self {
            closed: false,
            winner: None,
        }
    }

    /// What one share of `outcome` is worth now that the market has settled:
    /// `1` for the winning leg, `0` for the losing one, `None` while it trades.
    ///
    /// `None` also covers a settled market whose winner we could not read.
    /// Booking such a position at zero would invent a total loss out of a
    /// parsing gap, and booking it at one would invent a win — leaving it open
    /// for the next tick is the only honest option.
    #[must_use]
    pub fn payout_for(&self, outcome: &str) -> Option<rust_decimal::Decimal> {
        if !self.closed {
            return None;
        }
        let winner = self.winner.as_deref()?;
        Some(if winner.eq_ignore_ascii_case(outcome) {
            rust_decimal::Decimal::ONE
        } else {
            rust_decimal::Decimal::ZERO
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_as_u8() {
        assert_eq!(Side::Buy.as_u8(), 0);
        assert_eq!(Side::Sell.as_u8(), 1);
    }

    fn meta_with(tokens: Vec<(String, String)>) -> ClobMarketMeta {
        ClobMarketMeta {
            condition_id: "0xabc".into(),
            end_date: None,
            game_start_time: None,
            closed: false,
            tokens,
        }
    }

    #[test]
    fn outcome_for_names_the_leg_a_token_belongs_to() {
        let m = meta_with(vec![
            ("111".into(), "Yes".into()),
            ("222".into(), "No".into()),
        ]);
        assert_eq!(m.outcome_for("111"), Some("Yes"));
        assert_eq!(m.outcome_for("222"), Some("No"));
    }

    /// The caller repairs a blank outcome with this, so an answer that is
    /// itself blank is no answer -- it would swap one empty string for another
    /// and log a recovery that did not happen.
    #[test]
    fn outcome_for_treats_a_blank_leg_as_unknown() {
        let m = meta_with(vec![("111".into(), String::new())]);
        assert_eq!(m.outcome_for("111"), None);
    }

    #[test]
    fn outcome_for_misses_a_token_the_market_does_not_name() {
        let m = meta_with(vec![("111".into(), "Yes".into())]);
        assert_eq!(m.outcome_for("333"), None);
        assert_eq!(meta_with(Vec::new()).outcome_for("111"), None);
    }

    #[test]
    fn a_settled_market_pays_a_dollar_to_the_winner_and_nothing_to_the_loser() {
        let res = MarketResolution {
            closed: true,
            winner: Some("Detroit Tigers".to_owned()),
        };
        assert_eq!(
            res.payout_for("Detroit Tigers"),
            Some(rust_decimal::Decimal::ONE)
        );
        assert_eq!(
            res.payout_for("Cleveland Guardians"),
            Some(rust_decimal::Decimal::ZERO)
        );
    }

    /// Outcome names reach us from the trade feed and from the exchange, and
    /// nothing guarantees the two agree on case.
    #[test]
    fn the_winner_is_matched_case_insensitively() {
        let res = MarketResolution {
            closed: true,
            winner: Some("Yes".to_owned()),
        };
        assert_eq!(res.payout_for("YES"), Some(rust_decimal::Decimal::ONE));
        assert_eq!(res.payout_for("no"), Some(rust_decimal::Decimal::ZERO));
    }

    /// A market still trading has no payout — not a payout of zero. The caller
    /// closes a position on `Some`, so the difference is the whole position.
    #[test]
    fn an_open_market_has_no_payout() {
        assert_eq!(MarketResolution::open().payout_for("Yes"), None);
    }

    /// Settled but with no winner on the wire: the honest answer is "I do not
    /// know", because the two candidates are the whole dollar and none of it.
    #[test]
    fn a_settled_market_without_a_winner_refuses_to_guess() {
        let res = MarketResolution {
            closed: true,
            winner: None,
        };
        assert_eq!(res.payout_for("Yes"), None);
    }

    #[test]
    fn side_serde_roundtrip() {
        let j = serde_json::to_string(&Side::Buy).unwrap();
        assert_eq!(j, r#""BUY""#);
        let back: Side = serde_json::from_str(&j).unwrap();
        assert_eq!(back, Side::Buy);
    }

    #[test]
    fn credentials_debug_redacts_secret() {
        let creds = ClobCredentials {
            address: "0xABC".into(),
            api_key: "key".into(),
            api_secret: "supersecret".into(),
            passphrase: "passphrase".into(),
            builder_code: None,
        };
        let s = format!("{creds:?}");
        assert!(!s.contains("supersecret"));
        assert!(s.contains("REDACTED"));
    }

    fn sign_test_creds() -> ClobCredentials {
        use base64::Engine as _;
        // Polymarket secrets are URL-safe base64 — encode the test secret the
        // same way the real API hands it out.
        let secret_b64 =
            base64::engine::general_purpose::URL_SAFE.encode(b"garnet_test_secret_bytes_for_hmac");
        ClobCredentials {
            address: "0x0".into(),
            api_key: "k".into(),
            api_secret: secret_b64,
            passphrase: "p".into(),
            builder_code: None,
        }
    }

    #[test]
    fn credentials_sign_produces_url_safe_base64() {
        use base64::Engine as _;
        let creds = sign_test_creds();
        let sig = creds.sign("1700000000", "GET", "/orders", None).unwrap();
        assert!(!sig.is_empty());
        // Signature must decode as URL-safe base64 (the alphabet the CLOB uses).
        base64::engine::general_purpose::URL_SAFE
            .decode(&sig)
            .expect("signature must be URL-safe base64");
        // Deterministic for identical inputs.
        let sig2 = creds.sign("1700000000", "GET", "/orders", None).unwrap();
        assert_eq!(sig, sig2);
    }

    #[test]
    fn credentials_sign_includes_body_in_hmac() {
        let creds = sign_test_creds();
        let without = creds.sign("1700000000", "POST", "/order", None).unwrap();
        let with_body = creds
            .sign("1700000000", "POST", "/order", Some(r#"{"a":1}"#))
            .unwrap();
        // The body is part of the signed message — POST signatures with and
        // without a body must differ, or the server-side L2 check would fail.
        assert_ne!(
            without, with_body,
            "request body must contribute to the HMAC"
        );
    }

    #[test]
    fn credentials_sign_known_vector() {
        // Regression pin: secret = URL-safe base64("0123456789abcdef0123456789abcdef"),
        // message = "1700000000" + "GET" + "/orders" (no body).
        // HMAC-SHA256 reference value (url-safe base64, computed offline).
        use base64::Engine as _;
        let secret =
            base64::engine::general_purpose::URL_SAFE.encode(b"0123456789abcdef0123456789abcdef");
        let creds = ClobCredentials {
            address: "0x0".into(),
            api_key: "k".into(),
            api_secret: secret,
            passphrase: "p".into(),
            builder_code: None,
        };
        let sig = creds.sign("1700000000", "GET", "/orders", None).unwrap();
        assert_eq!(sig, "d_8rYNiskB9DSmPVGBEqPIK9veia9oWolAp_uuLjkXA=");
    }

    #[test]
    fn credentials_builder_code_some_and_none() {
        // Serialise with a static mutex; env vars are process-global state.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        std::env::set_var("POLY_ADDRESS", "0xABC");
        std::env::set_var("POLY_API_KEY", "key123");
        std::env::set_var("POLY_API_SECRET", "c2VjcmV0");
        std::env::set_var("POLY_API_PASSPHRASE", "pass");
        std::env::set_var("BUILDER_CODE", "GARNET_TEST");
        let creds = ClobCredentials::from_env().unwrap();
        assert_eq!(creds.builder_code, Some("GARNET_TEST".into()));

        std::env::remove_var("BUILDER_CODE");
        let creds2 = ClobCredentials::from_env().unwrap();
        assert_eq!(creds2.builder_code, None);

        for var in &[
            "POLY_ADDRESS",
            "POLY_API_KEY",
            "POLY_API_SECRET",
            "POLY_API_PASSPHRASE",
        ] {
            std::env::remove_var(var);
        }
    }
}
