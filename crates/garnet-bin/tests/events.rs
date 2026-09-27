//! Trading-path events against a live NATS.
//!
//! Verified against a real bus rather than a spy: an event that did not reach NATS is
//! indistinguishable from one never published, and faking the addressee would check our call
//! instead of the delivery.
//!
//! The test skips itself if NATS is not up: it is about delivery, not about the presence of
//! infrastructure.

use garnet_bin::app::{App, BookSource};
use garnet_bin::events::Events;
use garnet_bus::{subjects, Bus};
use garnet_config::Config;
use garnet_core::book::Book;
use garnet_core::detect::{Detector, MarketSource};
use garnet_core::execute::{ClobExec, ExecError, OrderRequest};
use garnet_core::market_meta::{parse_clob_market, MarketMeta};
use garnet_core::shadow::Fill;
use garnet_db::Mode;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio_stream::StreamExt;

const URL: &str = "nats://127.0.0.1:4223";

fn fixture(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct StubMarkets;

impl MarketSource for StubMarkets {
    async fn get(&self, token_id: &str) -> anyhow::Result<MarketMeta> {
        parse_clob_market(&fixture("clob_sports.json"), token_id)
    }
}

struct StubBooks;

impl BookSource for StubBooks {
    async fn book(&self, _token_id: &str) -> anyhow::Result<Book> {
        Book::from_clob(&fixture("book_thin.json"))
    }
}

struct StubClob;

impl ClobExec for StubClob {
    async fn place_ioc(&self, _req: &OrderRequest) -> Result<Fill, ExecError> {
        Err(ExecError::Rejected("this test does not trade live".into()))
    }
}

/// A namespace of its own per run.
///
/// On 05.09.2026 these tests published to the same bus `garnet-tg` listens on, and the
/// operator received fixtures in the chat: "Resolved · LIVE · 0xsettled", one message per
/// test run.
fn namespace() -> String {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("t_events_{ns}")
}

async fn bus_or_skip_in(ns: &str) -> Option<Bus> {
    match tokio::time::timeout(std::time::Duration::from_secs(2), Bus::connect_in(URL, ns)).await {
        Ok(Ok(b)) => Some(b),
        _ => {
            eprintln!("NATS unreachable at {URL}: test skipped");
            None
        }
    }
}

#[tokio::test]
async fn a_copied_trade_is_announced_on_the_bus() {
    let ns = namespace();
    let Some(listener) = bus_or_skip_in(&ns).await else {
        return;
    };
    let mut signals = listener.subscribe(subjects::SIGNAL_DETECTED).await.unwrap();
    let mut fills = listener.subscribe(subjects::ORDER_FILLED).await.unwrap();

    let db = garnet_db::testing::isolated_db("events").await.unwrap();
    db.wallets().add("0xevents", Some("whale")).await.unwrap();
    db.wallets().set_stake("0xevents", dec!(25)).await.unwrap();
    db.wallets().set_enabled("0xevents", true).await.unwrap();

    let events = Events::connect_in(URL, &ns)
        .await
        .expect("the bus is connected");
    let detector = Detector::new(db.clone(), StubMarkets, ["0xevents".to_string()]);
    let app = App::new(
        db.clone(),
        detector,
        StubBooks,
        StubClob,
        &Config::from_toml("database_url = \"unused\"").unwrap(),
        events,
    );

    let mut frame = fixture("rtds_normal.json");
    frame["payload"]["proxyWallet"] = serde_json::json!("0xevents");
    // The fixture is dated 2026-09-03 while the wallet is created now: trades older than the
    // assignment give birth to no signal (see `wiring.rs`).
    frame["payload"]["timestamp"] = serde_json::json!(chrono::Utc::now().timestamp());
    app.on_frame(&frame).await.unwrap();

    let signal = next_event(&mut signals).await;
    assert_eq!(signal["wallet"], "0xevents");
    assert_eq!(signal["verdict"], "copy");
    assert_eq!(signal["mode"], "shadow");

    let fill = next_event(&mut fills).await;
    assert_eq!(fill["token_id"], "tok_lal");
    assert_eq!(fill["mode"], "shadow");
    assert!(
        fill["size"].as_str().unwrap().parse::<f64>().unwrap() > 0.0,
        "the fill's size must be in the event: {fill}"
    );
}

#[tokio::test]
async fn without_a_bus_the_trade_path_still_works() {
    // The bus is not a precondition of trading. An unreachable NATS must cost us events, not
    // trades: in the predecessor a dependency of that kind dropping out stopped the whole
    // circuit.

    let db = garnet_db::testing::isolated_db("events_off").await.unwrap();
    db.wallets().add("0xnobus", None).await.unwrap();
    db.wallets().set_stake("0xnobus", dec!(25)).await.unwrap();
    db.wallets().set_enabled("0xnobus", true).await.unwrap();

    let detector = Detector::new(db.clone(), StubMarkets, ["0xnobus".to_string()]);
    let app = App::new(
        db.clone(),
        detector,
        StubBooks,
        StubClob,
        &Config::from_toml("database_url = \"unused\"").unwrap(),
        Events::off(),
    );

    let mut frame = fixture("rtds_normal.json");
    frame["payload"]["proxyWallet"] = serde_json::json!("0xnobus");
    // The fixture is dated 2026-09-03 while the wallet is created now: trades older than the
    // assignment give birth to no signal (see `wiring.rs`).
    frame["payload"]["timestamp"] = serde_json::json!(chrono::Utc::now().timestamp());
    app.on_frame(&frame).await.unwrap();

    let pos = db
        .positions()
        .get("0xnobus", "tok_lal", Mode::Shadow)
        .await
        .unwrap()
        .expect("the position was taken on without a bus");
    assert!(pos.size_bought > Decimal::ZERO);
}

async fn next_event(
    sub: &mut (impl StreamExt<Item = async_nats::Message> + Unpin),
) -> serde_json::Value {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), sub.next())
        .await
        .expect("no event within 3 s")
        .expect("the subscription closed");
    serde_json::from_slice(&msg.payload).unwrap()
}

