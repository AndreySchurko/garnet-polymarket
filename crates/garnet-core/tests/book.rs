//! The order book: the order of levels and the mid of the market.

use garnet_core::book::Book;
use rust_decimal_macros::dec;

fn fixture(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn the_best_levels_come_from_price_not_from_array_order() {
    // `/book` returns asks by descending price: the first element is the worst price.
    let b = Book::from_clob(&fixture("book_thin.json")).unwrap();
    assert_eq!(b.best_ask(), Some(dec!(0.30)));
    assert_eq!(b.best_bid(), Some(dec!(0.28)));
}

#[test]
fn the_mid_is_the_middle_of_the_two_best_prices() {
    // An open position is valued at the mid: the ask would overstate equity by the
    // spread, the bid would understate it by the same.
    let b = Book::from_clob(&fixture("book_thin.json")).unwrap();
    assert_eq!(b.mid(), Some(dec!(0.29)));
}

#[test]
fn a_one_sided_book_has_no_mid() {
    // A position without a price must count as unpriced and be visible as a number.
    // An invented mid would distort equity silently.
    let raw = serde_json::json!({
        "asks": [{ "price": "0.30", "size": "10" }],
        "bids": []
    });
    assert_eq!(Book::from_clob(&raw).unwrap().mid(), None);
}

#[test]
fn an_empty_book_has_no_mid() {
    let raw = serde_json::json!({ "asks": [], "bids": [] });
    assert_eq!(Book::from_clob(&raw).unwrap().mid(), None);
}
