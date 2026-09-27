use chrono::{Duration, Utc};
use garnet_copy_engine::{
    buy_limit, decide, decide_with, sell_limit, sizing, Quote, SkipReason, Verdict,
};
use garnet_core::market_meta::{FeeSchedule, MarketMeta};
use garnet_db::{Mode, Wallet};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn wallet(mode: Mode, stake: Decimal, slip: Decimal) -> Wallet {
    Wallet {
        address: "0xleader".into(),
        nickname: Some("whale-1".into()),
        mode,
        stake_usd: stake,
        max_slippage_pct: slip,
        enabled: true,
        created_at: Utc::now(),
    }
}

fn live(stake: Decimal) -> Wallet {
    wallet(Mode::Live, stake, dec!(0.15))
}

fn market(game_start: Option<chrono::DateTime<Utc>>) -> MarketMeta {
    MarketMeta {
        token_id: "tok".into(),
        condition_id: "0xcid".into(),
        question: "Lakers vs Celtics".into(),
        outcome_label: "Los Angeles Lakers".into(),
        category: Some("Sports".into()),
        game_start_time: game_start,
        end_date: None,
        neg_risk: false,
        closed: false,
        fee: FeeSchedule::free(),
        outcome_index: None,
        resolved_outcome: None,
        winner_token_id: None,
        tick: None,
        min_order_size: None,
    }
}

fn quote(leader: Decimal, ask: Decimal) -> Quote {
    Quote {
        leader_price: leader,
        best_ask: Some(ask),
    }
}

/// A book with nothing for us to buy: there are no asks at all.
fn no_book(leader: Decimal) -> Quote {
    Quote {
        leader_price: leader,
        best_ask: None,
    }
}

#[test]
fn copies_in_play_and_pre_game_alike() {
    let pre = market(Some(Utc::now() + Duration::hours(5)));
    let in_play = market(Some(Utc::now() - Duration::minutes(20)));
    for m in [pre, in_play] {
        assert!(
            matches!(
                decide(
                    &live(dec!(25)),
                    &m,
                    quote(dec!(0.30), dec!(0.31)),
                    dec!(1000),
                    false
                ),
                Verdict::Copy { .. }
            ),
            "the phase of the match does not affect the decision"
        );
    }
}

#[test]
fn no_price_band_no_category_no_leader_size_filter() {
    for price in [dec!(0.02), dec!(0.50), dec!(0.97)] {
        let v = decide(
            &live(dec!(25)),
            &market(None),
            quote(price, price),
            dec!(1000),
            false,
        );
        assert!(
            matches!(v, Verdict::Copy { .. }),
            "the price {price} is no reason to refuse"
        );
    }
    // the leader's size is not among the arguments at all — there is nothing to refuse on
}

#[test]
fn stake_is_per_signal_not_per_market() {
    let w = live(dec!(25));
    let m = market(None);
    let first = decide(&w, &m, quote(dec!(0.30), dec!(0.30)), dec!(1000), false);
    let second = decide(&w, &m, quote(dec!(0.35), dec!(0.35)), dec!(1000), false);
    for v in [first, second] {
        match v {
            Verdict::Copy { size_usd, .. } => assert_eq!(size_usd, dec!(25)),
            other => panic!("an add-on buy must be copied: {other:?}"),
        }
    }
}

#[test]
fn slippage_cap_is_the_only_price_guard() {
    let w = live(dec!(25));
    // the leader at 0.30, the ask moved to 0.55 — 83% worse
    assert_eq!(
        decide(
            &w,
            &market(None),
            quote(dec!(0.30), dec!(0.55)),
            dec!(1000),
            false
        ),
        Verdict::Skip(SkipReason::SlippageExceeded)
    );
    // 0.33 — within 15%
    match decide(
        &w,
        &market(None),
        quote(dec!(0.30), dec!(0.33)),
        dec!(1000),
        false,
    ) {
        Verdict::Copy { limit_price, .. } => assert_eq!(limit_price, dec!(0.345)),
        other => panic!("expected Copy, got {other:?}"),
    }
}

#[test]
fn limits_are_clamped_to_the_tradable_range() {
    assert_eq!(
        buy_limit(dec!(0.98), dec!(0.15)),
        dec!(0.999),
        "nothing above 0.999 exists"
    );
    assert_eq!(
        sell_limit(dec!(0.005), dec!(0.99)),
        dec!(0.001),
        "nothing below 0.001 exists"
    );
    assert_eq!(
        sell_limit(dec!(0.40), dec!(0.15)),
        dec!(0.34),
        "the floor mirrors the ceiling"
    );
}

