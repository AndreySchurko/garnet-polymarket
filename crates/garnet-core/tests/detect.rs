use garnet_core::detect::{parse_frame, Detector, MarketSource, RawTrade};
use garnet_core::market_meta::{parse_clob_market, MarketMeta};
use garnet_db::{Db, Side, Source};
use rust_decimal_macros::dec;

fn load_fixture(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap()
}

/// A CLOB stub: it answers with what `/markets/<cid>` would really return.
struct StubMarkets;

impl MarketSource for StubMarkets {
    async fn get(&self, token_id: &str) -> anyhow::Result<MarketMeta> {
        let fixture = match token_id {
            "tok_lal" | "tok_bos" => "clob_sports.json",
            "tok_up" | "tok_down" => "clob_crypto.json",
            "tok_over" | "tok_under" => "clob_totals.json",
            other => anyhow::bail!("no fixture for {other}"),
        };
        parse_clob_market(&load_fixture(fixture), token_id)
    }
}

/// A Postgres schema of its own per test: settlement and reconciliation passes are
/// global in meaning, and in a shared schema a neighbouring test would change our rows.
async fn fresh(wallet: &str) -> Db {
    let db = garnet_db::testing::isolated_db("detect").await.unwrap();
    db.wallets().add(wallet, None).await.unwrap();
    db
}

fn detector(db: Db, wallet: &str) -> Detector<StubMarkets> {
    Detector::new(db, StubMarkets, [wallet.to_string()])
}

#[tokio::test]
async fn blank_metadata_frame_is_recovered_not_dropped() {
    // 7.8% of the real stream arrives with empty conditionId/outcome/slug/title
    let frame = load_fixture("rtds_blank_meta.json");
    assert_eq!(
        frame["payload"]["conditionId"], "",
        "the fixture must be empty"
    );
    assert_eq!(frame["payload"]["outcome"], "");

    let db = fresh("0xleader1").await;
    let rows = detector(db, "0xleader1").on_frame(&frame).await.unwrap();

    let rows = rows.trades;
    assert_eq!(
        rows.len(),
        1,
        "a frame with empty metadata must not be lost"
    );
    assert_eq!(rows[0].token_id, "tok_up");
    assert_eq!(
        rows[0].outcome_text, "Up",
        "the outcome was recovered via CLOB tokens[]"
    );
    assert!(
        !rows[0].market_text.is_empty(),
        "the text is captured immediately"
    );
}

#[tokio::test]
async fn same_trade_from_both_paths_is_one_row() {
    let db = fresh("0xleader2").await;
    let mut frame = load_fixture("rtds_normal.json");
    frame["payload"]["proxyWallet"] = serde_json::json!("0xleader2");
    let det = detector(db.clone(), "0xleader2");

    let first = det.on_frame(&frame).await.unwrap().trades;
    assert_eq!(first.len(), 1);

    // the same on-chain trade, arriving via the poll
    let same = RawTrade {
        wallet: "0xleader2".into(),
        tx_hash: "0xaaa1".into(),
        token_id: "tok_lal".into(),
        side: Side::Buy,
        price: dec!(0.42),
        size: dec!(120),
        ts_trade: first[0].ts_trade,
    };
    let second = det
        .ingest_poll(vec![garnet_core::detect::LeaderAction::Trade(same)])
        .await
        .unwrap()
        .trades;

    assert!(second.is_empty(), "a duplicate creates no second row");
    assert_eq!(db.trades().count_for_wallet("0xleader2").await.unwrap(), 1);
}

#[tokio::test]
async fn second_buy_same_params_is_a_new_trade() {
    let db = fresh("0xleader3").await;
    let mut frame = load_fixture("rtds_normal.json");
    frame["payload"]["proxyWallet"] = serde_json::json!("0xleader3");
    let det = detector(db.clone(), "0xleader3");

    det.on_frame(&frame).await.unwrap();
    // a different trade, the same parameters: the leader averaging in
    frame["payload"]["transactionHash"] = serde_json::json!("0xaaa2");
    let again = det.on_frame(&frame).await.unwrap().trades;

    assert_eq!(again.len(), 1, "the leader's averaging must be copied");
    assert_eq!(db.trades().count_for_wallet("0xleader3").await.unwrap(), 2);
}

