//! The payout the platform credits.
//!
//! A Polymarket account with auto-payout enabled redeems winning positions itself: the
//! outcome tokens sit on the proxy, its relay pays the gas, and the pUSD arrives without our
//! involvement. In this configuration our `redeem_position` is not merely unnecessary — it
//! would not work: it sends the transaction from an EOA that does not hold those tokens.
//!
//! So in this configuration the check reads differently: what is verified is not our
//! redemption but the agreement of the credited payout with our ledger.

use garnet_bin::auto_payout::AutoPayout;
use garnet_core::detect::MarketSource;
use garnet_core::market_meta::{parse_clob_market, MarketMeta};
use garnet_core::settle::settle_once;
use garnet_db::{Db, Mode};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn fixture(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct StubMarkets;

impl MarketSource for StubMarkets {
    async fn get(&self, token_id: &str) -> anyhow::Result<MarketMeta> {
        parse_clob_market(&fixture("clob_resolved_down.json"), token_id)
    }
}

async fn db(wallet: &str) -> Db {
    let db = garnet_db::testing::isolated_db("autopayout").await.unwrap();
    db.wallets().add(wallet, None).await.unwrap();
    db
}

#[tokio::test]
async fn a_win_is_settled_without_any_transaction_of_ours() {
    let db = db("0xauto_win").await;
    let pos = db
        .positions()
        .apply_buy(
            "0xauto_win",
            "tok_sol_down",
            Mode::Live,
            dec!(100),
            dec!(40),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let report = settle_once(&db, &StubMarkets, &AutoPayout, 100)
        .await
        .unwrap();
    assert_eq!(report.resolved, 1);

    let s = db
        .settlements()
        .for_position(pos.id)
        .await
        .unwrap()
        .expect("the settlement was recorded");
    assert!(s.won);
    assert_eq!(s.payout_usd, dec!(100), "1.0 per share of the winning side");
    assert_eq!(
        s.tx_hash, None,
        "there is no transaction: the platform credited the payout, not us"
    );
}

#[tokio::test]
async fn a_loss_is_settled_the_same_way_as_anywhere_else() {
    let db = db("0xauto_lose").await;
    let pos = db
        .positions()
        .apply_buy(
            "0xauto_lose",
            "tok_sol_up",
            Mode::Live,
            dec!(100),
            dec!(60),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    settle_once(&db, &StubMarkets, &AutoPayout, 100)
        .await
        .unwrap();

    let s = db
        .settlements()
        .for_position(pos.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!s.won);
    assert_eq!(s.payout_usd, Decimal::ZERO);
}