struct ResolvedMarkets;

impl MarketSource for ResolvedMarkets {
    async fn get(&self, token_id: &str) -> anyhow::Result<MarketMeta> {
        // A crypto pair: Down won. The labels here are deliberately not Yes/No — the event
        // has to carry the real outcome label.
        parse_clob_market(&fixture("clob_resolved_down.json"), token_id)
    }
}

#[tokio::test]
async fn a_resolved_position_is_announced_on_the_bus() {
    // `settle_once` returned only counters, and a resolution was the only trading-path event
    // the bus stayed silent about: the dashboard and Telegram saw the entry without seeing the
    // outcome.
    let ns = namespace();
    let Some(listener) = bus_or_skip_in(&ns).await else {
        return;
    };
    let mut settled = listener
        .subscribe(subjects::POSITION_SETTLED)
        .await
        .unwrap();

    let db = garnet_db::testing::isolated_db("events_settled")
        .await
        .unwrap();
    db.wallets().add("0xsettled", None).await.unwrap();
    db.positions()
        .apply_buy(
            "0xsettled",
            "tok_sol_down",
            Mode::Live,
            dec!(4),
            dec!(1),
            dec!(0.05),
        )
        .await
        .unwrap();

    let events = Events::connect_in(URL, &ns)
        .await
        .expect("the bus is connected");
    let report = garnet_bin::loops::settle_and_announce(
        &db,
        &ResolvedMarkets,
        &garnet_bin::auto_payout::AutoPayout,
        50,
        &events,
    )
    .await
    .unwrap();
    assert_eq!(
        report.settled.len(),
        1,
        "the resolution rows must be returned outwards"
    );

    let ev = next_event(&mut settled).await;
    assert_eq!(ev["wallet"], "0xsettled");
    assert_eq!(ev["token_id"], "tok_sol_down");
    assert_eq!(ev["mode"], "live");
    assert_eq!(
        ev["resolved_outcome"], "Down",
        "the outcome label is not Yes/No"
    );
    assert_eq!(ev["won"], true);
    // Numbers travel as strings, like every Decimal on the bus; the trailing zeros from
    // numeric change nothing.
    assert_eq!(decimal(&ev["payout_usd"]), dec!(4));
    // The payout minus the stake and the fee: 4 - 1 - 0.05.
    assert_eq!(decimal(&ev["pnl_usd"]), dec!(2.95));
    assert!(
        ev["tx_hash"].is_null(),
        "the platform credits the payout, the hash is not ours"
    );
}

fn decimal(v: &serde_json::Value) -> Decimal {
    use std::str::FromStr as _;
    Decimal::from_str(v.as_str().unwrap_or_else(|| panic!("not a string: {v}"))).unwrap()
}
