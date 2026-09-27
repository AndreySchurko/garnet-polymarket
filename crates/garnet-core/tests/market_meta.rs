use garnet_core::market_meta::{
    parse_clob_market, parse_condition_id, parse_fee_schedule, snap_price_down, snap_price_up,
    snap_size_down, taker_fee, FeeSchedule,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn load_fixture(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap()
}

fn fee(rate: Decimal) -> FeeSchedule {
    FeeSchedule {
        rate,
        exponent: Decimal::ONE,
        taker_only: true,
    }
}

#[test]
fn fee_is_quadratic_not_linear() {
    // 0.05 * 0.5 * 0.5 = 0.0125 per share; 100 shares -> 1.25
    let f = taker_fee(&fee(dec!(0.05)), dec!(0.5), dec!(100));
    assert_eq!(
        f.round_dp(6),
        dec!(1.250000),
        "the form rate*(p*(1-p))^exponent"
    );

    // towards the edges the fee almost vanishes
    assert_eq!(
        taker_fee(&fee(dec!(0.05)), dec!(0.95), dec!(100)).round_dp(6),
        dec!(0.237500)
    );

    // Two wrong forms, both of which appeared in earlier calculations:
    // a rate on the price overstates twofold, a flat rate on the quantity fourfold.
    let linear_on_price = dec!(0.05) * dec!(0.5) * dec!(100);
    assert_eq!(
        linear_on_price / f,
        dec!(2),
        "rate*p overstates twofold at p=0.5"
    );
    let flat_on_size = dec!(0.05) * dec!(100);
    assert_eq!(
        flat_on_size / f,
        dec!(4),
        "a flat rate overstates fourfold at p=0.5"
    );
}

#[test]
fn free_market_costs_nothing() {
    assert_eq!(
        taker_fee(&FeeSchedule::free(), dec!(0.5), dec!(100)),
        Decimal::ZERO
    );
}

#[test]
fn outcome_labels_are_not_yes_no() {
    for (fixture, token, expect) in [
        ("clob_crypto.json", "tok_up", "Up"),
        ("clob_totals.json", "tok_over", "Over 2.5"),
        ("clob_sports.json", "tok_lal", "Los Angeles Lakers"),
    ] {
        let meta = parse_clob_market(&load_fixture(fixture), token).unwrap();
        assert_eq!(
            meta.outcome_label, expect,
            "{fixture}: the label must be the real one, not Yes/No"
        );
    }
}

#[test]
fn fee_rate_is_per_market_not_per_category() {
    let a = parse_fee_schedule(&load_fixture("gamma_fee_sports_003.json")).unwrap();
    let b = parse_fee_schedule(&load_fixture("gamma_fee_sports_005.json")).unwrap();
    assert_eq!(a.rate, dec!(0.03));
    assert_eq!(b.rate, dec!(0.05));
    assert_ne!(
        a.rate, b.rate,
        "two sports lines with different rates — the category is not a proxy"
    );

    let politics = parse_fee_schedule(&load_fixture("gamma_fee_politics.json")).unwrap();
    assert_eq!(politics.rate, dec!(0.04));
    assert!(politics.taker_only, "only the taker pays the fee");
}

#[test]
fn fees_disabled_means_no_schedule_not_a_missing_field() {
    let f = parse_fee_schedule(&load_fixture("gamma_fee_free.json")).unwrap();
    assert_eq!(
        f.rate,
        Decimal::ZERO,
        "feesEnabled=false means a market without a fee"
    );
}

#[test]
fn sports_timing_comes_from_game_start_time() {
    let m = parse_clob_market(&load_fixture("clob_sports.json"), "tok_lal").unwrap();
    assert!(
        m.game_start_time.is_some(),
        "sports must carry game_start_time"
    );
    assert_ne!(
        m.game_start_time.unwrap(),
        m.end_date.unwrap(),
        "end_date does not replace game_start_time: by it, in-play is indistinguishable from pre-game"
    );

    let crypto = parse_clob_market(&load_fixture("clob_crypto.json"), "tok_up").unwrap();
    assert!(crypto.game_start_time.is_none());
}

#[test]
fn resolution_comes_from_winner_flag_only() {
    let open = parse_clob_market(&load_fixture("clob_crypto.json"), "tok_up").unwrap();
    assert_eq!(open.resolved_outcome, None, "no winner means no resolution");

    let done = parse_clob_market(&load_fixture("clob_resolved.json"), "tok_eth_up").unwrap();
    assert_eq!(done.resolved_outcome.as_deref(), Some("Up"));
    assert!(done.closed);
}

#[test]
fn unknown_token_is_an_error_not_a_guess() {
    let e = parse_clob_market(&load_fixture("clob_sports.json"), "tok_nope").unwrap_err();
    assert!(
        e.to_string().contains("tok_nope"),
        "the error must name the token"
    );
}

#[test]
fn outcome_index_is_positional_not_a_label() {
    use garnet_types::market::OutcomeIndex;

    // The labels of the sides differ — Up/Down, team names — while the outcome index
    // is one and the same mechanism: the position in tokens[]. Redemption addresses
    // exactly that.
    let up = parse_clob_market(&load_fixture("clob_crypto.json"), "tok_up").unwrap();
    let down = parse_clob_market(&load_fixture("clob_crypto.json"), "tok_down").unwrap();
    assert_eq!(up.outcome_index, Some(OutcomeIndex::First));
    assert_eq!(down.outcome_index, Some(OutcomeIndex::Second));

    let lakers = parse_clob_market(&load_fixture("clob_sports.json"), "tok_lal").unwrap();
    assert_eq!(lakers.outcome_index, Some(OutcomeIndex::First));
    assert_eq!(
        lakers.outcome_index.unwrap().index_set(),
        1,
        "the first outcome is index_set 1"
    );

    let celtics = parse_clob_market(&load_fixture("clob_sports.json"), "tok_bos").unwrap();
    assert_eq!(celtics.outcome_index.unwrap().index_set(), 2);
}

// ---------------------------------------------------------------------------
// Tick size and minimum order size
// ---------------------------------------------------------------------------

#[test]
fn the_tick_and_the_minimum_size_come_from_the_clob_market() {
    // Without them the limit price drifts off the exchange's grid, and the order is
    // rejected locally by the SDK's builder — the predecessor merely logged such
    // refusals.
    let m = parse_clob_market(&load_fixture("clob_crypto.json"), "tok_up").unwrap();
    assert_eq!(m.tick, Some(dec!(0.001)));
    assert_eq!(m.min_order_size, Some(dec!(5)));
}

#[test]
fn a_market_without_them_still_parses() {
    // These fields did not appear in every CLOB response everywhere; their absence is
    // no reason to give up on a market.
    let mut raw = load_fixture("clob_crypto.json");
    raw.as_object_mut().unwrap().remove("minimum_tick_size");
    raw.as_object_mut().unwrap().remove("minimum_order_size");
    let m = parse_clob_market(&raw, "tok_up").unwrap();
    assert_eq!(m.tick, None);
    assert_eq!(m.min_order_size, None);
}

#[test]
fn a_buy_limit_snaps_down_to_the_tick() {
    // 0.055 * 1.15 = 0.06325 — off the 0.001 grid. Down, not up: up would mean paying
    // more than the slippage the operator permitted.
    assert_eq!(
        snap_price_down(dec!(0.06325), Some(dec!(0.001))),
        dec!(0.063)
    );
}

#[test]
fn a_price_already_on_the_tick_is_left_alone() {
    assert_eq!(snap_price_down(dec!(0.42), Some(dec!(0.01))), dec!(0.42));
}

#[test]
fn without_a_known_tick_the_price_is_untouched() {
    assert_eq!(snap_price_down(dec!(0.06325), None), dec!(0.06325));
}

#[test]
fn snapping_never_produces_zero() {
    // A price below one tick would round down to zero, and the exchange will not
    // accept an order at zero: we leave one tick.
    assert_eq!(
        snap_price_down(dec!(0.0004), Some(dec!(0.001))),
        dec!(0.001)
    );
}

#[test]
fn a_sell_floor_snaps_up_to_the_tick() {
    // For a sale the limit is a floor: downwards it would drop us below the permitted
    // slippage.
    assert_eq!(snap_price_up(dec!(0.51749), Some(dec!(0.01))), dec!(0.52));
    assert_eq!(snap_price_up(dec!(0.52), Some(dec!(0.01))), dec!(0.52));
    assert_eq!(snap_price_up(dec!(0.51749), None), dec!(0.51749));
}

#[test]
fn the_order_size_is_truncated_to_two_decimals() {
    // A live refusal from the exchange on 2026-09-04: "Size
    // 29.411764705882352941176470588 has 27 decimal places. Maximum lot size is 2".
    // The size comes from dividing the stake by the price and is almost always
    // non-terminating in decimal.
    assert_eq!(
        snap_size_down(dec!(29.411764705882352941176470588)),
        dec!(29.41)
    );
}

#[test]
fn the_size_is_never_rounded_up() {
    // Upwards would mean spending more than the stake the operator set.
    assert_eq!(snap_size_down(dec!(0.999)), dec!(0.99));
    assert_eq!(snap_size_down(dec!(5)), dec!(5));
}

#[test]
fn a_resolved_market_is_reached_through_gamma_when_the_book_is_gone() {
    // Measured 2026-09-04: `GET /book?token_id=` on a resolved market answers 404
    // "No orderbook exists for the requested token id". The token -> condition path
    // breaks exactly where settlement needs it, and the fallback is Gamma queried by
    // the token itself. It returns the condition and the fee in one row.
    let row = load_fixture("gamma_by_token_closed.json");
    assert_eq!(
        parse_condition_id(&row).unwrap(),
        "0x17577aa0f823a43afd36e3c9f77bf9007f09656a37d4a7bb9734397f7442659e"
    );
    assert_eq!(parse_fee_schedule(&row).unwrap().rate, dec!(0.05));
}

#[test]
fn an_empty_gamma_answer_is_an_error_not_a_free_market() {
    // Gamma does not return a closed market without `closed=true`, and an empty array
    // here means "ask differently", not "there is no fee". A silent zero would
    // understate the costs on every resolved market.
    let empty = serde_json::json!([]);
    assert!(parse_condition_id(&empty).is_err());
    assert!(parse_fee_schedule(&empty).is_err());
}
