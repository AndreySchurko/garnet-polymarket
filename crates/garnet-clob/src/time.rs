//! Parsing CLOB timestamps.
//!
//! Carried over from the predecessor's `orderbook_ws`: the module itself is not
//! carried over, but the REST client needs this function.

use crate::error::ClobError;
use chrono::Utc;

/// Polymarket sends unix seconds; a value above ~10^12 is treated as milliseconds,
/// because in seconds it would be out of range.
pub(crate) fn parse_ws_timestamp(raw: &str) -> Result<chrono::DateTime<Utc>, ClobError> {
    if let Ok(secs) = raw.parse::<i64>() {
        let (s, ns) = if secs > 10_000_000_000 {
            (
                secs / 1_000,
                u32::try_from((secs % 1_000) * 1_000_000).unwrap_or(0),
            )
        } else {
            (secs, 0)
        };
        if let Some(dt) = chrono::DateTime::from_timestamp(s, ns) {
            return Ok(dt);
        }
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| ClobError::Parse(format!("timestamp `{raw}`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_seconds_parse() {
        assert_eq!(
            parse_ws_timestamp("1700000000").unwrap().timestamp(),
            1_700_000_000
        );
    }

    #[test]
    fn milliseconds_parse() {
        assert_eq!(
            parse_ws_timestamp("1700000000000").unwrap().timestamp(),
            1_700_000_000
        );
    }

    #[test]
    fn rfc3339_parses_too() {
        assert!(parse_ws_timestamp("2026-09-03T10:00:00Z").is_ok());
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse_ws_timestamp("not a time").is_err());
    }
}