#[tokio::test]
async fn foreign_wallet_is_ignored() {
    let db = fresh("0xleader4").await;
    let rows = detector(db.clone(), "0xleader4")
        .on_frame(&load_fixture("rtds_foreign.json"))
        .await
        .unwrap()
        .trades;
    assert!(rows.is_empty(), "wallets that are not ours are not ours");
}

#[test]
fn batch_frame_separates_the_merge_from_the_trade() {
    // An RTDS frame labels a row with `side`, not `type`: `"side": "MERGE"`
    // sits next to `"side": "SELL"`. Here too is the answer to a question the plan
    // left open — the socket does carry merges, and until 19.09.2026 we lost them.
    let parsed = parse_frame(&load_fixture("rtds_batch.json"));
    assert_eq!(parsed.len(), 2, "a trade and a merge, not one trade");

    let trades = garnet_core::detect::trades_only(parsed.clone());
    assert_eq!(trades.len(), 1, "one of the two rows is a trade");
    assert_eq!(trades[0].side, Side::Sell);
    assert_eq!(trades[0].token_id, "tok_over");

    let merge = parsed
        .iter()
        .find_map(|a| match a {
            garnet_core::detect::LeaderAction::Other(x) => Some(x),
            garnet_core::detect::LeaderAction::Trade(_) => None,
        })
        .expect("a merge is no longer discarded");
    assert_eq!(merge.kind, garnet_core::detect::ActionKind::Merge);
    assert_eq!(merge.condition_id, "0xsports1");
    assert_eq!(merge.size, dec!(10));
}

#[test]
fn non_trade_frames_are_ignored() {
    let mut f = load_fixture("rtds_normal.json");
    f["type"] = serde_json::json!("orders_matched");
    assert!(parse_frame(&f).is_empty());

    let mut f2 = load_fixture("rtds_normal.json");
    f2["topic"] = serde_json::json!("comments");
    assert!(parse_frame(&f2).is_empty());
}

#[test]
fn zero_and_negative_values_are_not_trades() {
    let mut f = load_fixture("rtds_normal.json");
    f["payload"]["size"] = serde_json::json!(0);
    assert!(parse_frame(&f).is_empty(), "a zero size is not a trade");

    let mut f2 = load_fixture("rtds_normal.json");
    f2["payload"]["price"] = serde_json::json!(-1);
    assert!(parse_frame(&f2).is_empty());
}

#[test]
fn source_is_recorded_per_path() {
    assert_eq!(Source::Rtds.as_str(), "rtds");
    assert_eq!(Source::Poll.as_str(), "poll");
}

// ---------------------------------------------------------------------------
// The safety-net /activity poll
// ---------------------------------------------------------------------------
//
// The second detection circuit. On 31.08.2026 the `activity/trades` topic went down
// platform-wide for hours, from every IP at once, while REST kept returning the same
// trades throughout. The socket and the poll write to one table, and the dedup
// `(tx_hash, wallet, token_id, side)` decides whose copy arrived first.

#[test]
fn activity_rows_become_trades() {
    let rows = load_fixture("activity_rows.json");
    let trades = garnet_core::detect::trades_only(garnet_core::detect::parse_activity(&rows));

    assert_eq!(trades.len(), 2, "two trades out of three rows");
    assert_eq!(trades[0].side, garnet_db::Side::Buy);
    assert_eq!(trades[0].size, dec!(1.518515));
    assert_eq!(trades[0].price, dec!(0.8099999012));
    assert_eq!(
        trades[0].tx_hash,
        "0xdfa409f56bee72f5ace3c392470ea22e5dbe6693486963ff67882fcc95f0d176"
    );
    assert_eq!(trades[1].side, garnet_db::Side::Sell);
}