#[test]
fn shadow_never_skips_on_balance() {
    let w = wallet(Mode::Shadow, dec!(25), dec!(0.15));
    let v = decide(
        &w,
        &market(None),
        quote(dec!(0.30), dec!(0.30)),
        dec!(-500),
        false,
    );
    assert!(
        matches!(v, Verdict::Copy { .. }),
        "the shadow ledger is allowed to be negative"
    );
}

#[test]
fn live_skips_when_money_is_short() {
    let v = decide(
        &live(dec!(25)),
        &market(None),
        quote(dec!(0.30), dec!(0.30)),
        dec!(10),
        false,
    );
    assert_eq!(v, Verdict::Skip(SkipReason::InsufficientBalance));
}

#[test]
fn disabled_wallet_and_dead_market_are_the_other_two_skips() {
    let mut off = live(dec!(25));
    off.enabled = false;
    assert_eq!(
        decide(
            &off,
            &market(None),
            quote(dec!(0.30), dec!(0.30)),
            dec!(1000),
            false
        ),
        Verdict::Skip(SkipReason::WalletDisabled)
    );

    let mut resolved = market(None);
    resolved.resolved_outcome = Some("Up".into());
    assert_eq!(
        decide(
            &live(dec!(25)),
            &resolved,
            quote(dec!(0.30), dec!(0.30)),
            dec!(1000),
            false
        ),
        Verdict::Skip(SkipReason::MarketNotTradable)
    );
}

#[test]
fn verdicts_are_written_as_explicit_text() {
    assert_eq!(
        Verdict::Copy {
            size_usd: dec!(25),
            limit_price: dec!(0.3)
        }
        .as_str(),
        "copy"
    );
    assert_eq!(SkipReason::Duplicate.as_str(), "skip:duplicate");
    assert_eq!(
        SkipReason::SlippageExceeded.as_str(),
        "skip:slippage_exceeded"
    );
}

#[test]
fn shares_follow_the_price_paid() {
    assert_eq!(sizing::shares_for(dec!(25), dec!(0.50)), dec!(50));
    assert_eq!(sizing::shares_for(dec!(25), Decimal::ZERO), Decimal::ZERO);
}

/// An empty book is not "expensive".
///
/// Until 06.09.2026 `app.rs` substituted `unwrap_or(1.0)`, and the absence of a book
/// arrived in the statistics as a refusal on slippage: a threshold cannot be tuned
/// from such a log at any sample size, because the "expensive" bucket holds
/// observations where there was no price at all.
#[test]
fn an_empty_book_is_not_slippage() {
    let v = decide(
        &live(dec!(25)),
        &market(None),
        no_book(dec!(0.30)),
        dec!(1000),
        false,
    );
    assert_eq!(
        v,
        Verdict::Skip(SkipReason::MarketNotTradable),
        "no ask — the market is not tradable, rather than the price having moved"
    );
}

/// The absence of a book is not a sixth skip reason: it is the already existing
/// `market_not_tradable`, simply named correctly.
#[test]
fn an_empty_book_adds_no_sixth_reason() {
    let v = decide(
        &live(dec!(25)),
        &market(None),
        no_book(dec!(0.30)),
        dec!(1000),
        false,
    );
    let known = [
        "skip:wallet_disabled",
        "skip:slippage_exceeded",
        "skip:insufficient_balance",
        "skip:market_not_tradable",
        "skip:duplicate",
    ];
    assert!(known.contains(&v.as_str()), "a new verdict: {}", v.as_str());
}

/// A disabled wallet and a closed market are judged before the book: without a book
/// we have nothing to say about price, but the reason for refusal does not change.
#[test]
fn an_empty_book_does_not_mask_earlier_reasons() {
    let mut off = live(dec!(25));
    off.enabled = false;
    assert_eq!(
        decide(&off, &market(None), no_book(dec!(0.30)), dec!(1000), false),
        Verdict::Skip(SkipReason::WalletDisabled)
    );
}

/// A slice of an order we have already copied is not a decision by the leader.
///
/// The leader's taker order consumes as much of the book as is standing there and
/// arrives as that many frames. Measured 06.09.2026: 43% of our buys were such
/// slices, 75% for one wallet. Orders after the first in a wave return -15% against
/// +7.4% for the first, a difference of +23.6 pp with an interval of [3.1; 44.3].
#[test]
fn a_slice_of_an_order_we_already_copied_is_a_duplicate() {
    let v = decide(
        &live(dec!(25)),
        &market(None),
        quote(dec!(0.30), dec!(0.30)),
        dec!(1000),
        true,
    );
    assert_eq!(v, Verdict::Skip(SkipReason::Duplicate));
}

