//! Number formatting: what gets read by eye, and on a phone.

use garnet_telegram::fmt;
use rust_decimal_macros::dec;

#[test]
fn a_minus_stands_before_the_dollar_sign() {
    // "$−11 114.71" reads as a currency with an unintelligible sign inside it. The minus
    // applies to the amount as a whole and goes in front of it.
    assert_eq!(fmt::money(dec!(-11114.71)), "−$11\u{202f}114.71");
    assert_eq!(fmt::money(dec!(11114.71)), "$11\u{202f}114.71");
}

#[test]
fn thousands_are_grouped_so_the_eye_finds_the_order() {
    assert_eq!(fmt::money(dec!(11950)), "$11\u{202f}950");
    assert_eq!(fmt::money(dec!(3234.48)), "$3\u{202f}234.48");
    assert_eq!(fmt::money(dec!(25)), "$25");
}

#[test]
fn a_signed_amount_always_carries_its_sign() {
    // "+5" and "5" read the same and mean different things when there is a "−5" next to them.
    assert_eq!(fmt::signed_money(dec!(12.2)), "+$12.20");
    assert_eq!(fmt::signed_money(dec!(-0.47)), "−$0.47");
    assert_eq!(fmt::signed_money(dec!(0)), "$0");
}

#[test]
fn a_fee_below_a_cent_never_becomes_free() {
    // Zero means "free", which does not happen on this exchange: there are exactly as many
    // decimal places as it takes for the fee not to turn into zero.
    assert_eq!(fmt::fee(dec!(0.0004)), "$0.0004");
    assert_eq!(
        fmt::fee(dec!(0.00001)),
        "$0.00001",
        "the floor is numeric(18,6) six places"
    );
    assert_eq!(fmt::fee(dec!(0)), "$0", "a real zero stays zero");
}

#[test]
fn a_fee_of_ordinary_size_is_not_shown_to_four_decimals() {
    // "$14.461" is an internal representation, not an amount: an extra digit on large
    // numbers reads as a typo.
    assert_eq!(fmt::fee(dec!(14.4610)), "$14.46");
    assert_eq!(fmt::fee(dec!(0.3)), "$0.30");
    assert_eq!(fmt::fee(dec!(3.4888)), "$3.49");
}

#[test]
fn a_return_on_nothing_is_not_invented() {
    assert_eq!(fmt::roi(dec!(12.2), dec!(2.09)), Some("+583.7%".into()));
    assert_eq!(fmt::roi(dec!(-1), dec!(4)), Some("−25%".into()));
    assert_eq!(
        fmt::roi(dec!(5), dec!(0)),
        None,
        "there is nothing to divide by"
    );
}

#[test]
fn russian_plurals_follow_the_number() {
    assert_eq!(fmt::plural(1, "position", "positions"), "1 position");
    assert_eq!(fmt::plural(3, "position", "positions"), "3 positions");
    assert_eq!(fmt::plural(0, "position", "positions"), "0 positions");
    assert_eq!(fmt::plural(21, "position", "positions"), "21 positions");
    assert_eq!(fmt::plural(1, "wallet", "wallets"), "1 wallet");
}

#[test]
fn a_clamped_message_says_that_it_was_cut() {
    // A silently lost tail is worse than a rejected message: there it is visible that
    // something is missing, whereas here the reader is sure they are seeing everything.
    let long = "a line of the list\n".repeat(2000);
    let out = fmt::clamp(long, "\n…truncated");

    assert!(out.chars().count() <= fmt::TELEGRAM_LIMIT);
    assert!(out.ends_with("…truncated"));
}

#[test]
fn a_short_message_is_left_alone() {
    let text = "short".to_string();
    assert_eq!(fmt::clamp(text.clone(), "\n…truncated"), text);
}
