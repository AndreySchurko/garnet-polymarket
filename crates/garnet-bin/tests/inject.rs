//! Injecting a hand-made leader trade into the pipeline.
//!
//! A validation tool: a live $1 order is submitted by the same path as a real signal —
//! through `App::on_frame`. Otherwise a smoke test would check the signature and FAK but not
//! detection, dedup, slippage, accounting and the fee — that is, everything by which
//! execution differs from a single HTTP request.

use garnet_bin::inject::{confirm, fresh_tx_hash, synthesize};
use garnet_core::detect::{parse_frame, trades_only};
use garnet_db::Side;
use rust_decimal_macros::dec;

#[test]
fn a_synthesized_frame_is_read_back_by_the_real_parser() {
    // A frame hand-built to the wrong RTDS shape would produce "nothing happened" and would
    // look like an execution failure.
    let frame = synthesize(
        "0xLeader",
        "tok_up",
        Side::Buy,
        dec!(0.42),
        dec!(120),
        "0xdead",
    );
    let trades = trades_only(parse_frame(&frame));

    assert_eq!(trades.len(), 1);
    let t = &trades[0];
    assert_eq!(t.wallet, "0xleader", "the wallet is compared in lower case");
    assert_eq!(t.token_id, "tok_up");
    assert_eq!(t.side, Side::Buy);
    assert_eq!(t.price, dec!(0.42));
    assert_eq!(t.size, dec!(120));
    assert_eq!(t.tx_hash, "0xdead");
}

#[test]
fn a_sale_is_synthesized_as_a_sale() {
    let frame = synthesize("0xL", "tok", Side::Sell, dec!(0.6), dec!(10), "0xbeef");
    assert_eq!(trades_only(parse_frame(&frame))[0].side, Side::Sell);
}

#[test]
fn two_injections_never_share_a_transaction_hash() {
    // The dedup is keyed by `(tx_hash, wallet, token_id, side)`. A repeat smoke test with the
    // same hash would be rejected as a duplicate, and that would look like an execution
    // failure.
    assert_ne!(fresh_tx_hash(), fresh_tx_hash());
}

#[test]
fn a_live_injection_without_an_explicit_yes_is_refused() {
    let err = confirm(true, false).unwrap_err().to_string();
    assert!(
        err.contains("--yes"),
        "the refusal must name the flag: {err}"
    );
}

#[test]
fn a_live_injection_with_yes_goes_through() {
    assert!(confirm(true, true).is_ok());
}

#[test]
fn a_shadow_injection_needs_no_confirmation() {
    // Shadow spends no money: a confirmation there would be a ritual, and a ritual teaches
    // people to press "yes" without looking.
    assert!(confirm(false, false).is_ok());
}
