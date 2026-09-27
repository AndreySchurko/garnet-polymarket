//! Readiness checks before a live order.
//!
//! The point of the module is not "start or not" but to **name the broken thing**. In the
//! predecessor a failure of the live path looked like "startup failed", and on the host that
//! cost half an hour in each case. So every check prints its own line, and the exit code is
//! assembled from them.

use garnet_bin::preflight::{
    allowances, collateral, gas, live_wallet_stakes, probe, report, trading_identity,
    wallet_signature_match, Check, Flow,
};
use garnet_blockchain::types::AllowanceStatus;
use garnet_clob::SignatureType;
use rust_decimal_macros::dec;

fn all_allowances() -> AllowanceStatus {
    AllowanceStatus {
        pusd_to_ctf_exchange: true,
        pusd_to_neg_risk_exchange: true,
        ctf_to_ctf_exchange: true,
        ctf_to_neg_risk_exchange: true,
        ctf_to_neg_risk_adapter: true,
        ctf_to_collateral_adapter: true,
    }
}

#[test]
fn a_clean_run_exits_zero() {
    let checks = vec![
        Check::pass("postgres", "migrations applied"),
        Check::pass("clob", "200"),
    ];
    assert_eq!(report(&checks), 0);
}

#[test]
fn a_single_failure_fails_the_whole_run() {
    let checks = vec![
        Check::pass("postgres", "ok"),
        Check::fail("clob", "403 geoblock"),
    ];
    assert_eq!(report(&checks), 1);
}

#[test]
fn a_failed_check_names_the_thing_that_is_broken() {
    // "startup failed" does not say what to fix.
    let line = Check::fail("allowance", "pusd_to_ctf_exchange is not granted").render();
    assert!(line.contains("FAIL"), "got: {line}");
    assert!(line.contains("allowance"), "got: {line}");
    assert!(line.contains("pusd_to_ctf_exchange"), "got: {line}");
}

#[test]
fn enough_collateral_passes() {
    assert!(collateral(dec!(25), dec!(0), dec!(5)).ok);
}

#[test]
fn unwrapped_usdc_is_a_failure_that_names_the_wrap() {
    // The trading collateral is pUSD. USDC.e in the balance looks like money, but no order
    // will stand on it until it has been wrapped.
    let check = collateral(dec!(0), dec!(50), dec!(5));
    assert!(!check.ok);
    assert!(
        check.detail.contains("wrap"),
        "the check must name the action: {}",
        check.detail
    );
}

#[test]
fn an_empty_wallet_is_a_failure_that_names_the_shortfall() {
    let check = collateral(dec!(1), dec!(0), dec!(5));
    assert!(!check.ok);
    assert!(check.detail.contains('5'), "got: {}", check.detail);
}

#[test]
fn every_missing_allowance_is_named() {
    let mut status = all_allowances();
    status.ctf_to_neg_risk_adapter = false;
    status.ctf_to_collateral_adapter = false;

    let check = allowances(&status, Flow::SelfCustody);
    assert!(!check.ok);
    assert!(
        check.detail.contains("ctf_to_neg_risk_adapter"),
        "{}",
        check.detail
    );
    assert!(
        check.detail.contains("ctf_to_collateral_adapter"),
        "{}",
        check.detail
    );
}

#[test]
fn all_allowances_set_passes() {
    assert!(allowances(&all_allowances(), Flow::SelfCustody).ok);
}

#[test]
fn allowances_of_our_eoa_say_nothing_about_a_proxy_account() {
    // The allowances are read from the signer while the outcome tokens sit on the proxy.
    // In the proxy flow these are different addresses, and a failure on our EOA would mean a
    // problem where there is none.
    let mut status = all_allowances();
    status.ctf_to_collateral_adapter = false;
    let check = allowances(&status, Flow::Proxy);
    assert!(check.ok, "{}", check.detail);
    assert!(check.detail.contains("proxy"), "{}", check.detail);
}

#[test]
fn no_gas_is_a_failure_when_we_send_our_own_transactions() {
    // A redemption is a transaction: without MATIC a winning position does not turn into
    // money, while the orders go through as if nothing were wrong.
    assert!(!gas(dec!(0), dec!(0.5), Flow::SelfCustody).ok);
    assert!(gas(dec!(2), dec!(0.5), Flow::SelfCustody).ok);
}

#[test]
fn gas_is_not_ours_when_polymarket_relays() {
    // On a proxy account Polymarket's relay sends the transactions, and zero MATIC there is
    // normal rather than broken. A check demanding gas in that mode would send the operator
    // hunting for a problem that does not exist.
    let check = gas(dec!(0), dec!(0.5), Flow::Proxy);
    assert!(check.ok, "{}", check.detail);
    assert!(check.detail.contains("relay"), "{}", check.detail);
}

