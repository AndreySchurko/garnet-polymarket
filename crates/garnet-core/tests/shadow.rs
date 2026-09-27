use garnet_core::book::Book;
use garnet_core::market_meta::{taker_fee, FeeSchedule};
use garnet_core::shadow::{simulate_exit, simulate_fill, FillSource};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn load_book(name: &str) -> Book {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    let raw = std::fs::read_to_string(&path).unwrap();
    Book::from_clob(&serde_json::from_str(&raw).unwrap()).unwrap()
}

fn fee(rate: Decimal) -> FeeSchedule {
    FeeSchedule {
        rate,
        exponent: Decimal::ONE,
        taker_only: true,
    }
}

#[test]
fn book_is_resorted_because_clob_returns_asks_descending() {
    // In a live response the first element of asks is 0.999, the best price sits last.
    // Walking in array order would buy at the worst price in the book.
    let b = load_book("book_thin.json");
    assert_eq!(
        b.best_ask(),
        Some(dec!(0.30)),
        "the best ask is the lowest price"
    );
    assert_eq!(
        b.best_bid(),
        Some(dec!(0.28)),
        "the best bid is the highest price"
    );
    assert_eq!(b.asks.first().unwrap().price, dec!(0.30));
    assert_eq!(b.asks.last().unwrap().price, dec!(0.40));
}

#[test]
fn walks_the_ask_ladder_not_the_top_of_book() {
    // 10 shares at 0.30 = 3.00; 10 at 0.34 = 3.40; the remaining 18.60 at 0.40 = 46.5 shares
    let f = simulate_fill(
        &load_book("book_thin.json"),
        dec!(25),
        dec!(0.45),
        &fee(Decimal::ZERO),
    );
    assert_eq!(f.size.round_dp(2), dec!(66.50));
    assert_eq!(f.notional.round_dp(2), dec!(25.00));
    assert_eq!(f.avg_price.round_dp(4), dec!(0.3759));
    assert_eq!(f.source, FillSource::BookWalk);
}

#[test]
fn limit_price_truncates_the_walk() {
    let f = simulate_fill(
        &load_book("book_thin.json"),
        dec!(25),
        dec!(0.35),
        &fee(Decimal::ZERO),
    );
    assert_eq!(
        f.size.round_dp(2),
        dec!(20.00),
        "we take nothing above the limit"
    );
    assert_eq!(f.notional.round_dp(2), dec!(6.40));
}

#[test]
fn nothing_inside_the_limit_means_no_fill() {
    let f = simulate_fill(
        &load_book("book_thin.json"),
        dec!(25),
        dec!(0.10),
        &fee(dec!(0.05)),
    );
    assert!(f.is_empty());
    assert_eq!(f.fee_usd, Decimal::ZERO, "nothing filled, nothing paid");
}

#[test]
fn shadow_pays_the_same_fee_as_live() {
    let schedule = fee(dec!(0.05));
    let f = simulate_fill(
        &load_book("book_thin.json"),
        dec!(25),
        dec!(0.45),
        &schedule,
    );

    let expected = taker_fee(&schedule, f.avg_price, f.size);
    assert_eq!(f.fee_usd, expected);
    assert!(
        f.fee_usd > Decimal::ZERO,
        "a free shadow would cheat the comparison"
    );
}

#[test]
fn free_market_costs_nothing_in_shadow_either() {
    let f = simulate_fill(
        &load_book("book_thin.json"),
        dec!(25),
        dec!(0.45),
        &FeeSchedule::free(),
    );
    assert_eq!(f.fee_usd, Decimal::ZERO);
}

#[test]
fn exit_walks_the_bid_ladder_from_the_best_price() {
    // The bids in the fixture sit at 0.20 / 0.28, best last. Selling 60 shares:
    // 40 at 0.28 = 11.20, the remaining 20 at 0.20 = 4.00.
    let f = simulate_exit(
        &load_book("book_thin.json"),
        dec!(60),
        dec!(0.15),
        &fee(Decimal::ZERO),
    );
    assert_eq!(f.size.round_dp(2), dec!(60.00));
    assert_eq!(f.notional.round_dp(2), dec!(15.20));
    assert_eq!(f.avg_price.round_dp(4), dec!(0.2533));
    assert_eq!(f.source, FillSource::BookWalk);
}

#[test]
fn the_exit_floor_truncates_the_walk() {
    let f = simulate_exit(
        &load_book("book_thin.json"),
        dec!(60),
        dec!(0.25),
        &fee(Decimal::ZERO),
    );
    assert_eq!(
        f.size.round_dp(2),
        dec!(40.00),
        "we part with nothing below the floor"
    );
    assert_eq!(f.notional.round_dp(2), dec!(11.20));
}

#[test]
fn an_exit_below_every_bid_sells_nothing() {
    // The floor is above the best bid — the position lives to resolution. An exit does
    // not turn into a market order at any price: that is no longer copying.
    let f = simulate_exit(
        &load_book("book_thin.json"),
        dec!(60),
        dec!(0.36),
        &fee(Decimal::ZERO),
    );
    assert!(f.is_empty());
}

#[test]
fn a_simulated_exit_pays_the_same_taker_fee_as_live() {
    // Invariant 5: without the fee, shadow is systematically better than live by
    // exactly its amount, and the comparison between the modes would be measuring our
    // own undercount.
    let f = simulate_exit(
        &load_book("book_thin.json"),
        dec!(40),
        dec!(0.25),
        &fee(dec!(0.05)),
    );
    assert_eq!(f.fee_usd, taker_fee(&fee(dec!(0.05)), dec!(0.28), dec!(40)));
    assert!(f.fee_usd > Decimal::ZERO);
}