#[test]
fn a_redemption_is_not_a_trade_but_is_no_longer_lost() {
    // REDEEM, SPLIT and MERGE come through the same endpoint. None of them is a
    // trade — there is nothing to copy, and the price there is not a price. But
    // discarding them is not allowed either: a merge changes the leader's position
    // while trading nothing (invariant 40).
    let rows = load_fixture("activity_rows.json");
    let parsed = garnet_core::detect::parse_activity(&rows);
    let trades = garnet_core::detect::trades_only(parsed.clone());
    assert!(
        trades.iter().all(|t| t.tx_hash != "0xbb22"),
        "a redemption ended up among the trades"
    );
    assert!(
        parsed.iter().any(|a| matches!(a,
            garnet_core::detect::LeaderAction::Other(x) if x.tx_hash == "0xbb22")),
        "a redemption must stay visible rather than vanish"
    );
}

#[test]
fn the_wallet_is_lowercased_the_same_way_as_in_the_socket() {
    // Otherwise one and the same wallet arrives under two different keys, and the
    // dedup lets the second copy through as a new trade.
    let rows = load_fixture("activity_rows.json");
    let trades = garnet_core::detect::trades_only(garnet_core::detect::parse_activity(&rows));
    assert!(trades.iter().all(|t| t.wallet == t.wallet.to_lowercase()));
    assert_eq!(
        trades[0].wallet, trades[1].wallet,
        "letter case creates no second wallet"
    );
}

#[tokio::test]
async fn a_wallet_added_after_startup_is_detected_without_a_restart() {
    // The list of watched wallets used to be fixed at startup, and a wallet added by
    // the operator went unnoticed until the process restarted. For a bot whose only
    // control is the assignment of wallets, that means the control does not
    // work.
    let db = garnet_db::testing::isolated_db("detect_refresh")
        .await
        .unwrap();
    db.wallets().add("0xlate", None).await.unwrap();

    let detector = Detector::new(db.clone(), StubMarkets, Vec::<String>::new());

    let mut frame = load_fixture("rtds_normal.json");
    frame["payload"]["proxyWallet"] = serde_json::json!("0xlate");
    assert!(
        detector.on_frame(&frame).await.unwrap().trades.is_empty(),
        "until a wallet is assigned, its trades are not ours"
    );

    detector.set_watched(["0xLate".to_string()]);
    frame["payload"]["transactionHash"] = serde_json::json!("0xafter");
    let trades = detector.on_frame(&frame).await.unwrap().trades;

    assert_eq!(trades.len(), 1, "after assignment the trades are visible");
    assert_eq!(
        trades[0].wallet, "0xlate",
        "letter case creates no second wallet"
    );
}

/// A leader trade must leave a trace of the market in the database, not only in the cache.
///
/// The process cache lives ten minutes and does not survive a restart; on a resolved
/// market the book answers 404, and the token -> condition path was exactly what we
/// used to fetch from there. That is precisely where settlement needs `condition_id`.
#[tokio::test]
async fn a_seen_trade_leaves_the_market_in_the_registry() {
    let db = fresh("0xmkt1").await;
    let d = detector(db.clone(), "0xmkt1");

    let mut frame = load_fixture("rtds_normal.json");
    frame["payload"]["proxyWallet"] = serde_json::json!("0xmkt1");
    d.on_frame(&frame).await.unwrap();

    let m = db
        .markets()
        .get("tok_lal")
        .await
        .unwrap()
        .expect("the market is recorded end to end, not only into the cache");
    assert_eq!(
        m.outcome_label, "Los Angeles Lakers",
        "the outcome label is not Yes/No"
    );
    assert!(
        !m.condition_id.is_empty(),
        "the condition is the reason the row exists"
    );
}

