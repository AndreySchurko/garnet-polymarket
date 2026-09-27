//! Decoding Polygon logs.
//!
//! The `chain_order_filled.json` fixture holds **real logs**, lifted from the network on
//! 19.09.2026 rather than assembled by hand. One trade: the passive maker buys, the
//! aggressor sells.

use garnet_feed::chain::{parse_log, wallet_topic, Side, ORDER_FILLED_TOPIC0};
use rust_decimal_macros::dec;

fn logs() -> Vec<serde_json::Value> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/chain_order_filled.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["logs"].as_array().unwrap().clone()
}

#[test]
fn a_real_log_decodes_to_the_price_the_market_actually_traded() {
    let fills: Vec<_> = logs().iter().filter_map(parse_log).collect();
    assert_eq!(
        fills.len(),
        2,
        "two OrderFilled events; OrdersMatched does not belong here"
    );

    // The maker bought 17.15 shares for 9.4325 USDC.
    let buy = fills.iter().find(|f| f.side == Side::Buy).unwrap();
    assert_eq!(buy.size, dec!(17.15));
    assert_eq!(buy.price, dec!(0.55));
    assert_eq!(buy.maker, "0x06dc51826bc524d9a83770e7de9dd7e005b04524");

    // The aggressor sold the same 17.15 at the same price.
    let sell = fills.iter().find(|f| f.side == Side::Sell).unwrap();
    assert_eq!(
        sell.size,
        dec!(17.15),
        "on a sale the size is shares, not dollars"
    );
    assert_eq!(
        sell.price,
        dec!(0.55),
        "one price for both legs of one trade"
    );
    assert_eq!(sell.maker, "0x83e8f2ea25df8e71fcaf2271c9edaae02860b9bc");

    assert_eq!(buy.token_id, sell.token_id, "one and the same token");
    assert_eq!(
        buy.token_id,
        "1521370259019747935371840349883807452048543412405834017532412091561547037718",
        "the token identifier does not fit in a u128 and must be read exactly"
    );
}

#[test]
fn the_aggressor_is_the_maker_of_its_own_log() {
    // The answer to a question left open, and it is the OPPOSITE of the fear: a second
    // filter on `topics[3]` is not needed. The aggressor has an `OrderFilled` of its
    // own, where it is the maker while `taker` is the exchange's address.

    let fills: Vec<_> = logs().iter().filter_map(parse_log).collect();
    let sell = fills.iter().find(|f| f.side == Side::Sell).unwrap();
    assert_eq!(
        sell.taker, "0xe111180000d2663c0091e4f400237545b87b996b",
        "the counterparty of the aggressor's leg is the exchange itself"
    );

    // The same address is the counterparty in the passive maker's leg.
    let buy = fills.iter().find(|f| f.side == Side::Buy).unwrap();
    assert_eq!(
        buy.taker, sell.maker,
        "the aggressor is visible from both sides of the trade"
    );
}

#[test]
fn the_side_is_read_from_the_field_not_inferred_from_a_zero_asset_id() {
    // In V1 the side was inferred from which `assetId` was zero. In V2 there is an
    // explicit `side` field, and carrying the zero-based inference across would give a
    // decoder that parses and answers the wrong question. Here the first data word is 0
    // (BUY) on one leg and 1 (SELL) on the other, with one and the same token.
    let l = logs();
    let buy = parse_log(&l[0]).unwrap();
    let sell = parse_log(&l[1]).unwrap();
    assert_eq!(buy.side, Side::Buy);
    assert_eq!(sell.side, Side::Sell);
    assert!(
        !buy.token_id.starts_with('0') && buy.token_id == sell.token_id,
        "neither leg has a zero identifier to guess from"
    );
}

#[test]
fn a_foreign_event_is_not_a_fill() {
    // `OrdersMatched` comes from the same address in the same transaction. Decoding it
    // as a fill would double the trade.
    let matched = logs()
        .into_iter()
        .find(|l| l["topics"][0].as_str() != Some(ORDER_FILLED_TOPIC0))
        .expect("the fixture contains OrdersMatched");
    assert!(parse_log(&matched).is_none());
}

#[test]
fn a_log_of_another_contract_version_is_refused_not_guessed() {
    // Seven data words means V2. Five words means V1, where the fields sit elsewhere:
    // decoding them with this function yields plausible numbers from the wrong fields.

    let mut v1 = logs()[0].clone();
    let d = v1["data"].as_str().unwrap().to_string();
    v1["data"] = serde_json::json!(d[..2 + 64 * 5].to_string());
    assert!(
        parse_log(&v1).is_none(),
        "a foreign contract version is not decoded"
    );
}