#[test]
fn a_live_wallet_above_the_smoke_cap_is_a_failure() {
    // A smoke test costs $1 only as long as the row in the database says $1.
    let check = live_wallet_stakes(&[("0xabc".into(), dec!(25))], dec!(1));
    assert!(!check.ok);
    assert!(check.detail.contains("0xabc"), "{}", check.detail);
    assert!(check.detail.contains("25"), "{}", check.detail);
}

#[test]
fn live_wallets_within_the_cap_pass() {
    let check = live_wallet_stakes(&[("0xabc".into(), dec!(1))], dec!(1));
    assert!(check.ok, "{}", check.detail);
}

#[test]
fn no_live_wallet_at_all_is_a_failure_before_a_smoke() {
    // Preflight --live is run in order to fire. An empty list means there is nothing to fire
    // with, and it is better to learn that here.
    assert!(!live_wallet_stakes(&[], dec!(1)).ok);
}

#[test]
fn the_l2_key_must_belong_to_the_signing_key() {
    // POLY_ADDRESS is the owner of the L2 key, and the signer has to be that owner: a key
    // issued by a different address authorises REST but signs orders on behalf of an account
    // the exchange will not credit them to.
    let check = trading_identity("0xAAA", "0xbbb", None);
    assert!(!check.ok);
    assert!(check.detail.contains("0xAAA"), "{}", check.detail);
    assert!(check.detail.contains("0xbbb"), "{}", check.detail);
}

#[test]
fn the_l2_key_check_ignores_case() {
    assert!(trading_identity("0xAbC", "0xabc", None).ok);
}

#[test]
fn a_funder_different_from_the_signer_is_the_point_of_a_proxy() {
    // The proxy holds the money, the EOA signs. Demanding equality — as the first version of
    // this check did — would declare Polymarket's standard arrangement broken.
    // Polymarket.
    let check = trading_identity("0xabc", "0xabc", Some("0xdef"));
    assert!(check.ok, "{}", check.detail);
    assert!(check.detail.contains("0xdef"), "{}", check.detail);
}

#[test]
fn a_proxy_flow_whose_funder_is_the_signer_has_no_proxy() {
    // The signature type says "through a proxy" while the funder is the signer itself: the
    // order would be built for a wallet that is not a proxy.
    let check = trading_identity("0xabc", "0xabc", Some("0xABC"));
    assert!(!check.ok, "{}", check.detail);
}

#[test]
fn a_probe_error_becomes_a_failed_check_not_a_crash() {
    // The first unavailable dependency has no right to abort the run: the point of preflight
    // is to see every broken thing in one pass rather than fixing them one at a time,
    // restarting after each.
    let result: Result<&str, String> = Err("connection refused".into());
    let check = probe("postgres", result);
    assert!(!check.ok);
    assert!(
        check.detail.contains("connection refused"),
        "{}",
        check.detail
    );
}

#[test]
fn a_successful_probe_reports_what_it_saw() {
    let check = probe("postgres", Ok::<_, String>("1 migration"));
    assert!(check.ok);
    assert!(check.detail.contains("1 migration"), "{}", check.detail);
}

// ---------------------------------------------------------------------------
// The signature type against what the wallet actually is
// ---------------------------------------------------------------------------

#[test]
fn a_safe_wallet_demands_the_safe_signature_type() {
    // A minimal proxy that answers `masterCopy()` is a Gnosis Safe. An order signed as
    // POLY_PROXY will be rejected by the exchange, and it will look like a problem with the
    // keys.
    let check = wallet_signature_match(SignatureType::PolyProxy, Some(true));
    assert!(!check.ok);
    assert!(
        check.detail.contains("POLY_GNOSIS_SAFE"),
        "{}",
        check.detail
    );
}

#[test]
fn a_poly_proxy_wallet_rejects_the_safe_signature_type() {
    let check = wallet_signature_match(SignatureType::PolyGnosisSafe, Some(false));
    assert!(!check.ok);
    assert!(check.detail.contains("POLY_PROXY"), "{}", check.detail);
}

#[test]
fn a_matching_pair_passes() {
    assert!(wallet_signature_match(SignatureType::PolyGnosisSafe, Some(true)).ok);
    assert!(wallet_signature_match(SignatureType::PolyProxy, Some(false)).ok);
}

#[test]
fn the_eoa_flow_has_no_wallet_contract_to_check() {
    assert!(wallet_signature_match(SignatureType::Eoa, None).ok);
}
