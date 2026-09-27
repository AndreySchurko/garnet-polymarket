//! Verification of the mini-app's `initData`.
//!
//! This is the dashboard's only door to the outside: the domain is public, Caddy
//! terminates TLS, and this signature is what decides who has arrived. A mistake here
//! means the wallet registry and the trading stop are available to anyone with the link.

use garnet_dash::initdata::{check, AuthError};

const TOKEN: &str = "12345:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw";

/// Build `initData` with a real signature — exactly as Telegram does.
fn signed(pairs: &[(&str, &str)], token: &str) -> String {
    use hmac::{Hmac, Mac as _};
    use sha2::Sha256;

    let mut sorted: Vec<&(&str, &str)> = pairs.iter().collect();
    sorted.sort_by_key(|(k, _)| *k);
    let check_string = sorted
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut secret = Hmac::<Sha256>::new_from_slice(b"WebAppData").unwrap();
    secret.update(token.as_bytes());
    let secret = secret.finalize().into_bytes();

    let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
    mac.update(check_string.as_bytes());
    let hash = hex::encode(mac.finalize().into_bytes());

    let mut query: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencode(v)))
        .collect();
    query.push(format!("hash={hash}"));
    query.join("&")
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[test]
fn a_genuine_init_data_names_its_user() {
    let data = signed(
        &[
            ("auth_date", &now().to_string()),
            ("user", r#"{"id":6218292348,"first_name":"Operator"}"#),
        ],
        TOKEN,
    );
    assert_eq!(check(&data, TOKEN, 86400).unwrap(), 6_218_292_348);
}

#[test]
fn a_forged_hash_is_refused() {
    let mut data = signed(
        &[("auth_date", &now().to_string()), ("user", r#"{"id":1}"#)],
        TOKEN,
    );
    // We alter the last character of the signature — necessarily to a different one:
    // the signature is random, and "replace it with a zero" sometimes changed nothing.
    let last = data.pop().expect("the signature is not empty");
    data.push(if last == '0' { '1' } else { '0' });
    assert!(matches!(
        check(&data, TOKEN, 86400),
        Err(AuthError::BadHash)
    ));
}

#[test]
fn data_signed_by_another_bot_is_refused() {
    // The signature is derived from the token: another bot cannot issue a pass into our
    // dashboard, however perfect the format.
    let data = signed(
        &[("auth_date", &now().to_string()), ("user", r#"{"id":1}"#)],
        "999:OTHER",
    );
    assert!(matches!(
        check(&data, TOKEN, 86400),
        Err(AuthError::BadHash)
    ));
}

#[test]
fn stale_data_is_refused() {
    // `initData` does not expire by itself: otherwise an intercepted string works
    // forever.
    let old = now() - 90_000;
    let data = signed(
        &[("auth_date", &old.to_string()), ("user", r#"{"id":1}"#)],
        TOKEN,
    );
    assert!(matches!(check(&data, TOKEN, 86400), Err(AuthError::Stale)));
}

#[test]
fn a_field_added_after_signing_breaks_the_hash() {
    // **All** pairs except `hash` are verified: otherwise anything at all could be
    // appended to the signed string.
    let data = signed(
        &[("auth_date", &now().to_string()), ("user", r#"{"id":1}"#)],
        TOKEN,
    );
    let tampered = format!("{data}&chat_instance=-1");
    assert!(matches!(
        check(&tampered, TOKEN, 86400),
        Err(AuthError::BadHash)
    ));
}

#[test]
fn data_without_a_user_is_refused() {
    let data = signed(&[("auth_date", &now().to_string())], TOKEN);
    assert!(matches!(check(&data, TOKEN, 86400), Err(AuthError::NoUser)));
}

#[test]
fn garbage_is_refused_rather_than_panicking() {
    // The string is written by whoever opened the link: panicking on it is not allowed.
    for junk in ["", "hash=", "&&&", "hash=zz", "user=%%%&hash=00"] {
        assert!(check(junk, TOKEN, 86400).is_err(), "{junk}");
    }
}