/// This is not a sixth reason: `duplicate` was on the list from the start and until
/// now had never been issued once — a repeat by `tx_hash` is cut off earlier, never
/// giving birth to a signal. The label therefore means exactly one thing.
#[test]
fn a_first_slice_still_copies() {
    let v = decide(
        &live(dec!(25)),
        &market(None),
        quote(dec!(0.30), dec!(0.30)),
        dec!(1000),
        false,
    );
    assert!(matches!(v, Verdict::Copy { .. }));
}

/// The slice check is judged after what cannot be traded at all: a disabled wallet
/// and a closed market remain themselves.
#[test]
fn a_duplicate_does_not_mask_earlier_reasons() {
    let mut off = live(dec!(25));
    off.enabled = false;
    assert_eq!(
        decide(
            &off,
            &market(None),
            quote(dec!(0.30), dec!(0.30)),
            dec!(1000),
            true
        ),
        Verdict::Skip(SkipReason::WalletDisabled)
    );
    let resolved = {
        let mut m = market(None);
        m.closed = true;
        m
    };
    assert_eq!(
        decide(
            &live(dec!(25)),
            &resolved,
            quote(dec!(0.30), dec!(0.30)),
            dec!(1000),
            true
        ),
        Verdict::Skip(SkipReason::MarketNotTradable)
    );
}

/// The slice check is judged BEFORE price: a slice of an already-copied order is not
/// "expensive", and it has no place in the slippage statistics — otherwise the
/// threshold is once again tuned from a bucket holding two different things.
#[test]
fn a_duplicate_outranks_the_price_check() {
    let v = decide(
        &live(dec!(25)),
        &market(None),
        quote(dec!(0.30), dec!(0.99)),
        dec!(1000),
        true,
    );
    assert_eq!(v, Verdict::Skip(SkipReason::Duplicate));
}

/// The per-market exposure ceiling is the **sixth** skip reason.
///
/// It appeared not by a refactor but by an operator's decision on 06.09.2026:
/// concentration within one event is limited by money, not by a count of entries.
/// Measured on the first night: the largest market held 5.9% of the open book at a
/// flat stake.
#[test]
fn a_market_at_its_cap_is_skipped() {
    let w = live(dec!(25));
    let m = market(None);
    // The market already holds $90 of a $100 ceiling: a $25 stake does not fit.
    assert_eq!(
        decide_with(
            &w,
            &m,
            quote(dec!(0.30), dec!(0.30)),
            dec!(1000),
            false,
            dec!(90),
            dec!(100)
        ),
        Verdict::Skip(SkipReason::ExposureCapped)
    );
}

#[test]
fn room_left_in_the_market_still_copies() {
    let w = live(dec!(25));
    let m = market(None);
    assert!(matches!(
        decide_with(
            &w,
            &m,
            quote(dec!(0.30), dec!(0.30)),
            dec!(1000),
            false,
            dec!(70),
            dec!(100)
        ),
        Verdict::Copy { .. }
    ));
}

/// The ceiling cuts on the remainder, not on the stake: the part that fits is half of
/// the leader's decision, not a decision. Either the whole stake, or a refusal.
#[test]
fn a_partial_fit_is_a_refusal_not_a_smaller_bet() {
    let w = live(dec!(25));
    let m = market(None);
    match decide_with(
        &w,
        &m,
        quote(dec!(0.30), dec!(0.30)),
        dec!(1000),
        false,
        dec!(80),
        dec!(100),
    ) {
        Verdict::Skip(SkipReason::ExposureCapped) => {}
        other => {
            panic!("a $20 remainder is less than a $25 stake, a refusal was expected: {other:?}")
        }
    }
}

/// Zero disables the ceiling: otherwise a fresh installation would refuse on the very
/// first signal, and "the bot does not copy" is the most expensive failure in this
/// project.
#[test]
fn a_zero_cap_is_off() {
    let w = live(dec!(25));
    let m = market(None);
    assert!(matches!(
        decide_with(
            &w,
            &m,
            quote(dec!(0.30), dec!(0.30)),
            dec!(1000),
            false,
            dec!(9999),
            Decimal::ZERO
        ),
        Verdict::Copy { .. }
    ));
}

/// The ceiling is judged after price: an order that would not have been taken on
/// slippage anyway must not report itself as having hit the limit.
#[test]
fn price_outranks_the_cap() {
    let w = live(dec!(25));
    let m = market(None);
    assert_eq!(
        decide_with(
            &w,
            &m,
            quote(dec!(0.30), dec!(0.99)),
            dec!(1000),
            false,
            dec!(99),
            dec!(100)
        ),
        Verdict::Skip(SkipReason::SlippageExceeded)
    );
}