/// The same market, seen a second time, updates the row rather than growing the table.
#[tokio::test]
async fn seeing_a_market_twice_keeps_one_row() {
    let db = fresh("0xmkt2").await;
    let d = detector(db.clone(), "0xmkt2");

    let mut frame = load_fixture("rtds_normal.json");
    frame["payload"]["proxyWallet"] = serde_json::json!("0xmkt2");
    d.on_frame(&frame).await.unwrap();
    frame["payload"]["transactionHash"] = serde_json::json!("0xaaa2");
    d.on_frame(&frame).await.unwrap();

    assert_eq!(db.markets().count().await.unwrap(), 1);
}

/// Invariant 40: a leader exits by more than selling.
///
/// `MERGE`, `SPLIT` and `REDEEM` arrive in `/activity` via the `type` field and were
/// discarded by `parse_activity` from 03.09.2026. That is not a filter, it is a blind
/// spot: merging a pair is an exit at $1 on both legs, and it arrives in no trade
/// frame at all.
mod actions {
    use super::*;
    use garnet_core::detect::{parse_activity, ActionKind, LeaderAction};

    fn only_actions(v: &serde_json::Value) -> Vec<garnet_core::detect::RawAction> {
        parse_activity(v)
            .into_iter()
            .filter_map(|a| match a {
                LeaderAction::Other(x) => Some(x),
                LeaderAction::Trade(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_merge_and_a_split_are_no_longer_thrown_away() {
        let rows = load_fixture("activity_actions.json");
        let got = only_actions(&rows);

        // The fourth row of the fixture is a merge of zero size: an event without a
        // size says nothing about the position, and there is nothing to act on.
        assert_eq!(
            got.len(),
            3,
            "a merge, a split and a redeem; the zero one is out"
        );

        let merge = got.iter().find(|a| a.kind == ActionKind::Merge).unwrap();
        assert_eq!(merge.size, dec!(4.5));
        assert_eq!(
            merge.condition_id,
            "0x17577aa0f823a43afd36e3c9f77bf9007f09656a37d4a7bb9734397f7442659e",
            "a merge is keyed by condition, not by token: both legs burn"
        );
        assert_eq!(merge.tx_hash, "0xcc33");
        assert_eq!(
            merge.wallet, "0x052e0db825dd10cd00444902565c3a394ccd8d47",
            "the address is lowercased, as it is for trades"
        );

        let split = got.iter().find(|a| a.kind == ActionKind::Split).unwrap();
        assert_eq!(split.size, dec!(10));
        assert_eq!(
            split.wallet, "0x052e0db825dd10cd00444902565c3a394ccd8d47",
            "upper case in the fixture must not spawn a second wallet"
        );

        assert!(got.iter().any(|a| a.kind == ActionKind::Redeem));
    }

    #[test]
    fn a_merge_without_a_condition_is_not_guessed_from_the_token() {
        // The partition is computed by condition. A row without one is an event whose
        // market is unknown; substituting something here would mean acting on a merge
        // in the wrong market.
        let rows = serde_json::json!([{
            "proxyWallet": "0xabc", "timestamp": 1788544400, "conditionId": "",
            "type": "MERGE", "size": 4.5, "transactionHash": "0x01", "asset": "tok_up"
        }]);
        assert!(only_actions(&rows).is_empty());
    }

    #[test]
    fn trades_still_parse_and_do_not_become_actions() {
        // The old behaviour must be preserved to the letter: trades stay trades, and
        // REDEEM from the earlier fixture is still not a trade.
        let rows = load_fixture("activity_rows.json");
        let parsed = parse_activity(&rows);
        let trades: Vec<_> = parsed
            .iter()
            .filter_map(|a| match a {
                LeaderAction::Trade(t) => Some(t),
                LeaderAction::Other(_) => None,
            })
            .collect();

        assert_eq!(trades.len(), 2, "two trades, as before");
        assert_eq!(trades[0].side, Side::Buy);
        assert_eq!(trades[1].side, Side::Sell);

        let acts = only_actions(&rows);
        assert_eq!(acts.len(), 1, "REDEEM is now visible");
        assert_eq!(acts[0].kind, ActionKind::Redeem);
    }
}
