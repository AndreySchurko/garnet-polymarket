//! The end-to-end path on stubs: an RTDS frame reaches a position and leaves a trace at every
//! step.

use garnet_bin::app::{App, BookSource};
use garnet_config::Config;
use garnet_core::book::Book;
use garnet_core::detect::{Detector, MarketSource};
use garnet_core::execute::{ClobExec, ExecError, OrderRequest};
use garnet_core::market_meta::{parse_clob_market, MarketMeta};
use garnet_core::shadow::{Fill, FillSource};
use garnet_db::{Db, Mode};
use garnet_risk::killswitch::TripReason;
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
        let f = match token_id {
            "tok_lal" | "tok_bos" => "clob_sports.json",
            "tok_up" | "tok_down" => "clob_crypto.json",
            // A resolved market: an exit on it is no longer an exit.
            "tok_eth_up" => "clob_resolved.json",
            other => anyhow::bail!("no fixture for {other}"),
        };
        parse_clob_market(&fixture(f), token_id)
    }
}

struct StubBooks;

impl BookSource for StubBooks {
    async fn book(&self, _token_id: &str) -> anyhow::Result<Book> {
        Book::from_clob(&fixture("book_thin.json"))
    }
}

/// A book given by a fixture: tests about price have to be able to show an empty book and an
/// expensive one, otherwise they only check the happy path.
struct FixtureBooks(&'static str);

impl BookSource for FixtureBooks {
    async fn book(&self, _token_id: &str) -> anyhow::Result<Book> {
        Book::from_clob(&fixture(self.0))
    }
}

struct StubClob;

impl ClobExec for StubClob {
    async fn place_ioc(&self, req: &OrderRequest) -> Result<Fill, ExecError> {
        Ok(Fill {
            size: req.size_shares,
            avg_price: req.limit_price,
            notional: req.size_shares * req.limit_price,
            fee_usd: dec!(0.1),
            source: FillSource::BookWalk,
        })
    }
}

fn config() -> Config {
    Config::from_toml(
        r#"
database_url = "unused"
[shadow]
initial_capital_usd = 1000.0
"#,
    )
    .unwrap()
}

async fn app_with(
    wallet: &str,
    mode: Mode,
    stake: Decimal,
) -> (App<StubMarkets, StubBooks, StubClob>, Db) {
    let db = garnet_db::testing::isolated_db("wiring").await.unwrap();
    db.wallets().add(wallet, Some("whale-1")).await.unwrap();
    db.wallets().set_stake(wallet, stake).await.unwrap();
    db.wallets().set_enabled(wallet, true).await.unwrap();
    if mode == Mode::Live {
        db.wallets().set_mode(wallet, Mode::Live).await.unwrap();
    }

    let detector = Detector::new(db.clone(), StubMarkets, [wallet.to_string()]);
    let app = App::new(
        db.clone(),
        detector,
        StubBooks,
        StubClob,
        &config(),
        garnet_bin::events::Events::off(),
    );
    (app, db)
}

async fn app_with_books(
    wallet: &str,
    book: &'static str,
) -> (App<StubMarkets, FixtureBooks, StubClob>, Db) {
    let db = garnet_db::testing::isolated_db("wiring").await.unwrap();
    db.wallets().add(wallet, Some("whale-1")).await.unwrap();
    db.wallets().set_stake(wallet, dec!(25)).await.unwrap();
    db.wallets().set_enabled(wallet, true).await.unwrap();

    let detector = Detector::new(db.clone(), StubMarkets, [wallet.to_string()]);
    let app = App::new(
        db.clone(),
        detector,
        FixtureBooks(book),
        StubClob,
        &config(),
        garnet_bin::events::Events::off(),
    );
    (app, db)
}

/// A frame about a trade that happened **just now**.
///
/// The fixture's timestamp is fixed (2026-09-03), while only what the wallet did since it was
/// assigned is copied. The test about that very watershed sets the timestamp itself, via
/// `frame_at`.
fn frame_for(wallet: &str) -> serde_json::Value {
    frame_at(wallet, chrono::Utc::now().timestamp() + 5)
}

fn frame_at(wallet: &str, ts: i64) -> serde_json::Value {
    let mut f = fixture("rtds_normal.json");
    f["payload"]["proxyWallet"] = serde_json::json!(wallet);
    f["payload"]["timestamp"] = serde_json::json!(ts);
    f
}

#[tokio::test]
async fn frame_to_position_end_to_end_in_shadow() {
    let (app, db) = app_with("0xwire1", Mode::Shadow, dec!(25)).await;

    app.on_frame(&frame_for("0xwire1")).await.unwrap();

    let pos = db
        .positions()
        .get("0xwire1", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .expect("the position was created");
    assert!(pos.size_bought > Decimal::ZERO);
    assert_eq!(
        pos.leader_observed_size,
        dec!(120),
        "we remembered the leader's holding"
    );

    let signals = db.signals().recent(5).await.unwrap();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].verdict, "copy");
    assert_eq!(signals[0].mode, Mode::Shadow);

    // The virtual ledger debited the cost together with the fee.
    assert!(app.shadow_cash().await.unwrap() < dec!(1000));
    assert_eq!(
        app.metrics.counter("signals_total", &[("verdict", "copy")]),
        1
    );
    assert_eq!(
        app.metrics.count_of("trade_to_seen"),
        1,
        "the latency is measured"
    );
}

#[tokio::test]
async fn live_wallet_goes_through_the_clob() {
    let (app, db) = app_with("0xwire2", Mode::Live, dec!(25)).await;
    app.set_live_cash(dec!(500));

    app.on_frame(&frame_for("0xwire2")).await.unwrap();

    let pos = db
        .positions()
        .get("0xwire2", "tok_lal", Mode::Live)
        .await
        .unwrap()
        .expect("the live position was created");
    assert!(pos.size_bought > Decimal::ZERO);
    assert_eq!(
        app.metrics
            .counter("fills_total", &[("mode", "live"), ("source", "clob")]),
        1,
        "the fill came from the exchange"
    );
    assert_eq!(
        app.shadow_cash().await.unwrap(),
        dec!(1000),
        "the virtual ledger is untouched"
    );
}

#[tokio::test]
async fn skipped_signal_still_leaves_a_row_with_its_reason() {
    let (app, db) = app_with("0xwire3", Mode::Live, dec!(25)).await;
    app.set_live_cash(dec!(1)); // there is no money

    app.on_frame(&frame_for("0xwire3")).await.unwrap();

    let signals = db.signals().recent(5).await.unwrap();
    assert_eq!(signals[0].verdict, "skip:insufficient_balance");
    assert!(
        db.positions()
            .get("0xwire3", "tok_lal", Mode::Live)
            .await
            .unwrap()
            .unwrap()
            .size_bought
            == Decimal::ZERO,
        "no position was taken on"
    );
}

#[tokio::test]
async fn killswitch_blocks_the_order_but_not_the_signal() {
    let (app, db) = app_with("0xwire4", Mode::Live, dec!(25)).await;
    app.set_live_cash(dec!(500));
    app.trip(TripReason::FeedStalled);

    app.on_frame(&frame_for("0xwire4")).await.unwrap();

    let signals = db.signals().recent(5).await.unwrap();
    assert_eq!(
        signals[0].verdict, "copy",
        "the killswitch is not a reason to refuse a signal"
    );

    let orders: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT status, error FROM orders ORDER BY id DESC LIMIT 1")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(orders[0].0, "rejected");
    assert!(orders[0].1.as_ref().unwrap().contains("killswitch"));

    let pos = db
        .positions()
        .get("0xwire4", "tok_lal", Mode::Live)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pos.size_bought, Decimal::ZERO, "live execution is halted");
}

