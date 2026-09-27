//! Resolving the live path from the environment.
//!
//! Resolution is separated from connecting on purpose: connecting goes to the network and
//! cannot be verified in a test, whereas everything the live path gets wrong in practice is
//! the parsing of the environment. In the predecessor the signature type was hardcoded as
//! `PolyProxy`, and an EOA operator signed every order through a proxy that does not exist;
//! the exchange rejected everything while the logs showed "order sent".
//!
//! The tests do not touch `std::env`: process variables are global while Rust's tests run in
//! parallel in one process, so mutating the environment would make the failures flaky.

use garnet_bin::clob_live::{resolve, LiveEnv};
use garnet_clob::SignatureType;

const ADDRESS: &str = "0x1111111111111111111111111111111111111111";
const PROXY: &str = "0x2222222222222222222222222222222222222222";
const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

/// The full set: four L2 variables, the private key and the proxy.
fn full() -> Vec<(&'static str, &'static str)> {
    vec![
        ("POLY_ADDRESS", ADDRESS),
        ("POLY_API_KEY", "6b9d1f2e-0c3a-4f5b-8d7e-1a2b3c4d5e6f"),
        ("POLY_API_SECRET", "c2VjcmV0LWtleQ=="),
        ("POLY_API_PASSPHRASE", "passphrase"),
        ("PRIVATE_KEY", KEY),
        ("POLY_PROXY_ADDRESS", PROXY),
    ]
}

fn without(var: &str) -> LiveEnv {
    LiveEnv::from_pairs(full().into_iter().filter(|(k, _)| *k != var))
}

fn with(var: &str, value: &str) -> LiveEnv {
    let mut pairs: Vec<(&str, &str)> = full().into_iter().filter(|(k, _)| *k != var).collect();
    pairs.push((var, value));
    LiveEnv::from_pairs(pairs)
}

#[test]
fn empty_environment_disables_live_rather_than_failing() {
    // A shadow-only deployment has no keys and must start: a refusal here would mean the
    // measuring instrument cannot be run without a production wallet.
    let cfg = resolve(&LiveEnv::from_pairs(Vec::<(&str, &str)>::new())).unwrap();
    assert!(
        cfg.is_none(),
        "without the variables the live path is simply disabled"
    );
}

#[test]
fn a_half_configured_live_path_is_loud() {
    // Three variables out of four is not "there are no keys", it is a typo in the deployment.
    // Quietly slipping into shadow would show the operator a green log where they expect live
    // orders.
    let err = resolve(&without("POLY_API_SECRET"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("POLY_API_SECRET"),
        "the error must name the missing variable, got: {err}"
    );
}

#[test]
fn credentials_without_a_private_key_are_rejected() {
    // The L2 keys sign the REST requests, but the order is signed with the private key.
    // Without it the client would build and fail on every order in production.
    let err = resolve(&without("PRIVATE_KEY")).unwrap_err().to_string();
    assert!(err.contains("PRIVATE_KEY"), "got: {err}");
}

#[test]
fn the_signature_type_defaults_to_the_proxy_flow() {
    let cfg = resolve(&without("POLY_SIGNATURE_TYPE")).unwrap().unwrap();
    assert_eq!(cfg.signature_type, SignatureType::PolyProxy);
    assert_eq!(
        cfg.funder.map(|a| format!("{a:#x}")),
        Some(PROXY.to_lowercase()),
        "in proxy mode the proxy holds the collateral and is the funder"
    );
}

#[test]
fn eoa_has_no_funder_even_when_a_proxy_address_is_set() {
    // The EOA flow has no proxy: the signer is the holder. A funder passed in would tell the
    // SDK to build an order for a wallet that does not exist.
    let cfg = resolve(&with("POLY_SIGNATURE_TYPE", "EOA"))
        .unwrap()
        .unwrap();
    assert_eq!(cfg.signature_type, SignatureType::Eoa);
    assert!(cfg.funder.is_none());
}

#[test]
fn the_signature_type_is_case_insensitive() {
    let cfg = resolve(&with("POLY_SIGNATURE_TYPE", "poly_gnosis_safe"))
        .unwrap()
        .unwrap();
    assert_eq!(cfg.signature_type, SignatureType::PolyGnosisSafe);
}

#[test]
fn a_typo_in_the_signature_type_is_an_error_not_a_default() {
    // A silent fallback to a default would sign every order the wrong way.
    let err = resolve(&with("POLY_SIGNATURE_TYPE", "PROXY"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("EOA"),
        "the error must list the permitted values: {err}"
    );
}

#[test]
fn a_proxy_flow_without_a_proxy_address_is_an_error() {
    let err = resolve(&without("POLY_PROXY_ADDRESS"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("POLY_PROXY_ADDRESS"), "got: {err}");
}

#[test]
fn a_malformed_proxy_address_is_an_error() {
    let err = resolve(&with("POLY_PROXY_ADDRESS", "0xnope"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("POLY_PROXY_ADDRESS"), "got: {err}");
}

#[test]
fn the_chain_defaults_to_polygon_mainnet() {
    let cfg = resolve(&LiveEnv::from_pairs(full())).unwrap().unwrap();
    assert_eq!(cfg.chain_id, 137);
}

#[test]
fn a_malformed_chain_id_is_an_error() {
    // The wrong network means an EIP-712 signature with a foreign domain: the exchange will
    // reject everything, and the cause will look like a problem with the keys.
    let err = resolve(&with("POLYGON_CHAIN_ID", "polygon"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("POLYGON_CHAIN_ID"), "got: {err}");
}

#[test]
fn an_empty_variable_counts_as_unset() {
    // systemd's EnvironmentFile yields an empty string where a value was left unfilled. An
    // empty key would pass validation and fail on signing.
    let err = resolve(&with("POLY_API_KEY", "")).unwrap_err().to_string();
    assert!(err.contains("POLY_API_KEY"), "got: {err}");
}

#[test]
fn the_builder_code_is_optional_and_carried_through() {
    let cfg = resolve(&with("BUILDER_CODE", "garnet")).unwrap().unwrap();
    assert_eq!(cfg.credentials.builder_code.as_deref(), Some("garnet"));
}

#[test]
fn the_private_key_never_reaches_a_log_line() {
    // `LiveConfig` goes into the log in full at startup: a private key in `Debug` is a private
    // key in journald.
    let cfg = resolve(&LiveEnv::from_pairs(full())).unwrap().unwrap();
    let rendered = format!("{cfg:?}");
    assert!(
        !rendered.contains(KEY),
        "the private key is visible in Debug: {rendered}"
    );
}
