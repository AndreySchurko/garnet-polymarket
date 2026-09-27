//! Notifications: a bus event -> text for the operator.
//!
//! The rendering is checked by a pure function, the delivery against a live NATS and real
//! HTTP: a push that did not arrive is indistinguishable from one that was never sent.

use garnet_telegram::push::push_text;

#[test]
fn a_live_fill_reads_like_a_sentence() {
    let ev = serde_json::json!({
        "wallet": "0xabc", "mode": "live", "side": "buy",
        "token_id": "tok", "size": "59.5", "avg_price": "0.42",
        "notional": "24.99", "fee_usd": "0.30"
    });
    let text = push_text("order.filled", &ev, Some("whale-1"), Some("Lakers to win")).unwrap();

    assert!(text.contains("Real money"), "{text}");
    assert!(
        text.contains("whale-1"),
        "the wallet's name, not its address: {text}"
    );
    assert!(
        text.contains("Lakers to win"),
        "the outcome in words: {text}"
    );
    assert!(text.contains("0.42") && text.contains("59.5"), "{text}");
}

#[test]
fn a_shadow_fill_says_it_is_hypothetical() {
    // Otherwise the operator sees a stream of "buys" and concludes money is being spent.
    let ev = serde_json::json!({
        "wallet": "0xabc", "mode": "shadow", "side": "buy",
        "token_id": "tok", "size": "10", "avg_price": "0.5",
        "notional": "5", "fee_usd": "0"
    });
    let text = push_text("order.filled", &ev, None, None).unwrap();
    assert!(text.contains("Paper"), "{text}");
    assert!(!text.contains("SHADOW"), "the jargon is gone: {text}");
}

#[test]
fn an_unnamed_market_falls_back_to_the_token() {
    // The metadata is sometimes unavailable (7.8% of RTDS frames arrive empty). A push
    // without the market's name is better than no push.
    let ev = serde_json::json!({
        "wallet": "0xabc", "mode": "live", "side": "buy",
        "token_id": "78750515827071533171749126745679443330143284910428860198446982709710167332570",
        "size": "3", "avg_price": "0.81", "notional": "2.46", "fee_usd": "0.02"
    });
    let text = push_text("order.filled", &ev, None, None).unwrap();
    assert!(text.contains("787505"), "at least the token: {text}");
}

#[test]
fn a_settled_position_reports_the_outcome_and_the_result() {
    let ev = serde_json::json!({
        "wallet": "0xabc", "mode": "live", "token_id": "tok",
        "resolved_outcome": "Down", "won": true,
        "size": "4", "payout_usd": "4", "pnl_usd": "2.95"
    });
    let text = push_text("position.settled", &ev, Some("whale-1"), Some("Down")).unwrap();
    assert!(
        text.contains("Down"),
        "the outcome label, not Yes/No: {text}"
    );
    assert!(text.contains("2.95"), "the position's bottom line: {text}");
}

#[test]
fn a_rejection_and_a_killswitch_are_loud() {
    let rejected = push_text(
        "alert.order_rejected",
        &serde_json::json!({ "token_id": "tok", "mode": "live", "reason": "not enough balance" }),
        None,
        None,
    )
    .unwrap();
    assert!(rejected.contains("not enough balance"), "{rejected}");

    let kill = push_text(
        "alert.killswitch",
        &serde_json::json!({ "text": "killswitch armed: the feed stalled" }),
        None,
        None,
    )
    .unwrap();
    assert!(kill.contains("the feed stalled"), "{kill}");
}

#[test]
fn an_unknown_subject_is_not_forwarded() {
    // The bus also carries what is not addressed to the operator: `signal.detected` fires on
    // every leader trade, skipped ones included, and would turn the chat into a stream.
    assert!(push_text("signal.detected", &serde_json::json!({}), None, None).is_none());
}

use garnet_telegram::push::wanted;

#[test]
fn shadow_fills_can_be_kept_out_of_the_chat() {
    // Measured 2026-09-04: twelve wallets in shadow produced 83 fills within minutes. A push
    // for each one is a stream people stop reading, and with it they stop noticing the real
    // alerts. Shadow is a measuring instrument, and its place is the dashboard.

    let shadow = serde_json::json!({ "mode": "shadow" });
    let live = serde_json::json!({ "mode": "live" });

    assert!(!wanted("order.filled", &shadow, "live"));
    assert!(wanted("order.filled", &live, "live"));
    assert!(wanted("order.filled", &shadow, "all"));
    assert!(!wanted("order.filled", &live, "none"));
}

#[test]
fn alerts_and_resolutions_are_never_filtered() {
    // The setting concerns the stream of fills. The killswitch, a stalled feed and a
    // resolution are the events notifications exist for in the first place.
    let shadow = serde_json::json!({ "mode": "shadow" });
    for subject in [
        "alert.killswitch",
        "alert.order_rejected",
        "position.settled",
    ] {
        assert!(wanted(subject, &shadow, "none"), "{subject}");
    }
}

