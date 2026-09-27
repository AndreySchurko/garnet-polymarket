use garnet_core::equity::{snapshot_equity, PriceSource};
use garnet_core::reconcile::{reconcile_once, AlertSink, ChainBalances, Divergence};
use garnet_db::{Db, Mode};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::Mutex;

async fn db(wallet: &str) -> Db {
    let db = garnet_db::testing::isolated_db("equity").await.unwrap();
    db.wallets().add(wallet, None).await.unwrap();
    db
}

struct StubPrices(HashMap<String, Decimal>);

impl StubPrices {
    fn flat(p: Decimal) -> Self {
        let mut m = HashMap::new();
        m.insert("tok_a".to_string(), p);
        m.insert("tok_b".to_string(), p);
        Self(m)
    }
}

impl PriceSource for StubPrices {
    async fn mid(&self, token_id: &str) -> anyhow::Result<Option<Decimal>> {
        Ok(self.0.get(token_id).copied())
    }
}

struct StubChain(HashMap<String, Decimal>);

impl ChainBalances for StubChain {
    async fn balance_of(&self, token_id: &str) -> anyhow::Result<Decimal> {
        Ok(self.0.get(token_id).copied().unwrap_or(Decimal::ZERO))
    }
}

#[derive(Default)]
struct SpyAlerts(Mutex<Vec<(String, String)>>);

impl AlertSink for SpyAlerts {
    async fn alert(&self, subject: &str, text: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().push((subject.into(), text.into()));
        Ok(())
    }
}

impl SpyAlerts {
    fn published(&self, subject: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| s == subject)
            .count()
    }
}