#[tokio::test]
async fn shadow_keeps_trading_through_a_killswitch() {
    let (app, db) = app_with("0xwire5", Mode::Shadow, dec!(25)).await;
    app.trip(TripReason::FeedStalled);

    app.on_frame(&frame_for("0xwire5")).await.unwrap();

    let pos = db
        .positions()
        .get("0xwire5", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert!(
        pos.size_bought > Decimal::ZERO,
        "the measuring instrument does not stop"
    );
}

#[tokio::test]
async fn a_leader_sale_closes_part_of_our_position() {
    let (app, db) = app_with("0xwire6", Mode::Shadow, dec!(25)).await;
    // The best bid in the fixture is 0.28 while the floor at the 0.15 default is 0.36: with it
    // the exit honestly does not fill (see `a_shadow_exit_held_when_bids_are_low`).
    db.wallets()
        .set_slippage("0xwire6", dec!(0.40))
        .await
        .unwrap();

    app.on_frame(&frame_for("0xwire6")).await.unwrap();
    let before = db
        .positions()
        .get("0xwire6", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();

    // The leader sells 60 of the 120 observed — half.
    let mut sale = frame_for("0xwire6");
    sale["payload"]["side"] = serde_json::json!("SELL");
    sale["payload"]["size"] = serde_json::json!(60);
    sale["payload"]["transactionHash"] = serde_json::json!("0xsell1");
    app.on_frame(&sale).await.unwrap();

    let after = db
        .positions()
        .get("0xwire6", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.size_sold,
        before.size_bought / dec!(2),
        "we sold exactly half of our position"
    );
    assert!(after.proceeds_usd > Decimal::ZERO);
}

#[tokio::test]
async fn a_duplicate_frame_does_not_double_the_stake() {
    let (app, db) = app_with("0xwire7", Mode::Shadow, dec!(25)).await;

    app.on_frame(&frame_for("0xwire7")).await.unwrap();
    app.on_frame(&frame_for("0xwire7")).await.unwrap(); // the same on-chain trade

    let pos = db
        .positions()
        .get("0xwire7", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    let signals = db.signals().recent(10).await.unwrap();
    assert_eq!(
        signals.len(),
        1,
        "a duplicate gives birth to no second signal"
    );
    assert_eq!(
        pos.leader_observed_size,
        dec!(120),
        "and does not double the observed holding"
    );
}

/// An executor that counts calls and always refuses: needed in order to prove that an order
/// **was not sent**.
struct CountingClob(std::sync::atomic::AtomicUsize);

impl ClobExec for CountingClob {
    async fn place_ioc(&self, _req: &OrderRequest) -> Result<Fill, ExecError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(ExecError::Rejected(
            "this should not have been called".into(),
        ))
    }
}

#[tokio::test]
async fn a_stake_under_a_dollar_is_lifted_to_the_exchange_minimum() {
    // The market's `minimum_order_size` equals 5 shares, but the exchange's minimum is a $1
    // notional: "invalid amount for a marketable BUY order ($0.99975), min size: 1". A $1
    // stake at 0.48 is two shares, and such an order is legitimate; refusing on the
    // share-count minimum would deprive us of every expensive side.
    let db = garnet_db::testing::isolated_db("wiring").await.unwrap();
    db.wallets().add("0xwire9", Some("small")).await.unwrap();
    db.wallets().set_stake("0xwire9", dec!(1)).await.unwrap();
    db.wallets().set_enabled("0xwire9", true).await.unwrap();
    db.wallets().set_mode("0xwire9", Mode::Live).await.unwrap();

    let clob = CountingClob(std::sync::atomic::AtomicUsize::new(0));
    let detector = Detector::new(db.clone(), StubMarkets, ["0xwire9".to_string()]);
    let app = App::new(
        db.clone(),
        detector,
        StubBooks,
        clob,
        &config(),
        garnet_bin::events::Events::off(),
    );
    app.set_live_cash(dec!(500));

    app.on_frame(&frame_for("0xwire9")).await.unwrap();

    let order = db
        .signals()
        .last_order()
        .await
        .unwrap()
        .expect("the order row");
    assert_eq!(
        order.status, "rejected",
        "the stub refuses, but the order went out"
    );
    assert!(
        order
            .error
            .unwrap_or_default()
            .contains("this should not have been called"),
        "the refusal came from the exchange, not from our minimum check"
    );
}

#[tokio::test]
async fn the_limit_price_is_snapped_to_the_market_tick() {
    // 0.42 * 1.15 = 0.483 at a tick of 0.01 -> 0.48. An off-grid price is rejected locally by
    // the SDK's builder, and the order never reaches the exchange at all.
    let (app, db) = app_with("0xwire10", Mode::Live, dec!(25)).await;
    app.set_live_cash(dec!(500));

    app.on_frame(&frame_for("0xwire10")).await.unwrap();

    let order = db
        .signals()
        .last_order()
        .await
        .unwrap()
        .expect("the order row");
    assert_eq!(order.limit_price, dec!(0.48));
}

#[tokio::test]
async fn a_cleared_killswitch_lets_live_trade_again() {
    // The health loop clears its own stop when the feed returns. If clearing it does not bring
    // trading back, the bot stays dead until a restart — and it cannot restart itself.

    let (app, db) = app_with("0xwire11", Mode::Live, dec!(25)).await;
    app.set_live_cash(dec!(500));
    app.trip(TripReason::FeedStalled);
    assert_eq!(app.killswitch_reason(), Some(TripReason::FeedStalled));

    app.clear_trip();
    assert_eq!(app.killswitch_reason(), None);

    app.on_frame(&frame_for("0xwire11")).await.unwrap();

    let order = db
        .signals()
        .last_order()
        .await
        .unwrap()
        .expect("the order row");
    assert_eq!(
        order.status, "filled",
        "once the stop is cleared the order goes out"
    );
}

#[tokio::test]
async fn a_polled_row_is_copied_exactly_like_a_socket_frame() {
    // The safety-net circuit: when the socket's topic goes down platform-wide, the trades keep
    // arriving over REST. The copy has to come out the same — otherwise the bot trades
    // differently in an incident than normally, and there will be nowhere to check it.
    let (app, db) = app_with("0xwire12", Mode::Shadow, dec!(25)).await;

    let rows = serde_json::json!([{
        "proxyWallet": "0xWire12",
        "timestamp": chrono::Utc::now().timestamp(),
        "type": "TRADE",
        "size": 120,
        "transactionHash": "0xpoll1",
        "price": 0.42,
        "asset": "tok_lal",
        "side": "BUY"
    }]);
    app.on_activity(&rows).await.unwrap();

    let pos = db
        .positions()
        .get("0xwire12", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .expect("the position was taken on via the poll");
    assert!(pos.size_bought > Decimal::ZERO);
}

#[tokio::test]
async fn the_same_trade_from_both_paths_is_copied_once() {
    // Both circuits write to one table, and the dedup key is not a log index, which does not
    // exist, but `(tx_hash, wallet, token_id, side)`.
    let (app, db) = app_with("0xwire13", Mode::Shadow, dec!(25)).await;

    let mut frame = frame_for("0xwire13");
    frame["payload"]["transactionHash"] = serde_json::json!("0xsame");
    app.on_frame(&frame).await.unwrap();

    let rows = serde_json::json!([{
        "proxyWallet": "0xwire13",
        "timestamp": 1788447208,
        "type": "TRADE",
        "size": 120,
        "transactionHash": "0xsame",
        "price": 0.42,
        "asset": "tok_lal",
        "side": "BUY"
    }]);
    app.on_activity(&rows).await.unwrap();

    let signals = db.signals().recent(10).await.unwrap();
    assert_eq!(
        signals.len(),
        1,
        "a second copy of the same trade is not traded"
    );
}

/// Assembles the application with an executor of its own: needed in order to prove that paper
/// mode never reaches the exchange.
async fn app_with_clob<C: ClobExec>(
    wallet: &str,
    mode: Mode,
    clob: C,
) -> (App<StubMarkets, StubBooks, C>, Db) {
    let db = garnet_db::testing::isolated_db("wiring").await.unwrap();
    db.wallets().add(wallet, None).await.unwrap();
    db.wallets().set_stake(wallet, dec!(25)).await.unwrap();
    db.wallets().set_enabled(wallet, true).await.unwrap();
    db.wallets().set_mode(wallet, mode).await.unwrap();
    // The sell floor at 0.15 slippage is 0.36 while the best bid in the fixture is 0.28: with
    // the default any exit would hit the book and check nothing.
    db.wallets().set_slippage(wallet, dec!(0.40)).await.unwrap();

    let detector = Detector::new(db.clone(), StubMarkets, [wallet.to_string()]);
    let app = App::new(
        db.clone(),
        detector,
        StubBooks,
        clob,
        &config(),
        garnet_bin::events::Events::off(),
    );
    (app, db)
}

fn sale_of(wallet: &str, size: u32, tx: &str) -> serde_json::Value {
    let mut f = frame_for(wallet);
    f["payload"]["side"] = serde_json::json!("SELL");
    f["payload"]["size"] = serde_json::json!(size);
    f["payload"]["transactionHash"] = serde_json::json!(tx);
    f
}

#[tokio::test]
async fn a_shadow_exit_never_reaches_the_exchange() {
    // Found by a live run on 05.09.2026: the exit path did not branch on mode, and twelve
    // paper wallets sent 81 signed sell orders to the exchange. They were refused only
    // because the account did not hold those tokens.
    let clob = CountingClob(std::sync::atomic::AtomicUsize::new(0));
    let (app, db) = app_with_clob("0xwire13", Mode::Shadow, clob).await;

    app.on_frame(&frame_for("0xwire13")).await.unwrap();
    app.on_frame(&sale_of("0xwire13", 60, "0xsell13"))
        .await
        .unwrap();

    assert_eq!(
        app.clob().0.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "paper mode contacted the exchange"
    );

    let pos = db
        .positions()
        .get("0xwire13", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert!(
        pos.size_sold > Decimal::ZERO,
        "the exit must fill against the book"
    );
    assert!(pos.proceeds_usd > Decimal::ZERO);
}

#[tokio::test]
async fn a_shadow_exit_is_filled_from_the_bid_side_of_the_book() {
    // The exit price has to come from the bids rather than from our own limit: an exchange
    // stub returning the limit is exactly what hid the defect above.
    let clob = CountingClob(std::sync::atomic::AtomicUsize::new(0));
    let (app, db) = app_with_clob("0xwire14", Mode::Shadow, clob).await;

    app.on_frame(&frame_for("0xwire14")).await.unwrap();
    app.on_frame(&sale_of("0xwire14", 60, "0xsell14"))
        .await
        .unwrap();

    let order = db
        .signals()
        .last_order()
        .await
        .unwrap()
        .expect("the order row");
    assert_eq!(order.side, "sell");
    assert_eq!(order.status, "filled");
    assert_eq!(
        order.limit_price,
        dec!(0.28),
        "the price comes from the best bid, not from the 0.26 limit"
    );
}

/// Fills and counts calls: `CountingClob` always refuses, and a live buy through it would
/// never be taken on — there would be nothing to sell.
struct FillingCountingClob(std::sync::atomic::AtomicUsize);

impl ClobExec for FillingCountingClob {
    async fn place_ioc(&self, req: &OrderRequest) -> Result<Fill, ExecError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Fill {
            size: req.size_shares,
            avg_price: req.limit_price,
            notional: req.size_shares * req.limit_price,
            fee_usd: Decimal::ZERO,
            source: FillSource::Clob,
        })
    }
}

#[tokio::test]
async fn a_killswitch_blocks_a_live_exit() {
    // The killswitch stops live. A sale is just as much a live order as a buy, and until
    // 05.09.2026 the stop did not touch it at all.
    let clob = FillingCountingClob(std::sync::atomic::AtomicUsize::new(0));
    let (app, db) = app_with_clob("0xwire15", Mode::Live, clob).await;
    app.set_live_cash(dec!(500));

    app.on_frame(&frame_for("0xwire15")).await.unwrap();
    let calls_after_buy = app.clob().0.load(std::sync::atomic::Ordering::Relaxed);
    assert!(calls_after_buy > 0, "a live buy must reach the exchange");

    app.trip(TripReason::FeedStalled);
    app.on_frame(&sale_of("0xwire15", 60, "0xsell15"))
        .await
        .unwrap();

    assert_eq!(
        app.clob().0.load(std::sync::atomic::Ordering::Relaxed),
        calls_after_buy,
        "the stop did not halt a live sale"
    );
    let order = db
        .signals()
        .last_order()
        .await
        .unwrap()
        .expect("the order row");
    assert_eq!(order.side, "sell");
    assert_eq!(order.status, "rejected");
    assert!(order.error.unwrap_or_default().contains("killswitch"));
}

#[tokio::test]
async fn a_live_exit_without_a_killswitch_still_reaches_the_exchange() {
    let clob = FillingCountingClob(std::sync::atomic::AtomicUsize::new(0));
    let (app, db) = app_with_clob("0xwire17", Mode::Live, clob).await;
    app.set_live_cash(dec!(500));

    app.on_frame(&frame_for("0xwire17")).await.unwrap();
    let calls_after_buy = app.clob().0.load(std::sync::atomic::Ordering::Relaxed);
    app.on_frame(&sale_of("0xwire17", 60, "0xsell17"))
        .await
        .unwrap();

    assert!(
        app.clob().0.load(std::sync::atomic::Ordering::Relaxed) > calls_after_buy,
        "a live exit must go through the exchange rather than walk the book"
    );
    let pos = db
        .positions()
        .get("0xwire17", "tok_lal", Mode::Live)
        .await
        .unwrap()
        .unwrap();
    assert!(pos.size_sold > Decimal::ZERO);
}

#[tokio::test]
async fn a_shadow_exit_ignores_the_killswitch() {
    // Invariant 12: the killswitch stops live only. The measuring instrument does not stop.
    let clob = CountingClob(std::sync::atomic::AtomicUsize::new(0));
    let (app, db) = app_with_clob("0xwire16", Mode::Shadow, clob).await;

    app.on_frame(&frame_for("0xwire16")).await.unwrap();
    app.trip(TripReason::FeedStalled);
    app.on_frame(&sale_of("0xwire16", 60, "0xsell16"))
        .await
        .unwrap();

    let pos = db
        .positions()
        .get("0xwire16", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert!(
        pos.size_sold > Decimal::ZERO,
        "the paper exit was halted by the stop"
    );
}

#[tokio::test]
async fn a_shadow_exit_is_held_when_the_bids_are_below_the_floor() {
    // The floor at 0.15 slippage is 0.36, the best bid is 0.28. This used to look like a
    // successful sale: the exchange stub filled at our own limit without looking at the book,
    // and the paper exit was a fiction.
    let clob = CountingClob(std::sync::atomic::AtomicUsize::new(0));
    let (app, db) = app_with_clob("0xwire18", Mode::Shadow, clob).await;
    db.wallets()
        .set_slippage("0xwire18", dec!(0.15))
        .await
        .unwrap();

    app.on_frame(&frame_for("0xwire18")).await.unwrap();
    app.on_frame(&sale_of("0xwire18", 60, "0xsell18"))
        .await
        .unwrap();

    let pos = db
        .positions()
        .get("0xwire18", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        pos.size_sold,
        Decimal::ZERO,
        "we do not sell below the floor"
    );
    let order = db
        .signals()
        .last_order()
        .await
        .unwrap()
        .expect("the order row");
    assert_eq!(order.status, "rejected");
    assert_eq!(app.clob().0.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[tokio::test]
async fn a_killswitch_leaves_no_row_when_there_is_nothing_to_sell() {
    // The stop must not invent an event: without it a sale of an empty position leaves no
    // order row, and with the stop it is not needed either. Otherwise the log of refusals
    // grows on wallets that hold nothing.
    let clob = CountingClob(std::sync::atomic::AtomicUsize::new(0)); // the buy is refused
    let (app, db) = app_with_clob("0xwire19", Mode::Live, clob).await;
    app.set_live_cash(dec!(500));

    app.on_frame(&frame_for("0xwire19")).await.unwrap();
    app.trip(TripReason::FeedStalled);
    app.on_frame(&sale_of("0xwire19", 60, "0xsell19"))
        .await
        .unwrap();

    let pos = db
        .positions()
        .get("0xwire19", "tok_lal", Mode::Live)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pos.size_bought, Decimal::ZERO, "the buy was not taken on");

    let order = db
        .signals()
        .last_order()
        .await
        .unwrap()
        .expect("the order row");
    assert_eq!(
        order.side, "buy",
        "there was nothing to sell — there is no sell row"
    );
}

#[tokio::test]
async fn a_restart_does_not_refill_the_shadow_account() {
    // The same defect as with the operator's stop: the state lived inside the process. A
    // restart returned the paper account to its starting capital, and the series by which
    // shadow's drawdown is measured broke on every deployment.
    let (db, url) = garnet_db::testing::isolated("wiring_restart")
        .await
        .unwrap();
    db.wallets().add("0xwire20", None).await.unwrap();
    db.wallets().set_stake("0xwire20", dec!(25)).await.unwrap();
    db.wallets().set_enabled("0xwire20", true).await.unwrap();

    let detector = Detector::new(db.clone(), StubMarkets, ["0xwire20".to_string()]);
    let app = App::new(
        db.clone(),
        detector,
        StubBooks,
        StubClob,
        &config(),
        garnet_bin::events::Events::off(),
    );
    app.on_frame(&frame_for("0xwire20")).await.unwrap();

    let spent = app.shadow_cash().await.unwrap();
    assert!(spent < dec!(1000), "the buy debited the paper account");

    // A new process on the same data.
    let db2 = Db::connect(&url).await.unwrap();
    let detector2 = Detector::new(db2.clone(), StubMarkets, ["0xwire20".to_string()]);
    let restarted = App::new(
        db2,
        detector2,
        StubBooks,
        StubClob,
        &config(),
        garnet_bin::events::Events::off(),
    );

    assert_eq!(
        restarted.shadow_cash().await.unwrap(),
        spent,
        "the account reverted to its starting value"
    );
}

/// The moment the wallet was assigned, to the second.
async fn watched_since(db: &Db, wallet: &str) -> i64 {
    let (ts,): (chrono::DateTime<chrono::Utc>,) =
        sqlx::query_as("SELECT created_at FROM wallets WHERE address = $1")
            .bind(wallet)
            .fetch_one(db.pool())
            .await
            .unwrap();
    ts.timestamp()
}

#[tokio::test]
async fn a_trade_made_before_the_wallet_was_added_is_not_copied() {
    // `/activity?user=...&limit=20` on a quiet wallet returns weeks of history, and on the
    // first tick all of it looks like news. On 05.09.2026 that is how 69
    // `market_not_tradable` refusals and 28 copies of trades up to 3.4 days old were born —
    // one of them filled at 0.001 against the leader's 0.260.
    let (app, db) = app_with("0xwire21", Mode::Shadow, dec!(25)).await;

    let day_before = watched_since(&db, "0xwire21").await - 86_400;
    app.on_frame(&frame_at("0xwire21", day_before))
        .await
        .unwrap();

    assert!(
        db.signals().recent(5).await.unwrap().is_empty(),
        "history is not a signal"
    );
    assert!(
        db.positions()
            .get("0xwire21", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .is_none(),
        "and not a position"
    );
}

#[tokio::test]
async fn a_trade_made_before_the_wallet_was_added_is_still_recorded() {
    // We skip the signal but not the sighting: the trade has to stay in the log, otherwise the
    // dedup would forget it and the next poll tick would copy the same history.
    let (app, db) = app_with("0xwire22", Mode::Shadow, dec!(25)).await;

    let day_before = watched_since(&db, "0xwire22").await - 86_400;
    app.on_frame(&frame_at("0xwire22", day_before))
        .await
        .unwrap();

    let seen: Vec<(String,)> =
        sqlx::query_as("SELECT tx_hash FROM leader_trades WHERE wallet = $1")
            .bind("0xwire22")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(seen.len(), 1, "the leader trade was recorded");
}

#[tokio::test]
async fn a_trade_made_after_the_wallet_was_added_is_copied() {
    let (app, db) = app_with("0xwire23", Mode::Shadow, dec!(25)).await;

    app.on_frame(&frame_for("0xwire23")).await.unwrap();

    let signals = db.signals().recent(5).await.unwrap();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].verdict, "copy");
}

#[tokio::test]
async fn a_trade_in_the_same_second_as_the_assignment_is_copied() {
    // RTDS timestamps a trade to the second, `created_at` has microseconds. Comparing
    // different resolutions would discard the first second of observation, and that is
    // precisely the moment a wallet is added for.
    let (app, db) = app_with("0xwire24", Mode::Shadow, dec!(25)).await;
    let second = watched_since(&db, "0xwire24").await;

    app.on_frame(&frame_at("0xwire24", second)).await.unwrap();

    let signals = db.signals().recent(5).await.unwrap();
    assert_eq!(
        signals.len(),
        1,
        "a trade in the same second is already under observation"
    );
    assert_eq!(signals[0].verdict, "copy");
}

#[tokio::test]
async fn a_trade_one_second_before_the_assignment_is_not_copied() {
    let (app, db) = app_with("0xwire25", Mode::Shadow, dec!(25)).await;
    let second = watched_since(&db, "0xwire25").await;

    app.on_frame(&frame_at("0xwire25", second - 1))
        .await
        .unwrap();

    assert!(db.signals().recent(5).await.unwrap().is_empty());
}

/// The price we saw has to sit in `signals` on **every** verdict.
///
/// A slippage threshold can only be tuned from an uncensored tail: if `best_ask` is written
/// only where a trade happened, the sample keeps just the one edge the ceiling itself
/// refused.
#[tokio::test]
async fn a_copy_records_the_price_it_saw() {
    let (app, db) = app_with_books("0xask1", "book_thin.json").await;
    app.on_frame(&frame_for("0xask1")).await.unwrap();

    let s = &db.signals().recent(1).await.unwrap()[0];
    assert_eq!(s.verdict, "copy");
    assert_eq!(
        s.best_ask,
        Some(dec!(0.30)),
        "the best ask, not the first in the array"
    );
    assert_eq!(s.best_bid, Some(dec!(0.28)));
    assert!(s.limit_price.is_some());
}

/// A slippage refusal without a price is a refusal about which nothing can be said.
#[tokio::test]
async fn a_slippage_skip_records_both_the_price_and_the_cap() {
    let (app, db) = app_with_books("0xask2", "book_pricey.json").await;
    app.on_frame(&frame_for("0xask2")).await.unwrap();

    let s = &db.signals().recent(1).await.unwrap()[0];
    assert_eq!(s.verdict, "skip:slippage_exceeded");
    assert_eq!(s.best_ask, Some(dec!(0.60)));
    // The leader entered at 0.42, slippage 15% — a ceiling of 0.483. The miss is computed by
    // subtraction, and both of its halves are now in the row.
    assert_eq!(s.limit_price, Some(dec!(0.483)), "the ceiling that fired");
}

/// "There is no book" and "expensive" are different outcomes, and an empty price separates
/// them.
#[tokio::test]
async fn an_empty_book_is_not_recorded_as_slippage() {
    let (app, db) = app_with_books("0xask3", "book_no_asks.json").await;
    app.on_frame(&frame_for("0xask3")).await.unwrap();

    let s = &db.signals().recent(1).await.unwrap()[0];
    assert_eq!(
        s.verdict, "skip:market_not_tradable",
        "an empty book used to arrive in the slippage statistics via a 1.0 substitution"
    );
    assert_eq!(
        s.best_ask, None,
        "there was no price — inventing one is not allowed"
    );
    assert_eq!(
        s.best_bid,
        Some(dec!(0.28)),
        "one side of the book is not yet an empty book"
    );
}

/// A second slice of the same leader order does not become a copy.
///
/// A taker order consumes as much of the book as is standing there and arrives as that many
/// frames with different `tx_hash` values — dedup by hash does not catch them and cannot.
/// Measured 06.09.2026: 43% of our buys were such slices.
#[tokio::test]
async fn a_second_slice_of_the_same_order_is_not_copied() {
    let (app, db) = app_with("0xslice1", Mode::Shadow, dec!(25)).await;
    let base = chrono::Utc::now().timestamp() + 5;

    let mut f1 = frame_at("0xslice1", base);
    f1["payload"]["transactionHash"] = serde_json::json!("0xs1");
    app.on_frame(&f1).await.unwrap();

    let mut f2 = frame_at("0xslice1", base + 10);
    f2["payload"]["transactionHash"] = serde_json::json!("0xs2");
    app.on_frame(&f2).await.unwrap();

    let sigs = db.signals().recent(10).await.unwrap();
    assert_eq!(
        sigs.len(),
        2,
        "a trade is always recorded, otherwise the dedup would forget it"
    );
    assert_eq!(
        sigs[0].verdict, "skip:duplicate",
        "the second slice is not a decision by the leader"
    );
    assert_eq!(sigs[1].verdict, "copy");

    let pos = db
        .positions()
        .get("0xslice1", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .expect("there is one position");
    assert_eq!(
        pos.cost_usd.round_dp(0),
        dec!(25),
        "there is one stake in the market, not two"
    );
}

/// A leader returning to a market after a pause is a new decision.
#[tokio::test]
async fn a_return_after_the_window_is_a_new_decision() {
    let (app, db) = app_with("0xslice2", Mode::Shadow, dec!(25)).await;
    let base = chrono::Utc::now().timestamp() + 5;

    let mut f1 = frame_at("0xslice2", base);
    f1["payload"]["transactionHash"] = serde_json::json!("0xt1");
    app.on_frame(&f1).await.unwrap();

    // The default slice window is 300 s.
    let mut f2 = frame_at("0xslice2", base + 400);
    f2["payload"]["transactionHash"] = serde_json::json!("0xt2");
    app.on_frame(&f2).await.unwrap();

    let sigs = db.signals().recent(10).await.unwrap();
    assert_eq!(
        sigs[0].verdict, "copy",
        "a pause longer than the window means the leader decided afresh"
    );
    assert_eq!(sigs[1].verdict, "copy");
}

/// The slice window lives on the buy side and only there.
///
/// On a sale each slice takes out its own fraction of our position, and in total the exit is
/// correct; collapsing them would leave us short by exactly the discarded slices.
#[tokio::test]
async fn selling_is_not_merged() {
    let (app, db) = app_with("0xslice3", Mode::Shadow, dec!(25)).await;
    let base = chrono::Utc::now().timestamp() + 5;

    let mut buy = frame_at("0xslice3", base);
    buy["payload"]["transactionHash"] = serde_json::json!("0xu1");
    app.on_frame(&buy).await.unwrap();

    for (i, tx) in ["0xu2", "0xu3"].iter().enumerate() {
        let mut sell = frame_at("0xslice3", base + 10 + i as i64);
        sell["payload"]["side"] = serde_json::json!("SELL");
        sell["payload"]["size"] = serde_json::json!(30);
        sell["payload"]["transactionHash"] = serde_json::json!(tx);
        app.on_frame(&sell).await.unwrap();
    }

    let sigs = db.signals().recent(10).await.unwrap();
    let dups = sigs
        .iter()
        .filter(|s| s.verdict == "skip:duplicate")
        .count();
    assert_eq!(
        dups, 0,
        "a sale is not collapsed: selling short is worse than asking twice"
    );
}

/// A time-based exit is our decision, and the order from it does not reference the leader.
///
/// `orders.signal_id` used to be mandatory, because every order was born from somebody else's
/// trade. Inventing a signal for the sake of the reference would mean recording, in the
/// registry of the leader's decisions, something they never did.
#[tokio::test]
async fn a_time_exit_has_no_leader_behind_it() {
    let (app, db) = app_with("0xstale1", Mode::Shadow, dec!(25)).await;
    app.on_frame(&frame_for("0xstale1")).await.unwrap();

    let pos = db
        .positions()
        .get("0xstale1", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .expect("the position exists");

    let sold = app.close_stale(&pos).await.unwrap();
    assert!(
        sold,
        "the fixture's book provides bids — the exit must go through"
    );

    let exit = db
        .signals()
        .last_order()
        .await
        .unwrap()
        .expect("the order was recorded");
    assert_eq!(exit.side, "sell");
    assert_eq!(
        exit.signal_id, None,
        "there is no leader decision behind our exit"
    );
    assert_eq!(exit.status, "filled");
}

/// The killswitch stops our own exit too: a sale is a live order whoever decided it
/// (invariants 12 and 23).
#[tokio::test]
async fn a_time_exit_obeys_the_killswitch() {
    let (app, db) = app_with("0xstale2", Mode::Live, dec!(25)).await;
    app.on_frame(&frame_for("0xstale2")).await.unwrap();
    let pos = db
        .positions()
        .get("0xstale2", "tok_lal", Mode::Live)
        .await
        .unwrap()
        .expect("the position exists");

    app.trip(TripReason::Manual);
    assert!(
        !app.close_stale(&pos).await.unwrap(),
        "the stop halts the time-based exit too"
    );
}

/// Small exit fractions accumulate until they reach the exchange minimum.
///
/// The leader trims a position by percentages, our share comes out in cents, and every such
/// sale was refused by the $1 minimum. A refusal on each means we copy the leader's entry and
/// do not copy their exit.
#[tokio::test]
async fn small_exit_fractions_accumulate_until_they_clear_the_minimum() {
    let (app, db) = app_with("0xcarry1", Mode::Shadow, dec!(25)).await;
    let base = chrono::Utc::now().timestamp() + 5;

    let mut buy = frame_at("0xcarry1", base);
    buy["payload"]["transactionHash"] = serde_json::json!("0xc1");
    app.on_frame(&buy).await.unwrap();

    let pos = db
        .positions()
        .get("0xcarry1", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        pos.pending_exit_shares,
        Decimal::ZERO,
        "there are no debts yet"
    );

    // The leader sells one percent of what is observed at a time: our share is in cents.
    // Each such sale arrives as its own trade, the dedup will not collapse them.
    let mut deferred = Decimal::ZERO;
    for (i, tx) in ["0xc2", "0xc3"].iter().enumerate() {
        let mut sell = frame_at("0xcarry1", base + 600 + i as i64 * 600);
        sell["payload"]["side"] = serde_json::json!("SELL");
        sell["payload"]["size"] = serde_json::json!(1);
        // The sell floor is computed from the leader's price: at 0.42 it stands above the
        // fixture's best bid (0.28), and the exit would not go through at any level of
        // accumulation. The test is about the minimum notional, not about the price floor.
        sell["payload"]["price"] = serde_json::json!(0.30);
        sell["payload"]["transactionHash"] = serde_json::json!(tx);
        app.on_frame(&sell).await.unwrap();
        let p = db
            .positions()
            .get("0xcarry1", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .unwrap();
        assert!(
            p.pending_exit_shares > deferred,
            "the deferred amount must grow, it was {deferred}, it became {}",
            p.pending_exit_shares
        );
        deferred = p.pending_exit_shares;
    }

    // A large sale by the leader: what accumulated goes out with it.
    let mut big = frame_at("0xcarry1", base + 3000);
    big["payload"]["side"] = serde_json::json!("SELL");
    big["payload"]["size"] = serde_json::json!(60);
    big["payload"]["price"] = serde_json::json!(0.30);
    big["payload"]["transactionHash"] = serde_json::json!("0xc4");
    app.on_frame(&big).await.unwrap();

    let p = db
        .positions()
        .get("0xcarry1", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert!(p.size_sold > Decimal::ZERO, "the exit happened");
    assert!(
        p.pending_exit_shares < deferred,
        "what was sold is deducted from the deferred amount: it was {deferred}, it became {}",
        p.pending_exit_shares
    );
}

// --- The retry of a deferred exit ----------------------------------------
//
// A deferred fraction waited for the leader's next sale — and only for that. A leader who
// exited in full sells no more: on 06.09.2026 a position sat with a deferred exit covering its
// whole size and never repeated once during the run, while two closed ones carried ten
// unexecuted shares each away with them.

async fn position_owing(
    db: &Db,
    wallet: &str,
    token: &str,
    shares: Decimal,
    price: Decimal,
) -> garnet_db::Position {
    let p = db
        .positions()
        .apply_buy(
            wallet,
            token,
            Mode::Shadow,
            dec!(50),
            dec!(15),
            Decimal::ZERO,
        )
        .await
        .unwrap();
    db.positions()
        .set_pending_exit(p.id, shares, price)
        .await
        .unwrap();
    db.positions()
        .get(wallet, token, Mode::Shadow)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_pending_exit_is_retried_without_a_new_leader_sell() {
    let (app, db) = app_with_books("0xpend1", "book_thin.json").await;
    // The floor = 0.30 - 15% = 0.255, the fixture's best bid is 0.28 for 40 shares.
    let pos = position_owing(&db, "0xpend1", "tok_lal", dec!(20), dec!(0.30)).await;

    assert!(
        app.retry_pending(&pos).await.unwrap(),
        "the retry should have executed"
    );

    let after = db
        .positions()
        .get("0xpend1", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.size_sold, dec!(20));
    assert_eq!(after.pending_exit_shares, Decimal::ZERO);
    assert!(
        after.proceeds_usd > Decimal::ZERO,
        "the proceeds were not recorded"
    );
}

#[tokio::test]
async fn a_pending_exit_survives_a_book_that_cannot_pay() {
    let (app, db) = app_with_books("0xpend2", "book_thin.json").await;
    // The floor = 0.90 - 15% = 0.765, the best bid is 0.28: not enough.
    let pos = position_owing(&db, "0xpend2", "tok_lal", dec!(20), dec!(0.90)).await;

    assert!(!app.retry_pending(&pos).await.unwrap());

    let after = db
        .positions()
        .get("0xpend2", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.size_sold, Decimal::ZERO);
    // The debt remains: the retry decides nothing, it only tries.
    assert_eq!(after.pending_exit_shares, dec!(20));
    assert_eq!(after.pending_exit_price, dec!(0.90));
    // And it records no refusals: a row on every tick would clutter the order registry.
    let orders: Vec<(i64,)> = sqlx::query_as("SELECT id FROM orders")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert!(orders.is_empty(), "the retry recorded an order");
}

#[tokio::test]
async fn a_resolved_market_clears_the_debt_instead_of_selling() {
    let (app, db) = app_with_books("0xpend3", "book_thin.json").await;
    let pos = position_owing(&db, "0xpend3", "tok_eth_up", dec!(20), dec!(0.30)).await;

    assert!(!app.retry_pending(&pos).await.unwrap());

    // Settlement will return the money, and the debt is cleared — otherwise the loop would
    // come here forever.
    let after = db
        .positions()
        .get("0xpend3", "tok_eth_up", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.pending_exit_shares, Decimal::ZERO);
    assert_eq!(after.pending_exit_price, Decimal::ZERO);
}

#[tokio::test]
async fn the_killswitch_stops_the_retry_in_live() {
    let (app, db) = app_with_books("0xpend4", "book_thin.json").await;
    db.wallets().set_mode("0xpend4", Mode::Live).await.unwrap();
    let p = db
        .positions()
        .apply_buy(
            "0xpend4",
            "tok_lal",
            Mode::Live,
            dec!(50),
            dec!(15),
            Decimal::ZERO,
        )
        .await
        .unwrap();
    db.positions()
        .set_pending_exit(p.id, dec!(20), dec!(0.30))
        .await
        .unwrap();
    let pos = db
        .positions()
        .get("0xpend4", "tok_lal", Mode::Live)
        .await
        .unwrap()
        .unwrap();
    app.trip(TripReason::Manual);

    assert!(!app.retry_pending(&pos).await.unwrap());

    // A sale is a live order whoever decided it and whenever.
    let after = db
        .positions()
        .get("0xpend4", "tok_lal", Mode::Live)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.size_sold, Decimal::ZERO);
    assert_eq!(after.pending_exit_shares, dec!(20));
}

#[tokio::test]
async fn the_killswitch_keeps_the_debt_it_refused_to_pay() {
    let (app, db) = app_with("0xpend5", Mode::Live, dec!(25)).await;
    db.positions()
        .apply_buy(
            "0xpend5",
            "tok_lal",
            Mode::Live,
            dec!(50),
            dec!(15),
            Decimal::ZERO,
        )
        .await
        .unwrap();
    db.positions()
        .observe_leader("0xpend5", "tok_lal", Mode::Live, dec!(100))
        .await
        .unwrap();
    app.trip(TripReason::Manual);

    app.on_frame(&sale_of("0xpend5", 50, "0xks1"))
        .await
        .unwrap();

    // The stop halted our trading, it did not cancel the leader's sale: half the position
    // remains a debt the retry will execute once the stop is lifted.
    let pos = db
        .positions()
        .get("0xpend5", "tok_lal", Mode::Live)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        pos.size_sold,
        Decimal::ZERO,
        "the stop should have held the sale back"
    );
    assert_eq!(pos.pending_exit_shares, dec!(25));
    assert_eq!(
        pos.pending_exit_price,
        dec!(0.42),
        "the floor is computed from the leader's price"
    );
}

// --- Changing the mode of a wallet holding open positions -----------------
//
// The mode is part of the position's key, and a wallet is switched between shadow and live
// without waiting for its paper positions to close. The exit paths branched on the WALLET's
// mode while what they pick up is a position — that is, the very first row moved to live would
// have sent the exchange a sale of lots we do not hold there.

#[tokio::test]
async fn a_time_exit_of_a_paper_position_stays_paper_after_the_wallet_goes_live() {
    let clob = CountingClob(std::sync::atomic::AtomicUsize::new(0));
    let (app, db) = app_with_clob("0xflip1", Mode::Shadow, clob).await;
    db.positions()
        .apply_buy(
            "0xflip1",
            "tok_lal",
            Mode::Shadow,
            dec!(50),
            dec!(15),
            Decimal::ZERO,
        )
        .await
        .unwrap();
    // The operator moves the wallet to live, the paper position stays open.
    db.wallets().set_mode("0xflip1", Mode::Live).await.unwrap();
    let pos = db
        .positions()
        .get("0xflip1", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    let wallet = db.wallets().get("0xflip1").await.unwrap().unwrap();
    let meta = app.detector.market("tok_lal").await.unwrap();
    let book = app.books.book("tok_lal").await.unwrap();

    assert!(app.exit_stale(&pos, &wallet, &meta, &book).await.unwrap());

    assert_eq!(
        app.clob.0.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a paper position went to the exchange"
    );
    let after = db
        .positions()
        .get("0xflip1", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert!(
        after.size_sold > Decimal::ZERO,
        "the sale was not recorded in the paper row"
    );
    // And it did not create a live row with the same pair.
    assert!(db
        .positions()
        .get("0xflip1", "tok_lal", Mode::Live)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_pending_retry_of_a_paper_position_stays_paper_after_the_wallet_goes_live() {
    let clob = CountingClob(std::sync::atomic::AtomicUsize::new(0));
    let (app, db) = app_with_clob("0xflip2", Mode::Shadow, clob).await;
    let p = db
        .positions()
        .apply_buy(
            "0xflip2",
            "tok_lal",
            Mode::Shadow,
            dec!(50),
            dec!(15),
            Decimal::ZERO,
        )
        .await
        .unwrap();
    db.positions()
        .set_pending_exit(p.id, dec!(20), dec!(0.30))
        .await
        .unwrap();
    db.wallets().set_mode("0xflip2", Mode::Live).await.unwrap();
    let pos = db
        .positions()
        .get("0xflip2", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();

    assert!(app.retry_pending(&pos).await.unwrap());

    assert_eq!(
        app.clob.0.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a paper debt went to the exchange"
    );
    let after = db
        .positions()
        .get("0xflip2", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.size_sold, dec!(20));
    assert_eq!(after.pending_exit_shares, Decimal::ZERO);
}

/// Invariant 40: a leader exits by more than selling.
///
/// Merging a pair is an exit at $1 on both legs, and it arrives in no trade frame at all.
/// Until 19.09.2026 a position the leader merged out of was held by us until resolution with a
/// `leader_observed_size` that no longer meant anything.
mod leader_actions {
    use super::*;

    /// A frame about a non-trade. Labelled by the `side` field — that is how the socket
    /// labels it.
    fn action_frame(wallet: &str, kind: &str, condition: &str, size: f64) -> serde_json::Value {
        serde_json::json!({
            "topic": "activity",
            "type": "trades",
            "payload": {
                "proxyWallet": wallet,
                "conditionId": condition,
                "asset": "",
                "side": kind,
                "price": 1,
                "size": size,
                "timestamp": chrono::Utc::now().timestamp() + 6,
                "transactionHash": format!("0x{kind}_{condition}"),
            }
        })
    }

    /// A buy of the second leg of the same condition.
    fn buy_other_leg(wallet: &str) -> serde_json::Value {
        let mut f = frame_for(wallet);
        f["payload"]["asset"] = serde_json::json!("tok_bos");
        f["payload"]["outcome"] = serde_json::json!("Boston Celtics");
        f["payload"]["transactionHash"] = serde_json::json!("0xaaa2");
        f
    }

    #[tokio::test]
    async fn a_leader_merge_takes_us_out_of_both_legs() {
        let (app, db) = app_with("0xmrg1", Mode::Shadow, dec!(25)).await;

        app.on_frame(&frame_for("0xmrg1")).await.unwrap();
        app.on_frame(&buy_other_leg("0xmrg1")).await.unwrap();

        let lal = db
            .positions()
            .get("0xmrg1", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .unwrap();
        let bos = db
            .positions()
            .get("0xmrg1", "tok_bos", Mode::Shadow)
            .await
            .unwrap()
            .unwrap();
        assert!(lal.size_bought > Decimal::ZERO && bos.size_bought > Decimal::ZERO);
        assert_eq!(
            lal.size_sold,
            Decimal::ZERO,
            "before the merge we sold nothing"
        );
        assert_eq!(bos.size_sold, Decimal::ZERO);

        // The leader merged 120 of the 120 observed — that is, exited in full.
        app.on_frame(&action_frame("0xmrg1", "MERGE", "0xsports1", 120.0))
            .await
            .unwrap();

        let lal = db
            .positions()
            .get("0xmrg1", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .unwrap();
        let bos = db
            .positions()
            .get("0xmrg1", "tok_bos", Mode::Shadow)
            .await
            .unwrap()
            .unwrap();
        assert!(lal.size_sold > Decimal::ZERO, "the first leg was not sold");
        assert!(bos.size_sold > Decimal::ZERO, "the second leg was not sold");

        let act = &db.actions().recent(5).await.unwrap()[0];
        assert_eq!(act.kind, "merge");
        assert!(act.handled_at.is_some(), "the merge was handled");
    }

    #[tokio::test]
    async fn the_same_merge_delivered_twice_sells_once() {
        // Two circuits deliver one event. Selling on it twice would mean exiting a position
        // that no longer exists — precisely what the table has a dedup key for.

        let (app, db) = app_with("0xmrg2", Mode::Shadow, dec!(25)).await;
        app.on_frame(&frame_for("0xmrg2")).await.unwrap();

        let frame = action_frame("0xmrg2", "MERGE", "0xsports1", 60.0);
        app.on_frame(&frame).await.unwrap();
        let after_first = db
            .positions()
            .get("0xmrg2", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .unwrap()
            .size_sold;

        app.on_frame(&frame).await.unwrap(); // the same merge once more
        let after_second = db
            .positions()
            .get("0xmrg2", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .unwrap()
            .size_sold;

        assert_eq!(
            after_first, after_second,
            "a repeat does not sell a second time"
        );
        assert_eq!(db.actions().recent(10).await.unwrap().len(), 1, "one row");
    }

    #[tokio::test]
    async fn a_leader_split_buys_nothing() {
        // A split buys a pair for $1: that is not a bet on an outcome but a placement of
        // capital. Copying it with a `stake_usd` stake would mean betting twice.
        let (app, db) = app_with("0xmrg3", Mode::Shadow, dec!(25)).await;

        app.on_frame(&action_frame("0xmrg3", "SPLIT", "0xsports1", 50.0))
            .await
            .unwrap();

        assert!(
            db.positions()
                .for_wallet("0xmrg3")
                .await
                .unwrap()
                .is_empty(),
            "a split gives birth to no buy"
        );
        assert!(
            db.signals().recent(5).await.unwrap().is_empty(),
            "nor a decision"
        );

        let acts = db.actions().recent(5).await.unwrap();
        assert_eq!(
            acts.len(),
            1,
            "but the trace remains: without it no decision can be taken"
        );
        assert_eq!(acts[0].kind, "split");
        assert!(acts[0].handled_at.is_none(), "there is nothing to act on");
    }

    #[tokio::test]
    async fn a_leader_redemption_leaves_a_record_and_nothing_else() {
        // A leader's redemption does not touch our position: our own settlement closes it,
        // with its own source of truth.
        let (app, db) = app_with("0xmrg4", Mode::Shadow, dec!(25)).await;
        app.on_frame(&frame_for("0xmrg4")).await.unwrap();
        let before = db
            .positions()
            .get("0xmrg4", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .unwrap();

        app.on_frame(&action_frame("0xmrg4", "REDEEM", "0xsports1", 120.0))
            .await
            .unwrap();

        let after = db
            .positions()
            .get("0xmrg4", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.size_sold, before.size_sold,
            "a redemption sells nothing"
        );
        assert!(after.closed_at.is_none(), "and closes nothing");

        let acts = db.actions().recent(5).await.unwrap();
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].kind, "redeem");
    }

    #[tokio::test]
    async fn a_merge_older_than_the_assignment_does_not_sell() {
        // `/activity?limit=20` on a quiet wallet returns weeks of history, and on the first
        // tick all of it looks like news. A month-old merge would sell a position that did
        // not exist back then.
        let (app, db) = app_with("0xmrg5", Mode::Shadow, dec!(25)).await;
        app.on_frame(&frame_for("0xmrg5")).await.unwrap();

        let mut old = action_frame("0xmrg5", "MERGE", "0xsports1", 120.0);
        old["payload"]["timestamp"] =
            serde_json::json!(chrono::Utc::now().timestamp() - 30 * 24 * 3600);
        app.on_frame(&old).await.unwrap();

        let pos = db
            .positions()
            .get("0xmrg5", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pos.size_sold, Decimal::ZERO, "an old merge sells nothing");
        assert!(
            db.actions().recent(5).await.unwrap()[0]
                .handled_at
                .is_none(),
            "and does not count as handled"
        );
    }
}

/// The emergency close (invariant 46). The gate is checked separately and without a network;
/// here it is what exactly each mode does with the positions.
mod flatten {
    use super::*;
    use garnet_core::flatten::FlattenMode;

    /// A book with a bid of 0.28 — above our entry price, so the position is in profit.
    async fn with_position(wallet: &str) -> (App<StubMarkets, StubBooks, StubClob>, Db) {
        let (app, db) = app_with(wallet, Mode::Live, dec!(25)).await;
        app.set_live_cash(dec!(500));
        app.on_frame(&frame_for(wallet)).await.unwrap();
        (app, db)
    }

    #[tokio::test]
    async fn graceful_sells_nothing_and_stops_trading() {
        let (app, db) = with_position("0xfl1").await;
        let before = db
            .positions()
            .get("0xfl1", "tok_lal", Mode::Live)
            .await
            .unwrap()
            .unwrap();

        let r = app.flatten(FlattenMode::Graceful, "test").await.unwrap();

        assert_eq!(r.sold, 0, "graceful sells nothing");
        assert_eq!(r.kept_to_live, 1);
        let after = db
            .positions()
            .get("0xfl1", "tok_lal", Mode::Live)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.size_sold, before.size_sold);
        assert!(
            db.controls().manual_stop().await.unwrap(),
            "trading is halted"
        );
    }

    #[tokio::test]
    async fn panic_sells_despite_the_killswitch_being_on() {
        // `panic` is permitted only while trading is halted, and a stop forbidding it to sell
        // would make the mode impossible by construction: the operator would halt trading,
        // type the phrase and sell nothing.

        let (app, db) = with_position("0xfl2").await;
        db.controls()
            .set_manual_stop(true, "operator")
            .await
            .unwrap();
        app.trip(TripReason::Manual);

        let r = app.flatten(FlattenMode::Panic, "test").await.unwrap();

        assert_eq!(r.sold, 1, "the stop does not cancel the emergency close");
        let after = db
            .positions()
            .get("0xfl2", "tok_lal", Mode::Live)
            .await
            .unwrap()
            .unwrap();
        assert!(after.size_sold > Decimal::ZERO);
    }

    #[tokio::test]
    async fn hybrid_keeps_what_is_not_in_profit() {
        // The operator allowed taking a profit, not locking in a loss. We entered at 0.42
        // with a fee; the best bid in `book_thin` is 0.28.
        let (app, db) = with_position("0xfl3").await;

        let r = app.flatten(FlattenMode::Hybrid, "test").await.unwrap();

        assert_eq!(
            r.sold, 0,
            "the position is at a loss — hybrid leaves it alone"
        );
        assert_eq!(r.kept_losing, 1);
        let after = db
            .positions()
            .get("0xfl3", "tok_lal", Mode::Live)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.size_sold, Decimal::ZERO);
    }

    #[tokio::test]
    async fn hybrid_will_not_sell_what_it_cannot_price() {
        // There are no bids: there is nobody to sell to, and there is nothing to say whether
        // the position is in profit. Selling it would mean going beyond what the operator
        // agreed to.
        let (app, db) = app_with_books("0xfl4", "book_no_bids.json").await;
        db.positions()
            .apply_buy("0xfl4", "tok_lal", Mode::Live, dec!(10), dec!(4), dec!(0.1))
            .await
            .unwrap();

        let r = app.flatten(FlattenMode::Hybrid, "test").await.unwrap();

        assert_eq!(r.sold, 0);
        assert_eq!(
            r.kept_unmeasurable, 1,
            "nothing to measure with — we leave it alone"
        );
    }

    #[tokio::test]
    async fn a_paper_position_is_never_flattened() {
        // Paper positions are not money: there is nothing to save in them, and closing them
        // would destroy the only comparison shadow exists for.
        let (app, db) = app_with("0xfl5", Mode::Shadow, dec!(25)).await;
        app.on_frame(&frame_for("0xfl5")).await.unwrap();
        let before = db
            .positions()
            .get("0xfl5", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .unwrap();

        let r = app.flatten(FlattenMode::Panic, "test").await.unwrap();

        assert_eq!(
            r.considered, 0,
            "paper positions do not enter the emergency close"
        );
        let after = db
            .positions()
            .get("0xfl5", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.size_sold, before.size_sold);
    }

    #[tokio::test]
    async fn a_resolved_market_is_left_to_the_settlement() {
        // The market resolved: settlement will return the money, there is nothing to sell.
        let (app, db) = app_with("0xfl6", Mode::Live, dec!(25)).await;
        db.positions()
            .apply_buy(
                "0xfl6",
                "tok_eth_up",
                Mode::Live,
                dec!(10),
                dec!(4),
                dec!(0.1),
            )
            .await
            .unwrap();

        let r = app.flatten(FlattenMode::Panic, "test").await.unwrap();

        assert_eq!(r.sold, 0);
        assert_eq!(r.kept_to_live, 1);
        assert!(
            r.failed.is_empty(),
            "this is not a failure but a state of the market"
        );
    }
}

/// The hot path (invariant 42): it has no sequential waits that could run in parallel.
///
/// Parallelism must change nothing except the timing. These tests check the "nothing except".
mod hot_path {
    use super::*;
    use garnet_risk::metrics::STAGES;

    #[tokio::test]
    async fn every_declared_stage_is_actually_measured() {
        // Two of the four stages were declared in `STAGES` and never written: the hot path
        // that the work set out to speed up had nothing to measure it with. An instrument
        // showing emptiness is indistinguishable from an instantaneous path.

        let (app, _db) = app_with("0xhp1", Mode::Shadow, dec!(25)).await;
        app.on_frame(&frame_for("0xhp1")).await.unwrap();

        for stage in [STAGES[0], STAGES[1], STAGES[2]] {
            assert!(
                app.metrics.count_of(stage) > 0,
                "the stage \"{stage}\" is declared but not measured"
            );
        }

        // The fourth exists only for a live fill: a paper one is simulated against the book
        // within the same task, and its latency is identically zero.
        assert_eq!(
            app.metrics.count_of(STAGES[3]),
            0,
            "a paper fill must not enter the measurement of live latency"
        );
        let (live, _db) = app_with("0xhp1live", Mode::Live, dec!(25)).await;
        live.set_live_cash(dec!(500));
        live.on_frame(&frame_for("0xhp1live")).await.unwrap();
        assert!(
            live.metrics.count_of(STAGES[3]) > 0,
            "the stage \"{}\"",
            STAGES[3]
        );
    }

    #[tokio::test]
    async fn the_order_of_records_survives_the_parallel_reads() {
        // The four database calls now run at once, but `signals` has to stay ahead of
        // `orders`: the order registry references a decision, and an order with no decision
        // behind it is an invented reference.
        let (app, db) = app_with("0xhp2", Mode::Shadow, dec!(25)).await;
        app.on_frame(&frame_for("0xhp2")).await.unwrap();

        let signal_ts: chrono::DateTime<chrono::Utc> =
            sqlx::query_scalar("SELECT ts_signal FROM signals ORDER BY id LIMIT 1")
                .fetch_one(db.pool())
                .await
                .unwrap();
        let (order_ts, signal_id): (chrono::DateTime<chrono::Utc>, Option<i64>) =
            sqlx::query_as("SELECT ts_submitted, signal_id FROM orders ORDER BY id LIMIT 1")
                .fetch_one(db.pool())
                .await
                .unwrap();

        assert!(signal_id.is_some(), "a buy does have a decision behind it");
        assert!(
            order_ts >= signal_ts,
            "the decision was recorded before the order"
        );
    }

    #[tokio::test]
    async fn a_skip_leaves_a_signal_and_no_order() {
        // Parallelism must not give birth to an order where the verdict is a refusal.
        let (app, db) = app_with_books("0xhp3", "book_pricey.json").await;
        app.on_frame(&frame_for("0xhp3")).await.unwrap();

        let signals = db.signals().recent(5).await.unwrap();
        assert_eq!(signals.len(), 1, "a trace remains on a refusal too");
        assert_ne!(signals[0].verdict, "copy");
        let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(orders, 0, "a refusal leaves no order");
    }

    #[tokio::test]
    async fn the_leader_observation_still_lands_before_the_verdict_needs_it() {
        // `observe_leader` moved in parallel with the reads. It writes what the exit fraction
        // is later computed from, and must not get lost because of that.
        let (app, db) = app_with("0xhp4", Mode::Shadow, dec!(25)).await;
        app.on_frame(&frame_for("0xhp4")).await.unwrap();

        let pos = db
            .positions()
            .get("0xhp4", "tok_lal", Mode::Shadow)
            .await
            .unwrap()
            .expect("the position was created");
        assert_eq!(
            pos.leader_observed_size,
            dec!(120),
            "the leader's holding is remembered"
        );
    }

    #[tokio::test]
    async fn the_wave_window_still_cuts_the_second_slice() {
        // Reading the wave moved in parallel too. The slice window (invariant 29) has to work
        // exactly as it did.
        let (app, db) = app_with("0xhp5", Mode::Shadow, dec!(25)).await;
        let mut second = frame_for("0xhp5");
        second["payload"]["transactionHash"] = serde_json::json!("0xslice2");

        app.on_frame(&frame_for("0xhp5")).await.unwrap();
        app.on_frame(&second).await.unwrap();

        let signals = db.signals().recent(5).await.unwrap();
        assert_eq!(signals.len(), 2);
        assert_eq!(
            signals[0].verdict, "skip:duplicate",
            "the second slice was collapsed"
        );
    }
}