#[test]
fn the_numbers_in_a_push_are_for_reading() {
    // Numbers arrive in an event as strings out of `numeric(18,6)` and divisions: "59.500000
    // shares", "price 0.4200", "$24.990000". That is an internal representation, while a
    // notification is read by eye, on a phone.
    let ev = serde_json::json!({
        "wallet": "0xabc", "mode": "live", "side": "buy", "token_id": "tok",
        "size": "59.500000", "avg_price": "0.420000",
        "notional": "24.990000", "fee_usd": "0.017960"
    });
    let text = push_text("order.filled", &ev, Some("whale"), Some("Lakers")).unwrap();

    assert!(text.contains("59.5"), "{text}");
    assert!(
        !text.contains("59.500000"),
        "trailing zeros on the size: {text}"
    );
    assert!(
        text.contains("0.42") && !text.contains("0.420000"),
        "the price: {text}"
    );
    assert!(
        text.contains("24.99") && !text.contains("24.990000"),
        "the notional: {text}"
    );
    // A fee can be less than a cent, and rounding to two places would turn it into zero —
    // and zero means "free", which does not happen.
    assert!(text.contains("$0.02"), "the fee was not lost: {text}");
    assert!(
        !text.contains(
            "Fees         $0
"
        ),
        "and did not become zero: {text}"
    );
}

#[test]
fn a_number_that_is_not_a_number_is_shown_as_it_came() {
    // The field may not have arrived at all: showing "?" is more honest than "0".
    let ev = serde_json::json!({ "mode": "live", "size": "lots" });
    let text = push_text("order.filled", &ev, None, None).unwrap();
    assert!(text.contains("lots") || text.contains('?'), "{text}");
}

/// A resolution as the operator sees it.
fn settled_ev(mode: &str, won: bool, payout: &str, pnl: &str) -> serde_json::Value {
    serde_json::json!({
        "position_id": 1, "wallet": "0xabc", "token_id": "tok", "mode": mode,
        "resolved_outcome": "Betis", "won": won, "size": "14.29",
        "payout_usd": payout, "cost_usd": "2.09", "proceeds_usd": "0",
        "fees_usd": "0.04", "pnl_usd": pnl
    })
}

#[test]
fn a_resolution_says_whether_our_bet_won() {
    // This is exactly what the operator asked: "from the resolutions it is not clear whether
    // our bet won, and what the payout and the total mean".
    let text = push_text(
        "position.settled",
        &settled_ev("shadow", true, "14.29", "12.20"),
        Some("whale-1"),
        Some("Betis"),
    )
    .unwrap();

    assert!(
        text.contains("Won"),
        "the verdict on the first line: {text}"
    );
    assert!(text.contains("Stake"), "the stake is named: {text}");
    assert!(text.contains("Payout"), "the payout is named: {text}");
    assert!(
        text.contains("Result"),
        "the result is named in words: {text}"
    );
    assert!(
        !text.contains("total $"),
        "an unexplained \"total\" is gone: {text}"
    );
}

#[test]
fn a_lost_resolution_says_so_plainly() {
    let text = push_text(
        "position.settled",
        &settled_ev("shadow", false, "0", "-0.47"),
        Some("whale-1"),
        Some("SPX Up"),
    )
    .unwrap();

    assert!(text.contains("Lost"), "{text}");
    assert!(text.contains("−$0.47"), "{text}");
}

#[test]
fn a_win_that_still_lost_money_names_both() {
    // A winning outcome bought expensively brings a loss: the verdict is about the outcome,
    // the result is about money, and they must not be confused.
    let text = push_text(
        "position.settled",
        &settled_ev("shadow", true, "1.00", "-0.05"),
        None,
        Some("Yes"),
    )
    .unwrap();

    assert!(text.contains("Won"), "{text}");
    assert!(text.contains("−$0.05"), "the result is honest: {text}");
}

#[test]
fn money_at_risk_is_named_in_words_not_in_jargon() {
    // "LIVE" and "SHADOW" say nothing to a person who did not write this code, and the
    // difference between them is real money.
    let real = push_text(
        "position.settled",
        &settled_ev("live", true, "14.29", "12.20"),
        None,
        Some("Betis"),
    )
    .unwrap();
    let paper = push_text(
        "position.settled",
        &settled_ev("shadow", true, "14.29", "12.20"),
        None,
        Some("Betis"),
    )
    .unwrap();

    assert!(real.contains("real money"), "{real}");
    assert!(paper.contains("paper"), "{paper}");
    assert!(!paper.contains("SHADOW"), "the jargon is gone: {paper}");
    assert!(!real.contains("LIVE"), "the jargon is gone: {real}");
}

/// Invariant 47: "the check could not be performed" has to reach the operator.
#[test]
fn an_unreadable_balance_reaches_the_operator() {
    let ev = serde_json::json!({ "text": "tok_a: the balance could not be read — the RPC did not answer" });
    let text = push_text(
        garnet_bus::subjects::ALERT_RECONCILE_UNREADABLE,
        &ev,
        None,
        None,
    )
    .expect("the alert must arrive rather than be lost");
    assert!(text.contains("could not be read"), "{text}");
}

/// The `_ => None` branch swallowed any subject not on the list: a new alert reached the bus,
/// reached the `alert.*` subscription — and died silently. An alert the operator never
/// learned about is worse than a missing one: the first creates confidence that all is
/// quiet.
#[test]
fn an_unknown_alert_is_shown_not_swallowed() {
    let ev = serde_json::json!({ "text": "something new" });
    let text =
        push_text("alert.something_new", &ev, None, None).expect("an unknown alert must be shown");
    assert!(text.contains("something new"), "{text}");
    assert!(
        text.contains("alert.something_new"),
        "and name itself: {text}"
    );

    // Non-alerts are still not broadcast: they have a subscription of their own.
    assert!(push_text("order.submitted", &ev, None, None).is_none());
}
