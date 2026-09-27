//! Verification of the Telegram mini-app's `initData`.
//!
//! The dashboard's only door to the outside: the domain is public, Caddy terminates
//! TLS, and this signature decides who has arrived. A mistake here means the wallet
//! registry and the trading stop are available to anyone who opened the link.
//!
//! The algorithm is dictated by Telegram and reproduced here literally:
//!
//! 1. `hash` is dropped from the pairs, the rest are sorted by key and joined as
//!    `k=v` with newlines — **all** of them, including fields unknown to us:
//!    otherwise anything at all could be appended to the signed string;
//! 2. the signing key is `HMAC_SHA256("WebAppData", bot_token)`, so another bot
//!    cannot issue a pass into our dashboard;
//! 3. `auth_date` must be fresh: the string does not expire by itself, and an
//!    intercepted one would work forever.

use hmac::{Hmac, Mac as _};
use sha2::Sha256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// The signature does not match — either forged, or from another bot.
    BadHash,
    /// `auth_date` is older than permitted.
    Stale,
    /// The `user` pair is absent or does not parse.
    NoUser,
    /// The string does not look like `initData` at all.
    Malformed,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AuthError::BadHash => "the signature does not match",
            AuthError::Stale => "the data is stale",
            AuthError::NoUser => "the data contains no user",
            AuthError::Malformed => "the string did not parse",
        };
        f.write_str(s)
    }
}

impl std::error::Error for AuthError {}

/// Verify the signature and return the user's Telegram id.
///
/// # Errors
///
/// Any mismatch: a forgery, another bot, an expiry or junk.
pub fn check(init_data: &str, bot_token: &str, max_age_secs: i64) -> Result<i64, AuthError> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut given_hash: Option<String> = None;

    for chunk in init_data.split('&').filter(|c| !c.is_empty()) {
        let (k, v) = chunk.split_once('=').ok_or(AuthError::Malformed)?;
        let v = urldecode(v).ok_or(AuthError::Malformed)?;
        if k == "hash" {
            given_hash = Some(v);
        } else {
            pairs.push((k.to_string(), v));
        }
    }

    let given = given_hash
        .filter(|h| !h.is_empty())
        .ok_or(AuthError::Malformed)?;
    if pairs.is_empty() {
        return Err(AuthError::Malformed);
    }

    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let check_string = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut secret =
        Hmac::<Sha256>::new_from_slice(b"WebAppData").map_err(|_| AuthError::Malformed)?;
    secret.update(bot_token.as_bytes());
    let secret = secret.finalize().into_bytes();

    let mut mac = Hmac::<Sha256>::new_from_slice(&secret).map_err(|_| AuthError::Malformed)?;
    mac.update(check_string.as_bytes());
    // `verify_slice` compares in constant time: a character-by-character comparison of
    // the signature leaks its contents through the response time.
    let expected = hex::decode(&given).map_err(|_| AuthError::BadHash)?;
    mac.verify_slice(&expected)
        .map_err(|_| AuthError::BadHash)?;

    let auth_date: i64 = pairs
        .iter()
        .find(|(k, _)| k == "auth_date")
        .and_then(|(_, v)| v.parse().ok())
        .ok_or(AuthError::Malformed)?;
    if chrono::Utc::now().timestamp() - auth_date > max_age_secs {
        return Err(AuthError::Stale);
    }

    let user = pairs
        .iter()
        .find(|(k, _)| k == "user")
        .ok_or(AuthError::NoUser)?;
    let parsed: serde_json::Value = serde_json::from_str(&user.1).map_err(|_| AuthError::NoUser)?;
    parsed["id"].as_i64().ok_or(AuthError::NoUser)
}

/// Percent-decoding. Our own, so as not to pull in a dependency for a single function:
/// `initData` arrives as an ordinary query string.
fn urldecode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}