#[tokio::test]
async fn snapshot_separates_modes_and_prices_open_positions() {
    let db = db("0xeq").await;
    // live: 50 shares bought for 20; shadow: 50 shares for 15
    db.positions()
        .apply_buy(
            "0xeq",
            "tok_a",
            Mode::Live,
            dec!(50),
            dec!(20),
            Decimal::ZERO,
        )
        .await
        .unwrap();
    db.positions()
        .apply_buy(
            "0xeq",
            "tok_a",
            Mode::Shadow,
            dec!(50),
            dec!(15),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let prices = StubPrices::flat(dec!(0.50));
    let live = snapshot_equity(&db, Mode::Live, dec!(500), &prices)
        .await
        .unwrap();
    let shadow = snapshot_equity(&db, Mode::Shadow, dec!(1000), &prices)
        .await
        .unwrap();

    assert_eq!(live.positions_value, dec!(25), "50 shares at 0.50");
    assert_eq!(
        live.total_usd,
        dec!(525),
        "the total = free funds plus positions"
    );
    assert_eq!(
        shadow.cash_usd,
        dec!(1000),
        "the virtual ledger is separate from the on-chain balance"
    );
    assert_ne!(live.cash_usd, shadow.cash_usd);
}

#[tokio::test]
async fn unpriced_positions_are_counted_not_hidden() {
    let db = db("0xeq_unpriced").await;
    db.positions()
        .apply_buy(
            "0xeq_unpriced",
            "tok_unknown",
            Mode::Live,
            dec!(10),
            dec!(4),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let e = snapshot_equity(&db, Mode::Live, dec!(100), &StubPrices::flat(dec!(0.5)))
        .await
        .unwrap();
    assert_eq!(e.unpriced, 1, "a position without a price must be visible");
    assert_eq!(e.positions_value, Decimal::ZERO);
}

#[tokio::test]
async fn snapshots_form_a_series_for_the_chart() {
    let db = db("0xeq_series").await;
    let prices = StubPrices::flat(dec!(0.5));
    for cash in [dec!(1000), dec!(980), dec!(1010)] {
        snapshot_equity(&db, Mode::Shadow, cash, &prices)
            .await
            .unwrap();
    }

    let series = db.equity().series(Mode::Shadow, 10).await.unwrap();
    assert_eq!(series.len(), 3);
    assert_eq!(
        series.first().unwrap().cash_usd,
        dec!(1000),
        "the series runs oldest to newest"
    );
    assert_eq!(
        db.equity()
            .latest(Mode::Shadow)
            .await
            .unwrap()
            .unwrap()
            .cash_usd,
        dec!(1010)
    );
}

#[tokio::test]
async fn divergence_alerts_and_never_self_heals() {
    let db = db("0xrec").await;
    db.positions()
        .apply_buy(
            "0xrec",
            "tok_a",
            Mode::Live,
            dec!(50),
            dec!(20),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    // the chain holds only 30 of the 50
    let chain = StubChain(HashMap::from([("tok_a".to_string(), dec!(30))]));
    let alerts = SpyAlerts::default();
    let report = reconcile_once(&db, &chain, &ResolutionStub, &alerts, 900)
        .await
        .unwrap();

    assert_eq!(
        report.divergences,
        vec![Divergence {
            token_id: "tok_a".into(),
            in_db: dec!(50),
            on_chain: dec!(30)
        }]
    );
    assert_eq!(alerts.published("alert.reconcile_divergence"), 1);

    let after = db
        .positions()
        .get("0xrec", "tok_a", Mode::Live)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.size_bought,
        dec!(50),
        "a silent auto-correction is forbidden"
    );
}

#[tokio::test]
async fn matching_balances_raise_nothing() {
    let db = db("0xrec_ok").await;
    db.positions()
        .apply_buy(
            "0xrec_ok",
            "tok_a",
            Mode::Live,
            dec!(50),
            dec!(20),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let chain = StubChain(HashMap::from([("tok_a".to_string(), dec!(50))]));
    let alerts = SpyAlerts::default();
    let report = reconcile_once(&db, &chain, &ResolutionStub, &alerts, 900)
        .await
        .unwrap();

    assert_eq!(report.checked, 1);
    assert!(report.divergences.is_empty());
    assert_eq!(alerts.published("alert.reconcile_divergence"), 0);
}

#[tokio::test]
async fn shadow_positions_are_not_reconciled_against_the_chain() {
    let db = db("0xrec_shadow").await;
    db.positions()
        .apply_buy(
            "0xrec_shadow",
            "tok_b",
            Mode::Shadow,
            dec!(50),
            dec!(20),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let chain = StubChain(HashMap::new()); // nothing on chain — and there should not be
    let alerts = SpyAlerts::default();
    let report = reconcile_once(&db, &chain, &ResolutionStub, &alerts, 900)
        .await
        .unwrap();

    assert_eq!(report.checked, 0, "a paper position has no on-chain leg");
    assert!(report.divergences.is_empty());
}

// ---------------------------------------------------------------------------
// Reconciliation and resolution
// ---------------------------------------------------------------------------

struct ResolutionStub;

impl garnet_core::detect::MarketSource for ResolutionStub {
    async fn get(&self, token_id: &str) -> anyhow::Result<garnet_core::market_meta::MarketMeta> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
        // `tok_sol_*` is a resolved pair, `tok_a` is an open market.
        let file = if token_id.starts_with("tok_sol") {
            "clob_resolved_down.json"
        } else {
            "clob_crypto.json"
        };
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path.join(file)).unwrap()).unwrap();
        garnet_core::market_meta::parse_clob_market(&raw, token_id)
    }
}

#[tokio::test]
async fn a_resolved_market_with_no_tokens_left_is_not_a_divergence() {
    // Polymarket's auto-payout redeems a winning position by itself: the tokens are
    // already gone from the proxy while our settlement has not run yet. That is
    // waiting for settlement, not a divergence in the ledger, and it must not be an
    // alert — otherwise the reconciler shouts after every resolution and stops being
    // read.
    let db = db("0xrec_resolved").await;
    db.positions()
        .apply_buy(
            "0xrec_resolved",
            "tok_sol_down",
            Mode::Live,
            dec!(100),
            dec!(40),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let chain = StubChain(HashMap::new()); // zero on chain
    let alerts = SpyAlerts::default();
    let report = reconcile_once(&db, &chain, &ResolutionStub, &alerts, 900)
        .await
        .unwrap();

    assert!(report.divergences.is_empty(), "{:?}", report.divergences);
    assert_eq!(alerts.published("alert.reconcile_divergence"), 0);
    assert_eq!(
        report.awaiting_settlement, 1,
        "the position awaits settlement, and that is visible"
    );
}

#[tokio::test]
async fn an_open_market_that_lost_its_tokens_is_a_divergence() {
    // The same zero on chain, but the market is not resolved: the position vanished
    // to who knows where, and that is exactly what the reconciler exists for.
    let db = db("0xrec_open").await;
    db.positions()
        .apply_buy(
            "0xrec_open",
            "tok_up",
            Mode::Live,
            dec!(100),
            dec!(40),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let alerts = SpyAlerts::default();
    let report = reconcile_once(
        &db,
        &StubChain(HashMap::new()),
        &ResolutionStub,
        &alerts,
        900,
    )
    .await
    .unwrap();

    assert_eq!(report.divergences.len(), 1);
    assert_eq!(alerts.published("alert.reconcile_divergence"), 1);
}

#[tokio::test]
async fn an_unreadable_market_is_not_silently_called_clean() {
    // The metadata is unavailable — so we know nothing about the resolution.
    // Counting such a position as agreed is not allowed: silence from the reconciler
    // would mean "all is well", which we never checked.
    struct Broken;
    impl garnet_core::detect::MarketSource for Broken {
        async fn get(&self, _t: &str) -> anyhow::Result<garnet_core::market_meta::MarketMeta> {
            anyhow::bail!("metadata unavailable")
        }
    }

    let db = db("0xrec_broken").await;
    db.positions()
        .apply_buy(
            "0xrec_broken",
            "tok_a",
            Mode::Live,
            dec!(10),
            dec!(4),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let alerts = SpyAlerts::default();
    let report = reconcile_once(&db, &StubChain(HashMap::new()), &Broken, &alerts, 900)
        .await
        .unwrap();

    assert_eq!(
        report.divergences.len(),
        1,
        "an unknown resolution is not an excuse"
    );
    assert_eq!(alerts.published("alert.reconcile_divergence"), 1);
}

#[tokio::test]
async fn an_unpriced_position_is_recorded_not_just_counted() {
    // `unpriced` was returned to the caller and lost on write: the database kept an
    // understated total with no sign that it was incomplete. Measured 2026-09-04:
    // `/balance` showed a 24-hour delta of +12.55 against +12.28 realised — the
    // difference came from a position that fell out of the snapshot silently.
    let db = db("0xunpriced").await;
    db.positions()
        .apply_buy(
            "0xunpriced",
            "tok_no_price",
            Mode::Live,
            dec!(10),
            dec!(4),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let snap = snapshot_equity(&db, Mode::Live, dec!(100), &StubPrices::flat(dec!(0.5)))
        .await
        .unwrap();
    assert_eq!(snap.unpriced, 1);

    let stored = db
        .equity()
        .latest(Mode::Live)
        .await
        .unwrap()
        .expect("the snapshot was recorded");
    assert_eq!(
        stored.unpriced, 1,
        "the count of unpriced positions must reach whoever reads the snapshot"
    );
    assert_eq!(
        stored.positions_value,
        Decimal::ZERO,
        "no price means no value"
    );
}

#[tokio::test]
async fn shadow_cash_is_derived_from_the_ledger_of_trades() {
    // The virtual account is not a variable inside the process but a consequence of
    // what is already recorded: bought, sold, paid out. Otherwise it lies by exactly
    // whatever happened without passing through it.
    let db = db("0xcash").await;
    let pos = db
        .positions()
        .apply_buy(
            "0xcash",
            "tok_a",
            Mode::Shadow,
            dec!(200),
            dec!(100),
            dec!(2),
        )
        .await
        .unwrap();
    db.positions()
        .apply_sell("0xcash", "tok_a", Mode::Shadow, dec!(60), dec!(30), dec!(1))
        .await
        .unwrap();
    db.settlements()
        .record(pos.id, "tok_a", "Los Angeles Lakers", true, dec!(50), None)
        .await
        .unwrap();

    // 1000 - 100 spent - 3 in fees + 30 in proceeds + 50 in payouts
    assert_eq!(
        db.equity().shadow_cash(dec!(1000)).await.unwrap(),
        dec!(977)
    );
}

#[tokio::test]
async fn a_settlement_payout_reaches_the_shadow_account() {
    // The payout never reached the ledger: it changed only on fills, and the paper
    // account was understated by exactly the sum of every resolution.
    let db = db("0xcash_payout").await;
    let pos = db
        .positions()
        .apply_buy(
            "0xcash_payout",
            "tok_a",
            Mode::Shadow,
            dec!(100),
            dec!(40),
            Decimal::ZERO,
        )
        .await
        .unwrap();
    let before = db.equity().shadow_cash(dec!(1000)).await.unwrap();

    db.settlements()
        .record(pos.id, "tok_a", "Los Angeles Lakers", true, dec!(100), None)
        .await
        .unwrap();

    assert_eq!(
        db.equity().shadow_cash(dec!(1000)).await.unwrap(),
        before + dec!(100)
    );
}

#[tokio::test]
async fn live_positions_do_not_move_the_shadow_account() {
    let db = db("0xcash_modes").await;
    db.positions()
        .apply_buy(
            "0xcash_modes",
            "tok_a",
            Mode::Live,
            dec!(100),
            dec!(40),
            dec!(1),
        )
        .await
        .unwrap();

    assert_eq!(
        db.equity().shadow_cash(dec!(1000)).await.unwrap(),
        dec!(1000)
    );
}

#[tokio::test]
async fn shadow_cash_survives_a_process_restart() {
    // 05.09.2026: the ledger was a `Mutex` inside the process and was taken from the
    // config at startup. A restart gifted the paper account $14,854 and broke the
    // series by which shadow's drawdown is measured.
    let (db, url) = garnet_db::testing::isolated("cash_restart").await.unwrap();
    db.wallets().add("0xcash_restart", None).await.unwrap();
    db.positions()
        .apply_buy(
            "0xcash_restart",
            "tok_a",
            Mode::Shadow,
            dec!(100),
            dec!(250),
            dec!(5),
        )
        .await
        .unwrap();
    let before = db.equity().shadow_cash(dec!(1000)).await.unwrap();
    assert_eq!(before, dec!(745));

    // A new connection to the same data is the same as a new process.
    let restarted = Db::connect(&url).await.unwrap();
    assert_eq!(
        restarted.equity().shadow_cash(dec!(1000)).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn shadow_cash_goes_negative_and_stays_a_number() {
    // The ledger is allowed to go negative, and that is not an error. Twelve wallets
    // at $25 eat the starting thousand in about 40 trades.
    let db = db("0xcash_negative").await;
    for i in 0..50 {
        db.positions()
            .apply_buy(
                "0xcash_negative",
                &format!("tok_{i}"),
                Mode::Shadow,
                dec!(50),
                dec!(25),
                Decimal::ZERO,
            )
            .await
            .unwrap();
    }

    assert_eq!(
        db.equity().shadow_cash(dec!(1000)).await.unwrap(),
        dec!(-250)
    );
}