#[test]
fn a_zero_amount_is_not_a_fill() {
    // A zero size would give a division by zero, and a zero price a trade at zero.
    let mut zero = logs()[0].clone();
    let d = zero["data"].as_str().unwrap().to_string();
    let z = "0".repeat(64);
    zero["data"] = serde_json::json!(format!("{}{}{}", &d[..2 + 64 * 3], z, &d[2 + 64 * 4..]));
    assert!(parse_log(&zero).is_none());
}

#[test]
fn the_wallet_topic_is_the_address_padded_to_a_word() {
    assert_eq!(
        wallet_topic("0x83e8f2ea25df8e71fcaf2271c9edaae02860b9bc"),
        "0x00000000000000000000000083e8f2ea25df8e71fcaf2271c9edaae02860b9bc"
    );
    // Letter case creates no second filter: the node compares bytes.
    assert_eq!(
        wallet_topic("0x83E8F2EA25DF8E71FCAF2271C9EDAAE02860B9BC"),
        wallet_topic("83e8f2ea25df8e71fcaf2271c9edaae02860b9bc")
    );
}

#[test]
fn an_absurd_amount_is_refused_and_does_not_kill_the_circuit() {
    // The word comes from someone else's log — a value we do not choose.
    // `Decimal::from(u128)` panics on such a value and would take out the whole circuit
    // for the sake of one unusable log.
    let mut absurd = logs()[0].clone();
    let d = absurd["data"].as_str().unwrap().to_string();
    let max = "f".repeat(64);
    absurd["data"] = serde_json::json!(format!("{}{}{}", &d[..2 + 64 * 2], max, &d[2 + 64 * 3..]));
    assert!(
        parse_log(&absurd).is_none(),
        "an unusable log costs only itself"
    );
}

mod transport {
    use super::*;
    use garnet_feed::chain::{log_of_notification, subscribe_frame};

    #[test]
    fn the_subscription_filters_on_the_node_not_on_us() {
        // The point of the circuit is that the node wakes us when a leader trades, not
        // when anyone at all trades: without a filter we would have to accept the
        // platform's entire feed — 7255 fills over 120 blocks.
        let f = subscribe_frame(
            1,
            &["0xE111180000d2663C0091e4f400237545B87B996B".into()],
            &["0x83e8f2ea25df8e71fcaf2271c9edaae02860b9bc".into()],
        );
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["method"], "eth_subscribe");
        assert_eq!(v["params"][0], "logs");

        let topics = v["params"][1]["topics"].as_array().unwrap();
        assert_eq!(topics.len(), 3, "the zeroth, a skip and the maker");
        assert_eq!(topics[0], ORDER_FILLED_TOPIC0);
        assert!(topics[1].is_null(), "orderHash is not filtered");
        assert_eq!(
            topics[2][0],
            "0x00000000000000000000000083e8f2ea25df8e71fcaf2271c9edaae02860b9bc"
        );
    }

    #[test]
    fn a_log_removed_by_a_reorg_is_not_a_trade() {
        // The node sends removed logs on a chain reorganisation. Copying one means
        // buying something that no longer exists on chain.
        let raw = serde_json::json!({
            "method": "eth_subscription",
            "params": { "result": { "removed": true, "topics": [], "data": "0x" } }
        })
        .to_string();
        assert!(log_of_notification(&raw).is_none());
    }

    #[test]
    fn a_subscription_acknowledgement_is_not_a_log() {
        // The reply to `eth_subscribe` arrives on the same socket and is not a log.
        let ack = r#"{"jsonrpc":"2.0","id":1,"result":"0x9ce59a13059e417087c02d3236a0b1cc"}"#;
        assert!(log_of_notification(ack).is_none());
    }

    #[test]
    fn a_live_notification_decodes_end_to_end() {
        // A notification frame wrapped around a real log from the fixture.
        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_subscription",
            "params": { "subscription": "0x9ce5", "result": logs()[0].clone() }
        })
        .to_string();
        let log = log_of_notification(&raw).expect("the notification was decoded");
        let fill = parse_log(&log).expect("the log was decoded");
        assert_eq!(fill.price, dec!(0.55));
        assert_eq!(fill.size, dec!(17.15));
    }
}
