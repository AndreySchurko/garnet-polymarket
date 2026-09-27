//! Classify a CLOB REST response as a Cloudflare Bot-Management block.
//!
//! 403/503 + a `cf-ray` header + a non-JSON body means Cloudflare blocked us,
//! not the API rejecting a request. Surfaced as [`crate::error::ClobError::EgressBlocked`]
//! so the risk layer fires `KS_EGRESS_BLOCKED` instead of `KS_CLOB_LOST`.

use reqwest::header::HeaderMap;
use reqwest::StatusCode;

/// True when the response looks like a Cloudflare block rather than an API error.
#[must_use]
pub fn is_cloudflare_block(status: StatusCode, headers: &HeaderMap, body: &str) -> bool {
    if status != StatusCode::FORBIDDEN && status != StatusCode::SERVICE_UNAVAILABLE {
        return false;
    }
    let has_cf_ray = headers
        .keys()
        .any(|k| k.as_str().eq_ignore_ascii_case("cf-ray"));
    let looks_json = {
        let t = body.trim_start();
        t.starts_with('{') || t.starts_with('[')
    };
    has_cf_ray && !looks_json
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn hdrs(cf: bool) -> HeaderMap {
        let mut h = HeaderMap::new();
        if cf {
            h.insert("cf-ray", HeaderValue::from_static("abc-DUB"));
        }
        h
    }

    #[test]
    fn detects_html_403_with_cf_ray() {
        assert!(is_cloudflare_block(
            StatusCode::FORBIDDEN,
            &hdrs(true),
            "<html>x</html>"
        ));
    }

    #[test]
    fn json_403_is_not_a_block() {
        assert!(!is_cloudflare_block(
            StatusCode::FORBIDDEN,
            &hdrs(true),
            "{\"error\":\"nope\"}"
        ));
    }

    #[test]
    fn no_cf_ray_is_not_a_block() {
        assert!(!is_cloudflare_block(
            StatusCode::FORBIDDEN,
            &hdrs(false),
            "<html>x</html>"
        ));
    }

    #[test]
    fn service_unavailable_with_cf_ray_is_a_block() {
        assert!(is_cloudflare_block(
            StatusCode::SERVICE_UNAVAILABLE,
            &hdrs(true),
            "error"
        ));
    }
}
