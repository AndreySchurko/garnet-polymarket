//! `Retry-After` parsing.
//!
//! Cloudflare enforces Polymarket's API rate limits and tells us how long to
//! wait. Ignoring that header in favour of our own exponential guess is exactly
//! the "impatient bot" behaviour the spec targets (§A2) — and guessing short
//! deepens the throttle. The header is either delta-seconds or an HTTP-date.

use chrono::{DateTime, Utc};
use reqwest::header::HeaderMap;

/// Seconds to wait per the `Retry-After` header, if present and parseable.
///
/// An HTTP-date already in the past yields `0.0`, never a negative sleep.
#[must_use]
pub fn retry_after_secs(headers: &HeaderMap) -> Option<f64> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let raw = raw.trim();
    if let Ok(secs) = raw.parse::<f64>() {
        return Some(secs.max(0.0));
    }
    let when = DateTime::parse_from_rfc2822(raw).ok()?.with_timezone(&Utc);
    let millis = (when - Utc::now()).num_milliseconds().max(0);
    #[allow(clippy::cast_precision_loss)] // milliseconds of wait; f64 is exact here
    Some(millis as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::retry_after_secs;
    use chrono::Utc;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    fn with(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn parses_delta_seconds() {
        assert_eq!(retry_after_secs(&with("7")), Some(7.0));
        assert_eq!(retry_after_secs(&with(" 12 ")), Some(12.0));
    }

    #[test]
    fn parses_http_date_in_the_future() {
        let when = (Utc::now() + chrono::Duration::seconds(30)).to_rfc2822();
        let got = retry_after_secs(&with(&when)).expect("parsed");
        assert!((25.0..=31.0).contains(&got), "got {got}");
    }

    #[test]
    fn past_date_is_zero_not_negative() {
        let when = (Utc::now() - chrono::Duration::seconds(60)).to_rfc2822();
        assert_eq!(retry_after_secs(&with(&when)), Some(0.0));
    }

    #[test]
    fn absent_or_garbage_is_none() {
        assert_eq!(retry_after_secs(&HeaderMap::new()), None);
        assert_eq!(retry_after_secs(&with("soon")), None);
    }
}
