//! The gate of the emergency close (invariant 46).
//!
//! The tests are named after what each one guards: an irreversible action requires a
//! phrase, not a button, and each mode differs from its neighbour not in force but in
//! what it gives up.

use chrono::{Duration, Utc};
use garnet_core::flatten::{
    check, in_profit, required_phrase, FlattenMode, Gate, Intent, Refusal, INTENT_TTL_SECS,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const NAME: &str = "garnet";

fn intent(mode: FlattenMode, age_secs: i64, ack: bool) -> Intent {
    Intent {
        mode,
        declared_at: Utc::now() - Duration::seconds(age_secs),
        acknowledge_forfeit: ack,
    }
}

#[test]
fn the_right_phrase_on_stopped_trading_goes_through() {
    let i = intent(FlattenMode::Panic, 5, false);
    assert_eq!(
        check(&i, "panic garnet", NAME, Utc::now(), true),
        Gate::Go(FlattenMode::Panic)
    );
}

#[test]
fn a_wrong_phrase_is_refused() {
    let i = intent(FlattenMode::Panic, 5, false);
    let now = Utc::now();
    for wrong in [
        "panic",
        "garnet",
        "PANIC garnet",
        "panic  garnet",
        "panic garnet2",
        "",
    ] {
        assert_eq!(
            check(&i, wrong, NAME, now, true),
            Gate::Refuse(Refusal::WrongPhrase),
            "\"{wrong}\" must not open the gate"
        );
    }
    // Outer whitespace is not intent — the client adds it.
    assert_eq!(
        check(&i, "  panic garnet \n", NAME, now, true),
        Gate::Go(FlattenMode::Panic)
    );
}

#[test]
fn the_phrase_names_this_installation_not_any_installation() {
    // A phrase copied from documentation must not fire on someone else's host: the
    // installation's name in it is not decoration.
    let i = intent(FlattenMode::Panic, 5, false);
    assert_eq!(
        check(&i, "panic garnet", "other-host", Utc::now(), true),
        Gate::Refuse(Refusal::WrongPhrase)
    );
    assert_eq!(
        required_phrase(FlattenMode::Panic, "other-host"),
        "panic other-host"
    );
}

#[test]
fn an_expired_intent_is_refused() {
    let i = intent(FlattenMode::Panic, INTENT_TTL_SECS + 1, false);
    assert!(matches!(
        check(&i, "panic garnet", NAME, Utc::now(), true),
        Gate::Refuse(Refusal::Expired { .. })
    ));
    // The boundary is inclusive: exactly at the limit the intent is still alive.
    let edge = intent(FlattenMode::Panic, INTENT_TTL_SECS, false);
    assert_eq!(
        check(&edge, "panic garnet", NAME, Utc::now(), true),
        Gate::Go(FlattenMode::Panic)
    );
}

#[test]
fn a_timestamp_from_the_future_is_refused_before_it_can_never_expire() {
    // A timestamp from the future has a negative age, and the expiry check would pass
    // it as fresh: such an intent would never expire.
    let i = intent(FlattenMode::Panic, -3600, false);
    assert_eq!(
        check(&i, "panic garnet", NAME, Utc::now(), true),
        Gate::Refuse(Refusal::FromTheFuture)
    );
}

#[test]
fn panic_on_running_trading_is_refused() {
    // Closing everything while continuing to buy is not a halt, it is a swap.
    let i = intent(FlattenMode::Panic, 5, false);
    assert_eq!(
        check(&i, "panic garnet", NAME, Utc::now(), false),
        Gate::Refuse(Refusal::TradingStillRunning)
    );
}

#[test]
fn hybrid_without_acknowledgement_is_refused() {
    // It sells winning positions before resolution, that is, deliberately gives up
    // the expected payout. Inferring that consent from the choice of mode means not
    // asking for it at all.
    let now = Utc::now();
    let no_ack = intent(FlattenMode::Hybrid, 5, false);
    assert_eq!(
        check(&no_ack, "hybrid garnet", NAME, now, true),
        Gate::Refuse(Refusal::ForfeitNotAcknowledged)
    );

    let with_ack = intent(FlattenMode::Hybrid, 5, true);
    assert_eq!(
        check(&with_ack, "hybrid garnet", NAME, now, true),
        Gate::Go(FlattenMode::Hybrid)
    );
}

#[test]
fn hybrid_does_not_need_trading_to_be_stopped_but_panic_does() {
    // The difference is not in force but in what each mode does. `hybrid` takes the
    // profit and does not claim to be a halt; `panic` does.
    let now = Utc::now();
    assert_eq!(
        check(
            &intent(FlattenMode::Hybrid, 5, true),
            "hybrid garnet",
            NAME,
            now,
            false
        ),
        Gate::Go(FlattenMode::Hybrid)
    );
    assert_eq!(
        check(
            &intent(FlattenMode::Graceful, 5, false),
            "graceful garnet",
            NAME,
            now,
            false
        ),
        Gate::Go(FlattenMode::Graceful)
    );
}

#[test]
fn an_unmeasurable_position_is_not_in_profit_and_not_out_of_it() {
    // `hybrid` must leave such a position alone: selling something whose
    // profitability is unknown means going beyond what the operator agreed to
    // (invariant 27).
    assert_eq!(
        in_profit(dec!(10), dec!(0.5), dec!(20), None),
        None,
        "there is no bid"
    );
    assert_eq!(
        in_profit(dec!(10), dec!(0.5), Decimal::ZERO, Some(dec!(0.9))),
        None
    );
}

#[test]
fn the_fee_decides_the_edge_cases_of_profit() {
    // Invariant 4: a position whose proceeds do not cover the fee paid is not in
    // profit.
    // 20 shares for $10 plus $0.50 in fees — a full entry price of $0.525.
    assert_eq!(
        in_profit(dec!(10), dec!(0.5), dec!(20), Some(dec!(0.53))),
        Some(true)
    );
    assert_eq!(
        in_profit(dec!(10), dec!(0.5), dec!(20), Some(dec!(0.51))),
        Some(false),
        "without the fee this would look like a profit"
    );
}
