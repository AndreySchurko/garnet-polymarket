//! Executing commands against the wallet registry.
//!
//! The rule lives here: three actions — moving a wallet **to live**, changing the stake and
//! `/kill` — execute only after a button press. Everything else happens immediately: people
//! stop reading a confirmation that appears for everything, and it stops protecting
//! precisely where it is needed.

use garnet_db::{Db, Mode};
use garnet_telegram::commands::{handle, Reply};
use garnet_telegram::update::Incoming;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const CHAT: i64 = 42;

async fn fresh(addr: &str) -> Db {
    let db = Db::connect(&garnet_db::testing::default_url())
        .await
        .unwrap();
    db.migrate().await.unwrap();
    sqlx::query("DELETE FROM wallets WHERE address = $1")
        .bind(addr)
        .execute(db.pool())
        .await
        .unwrap();
    db
}

async fn say(db: &Db, text: &str) -> Reply {
    handle(
        db,
        &Incoming::Message {
            chat_id: CHAT,
            text: text.into(),
        },
    )
    .await
    .unwrap()
    .expect("there must be a reply")
}

async fn press(db: &Db, data: &str) -> Reply {
    handle(
        db,
        &Incoming::Button {
            chat_id: CHAT,
            message_id: 7,
            query_id: "cb".into(),
            data: data.into(),
        },
    )
    .await
    .unwrap()
    .expect("there must be a reply")
}

#[tokio::test]
async fn adding_a_wallet_no_longer_needs_psql() {
    let addr = "0x00000000000000000000000000000000000000a1";
    let db = fresh(addr).await;

    let reply = say(&db, &format!("/add {addr} whale-1")).await;
    assert!(
        reply.text.contains("whale-1"),
        "the reply names the wallet: {}",
        reply.text
    );

    let w = db
        .wallets()
        .get(addr)
        .await
        .unwrap()
        .expect("the wallet was created");
    assert_eq!(w.mode, Mode::Shadow, "a new wallet is born in shadow");
    assert!(!w.enabled, "and disabled");
}

#[tokio::test]
async fn adding_the_same_wallet_twice_explains_itself() {
    // A duplicate is a uniqueness error in the database. The operator must see a sentence,
    // not silence and not a panic of the poll loop.
    let addr = "0x00000000000000000000000000000000000000a2";
    let db = fresh(addr).await;
    say(&db, &format!("/add {addr}")).await;

    let reply = say(&db, &format!("/add {addr}")).await;
    assert!(
        reply.text.contains("already"),
        "the repeat is explained in words: {}",
        reply.text
    );
}

#[tokio::test]
async fn going_live_waits_for_a_button() {
    let addr = "0x00000000000000000000000000000000000000a3";
    let db = fresh(addr).await;
    say(&db, &format!("/add {addr}")).await;

    let ask = say(&db, &format!("/mode {addr} live")).await;
    assert!(
        !ask.buttons.is_empty(),
        "moving to live must ask with a button"
    );
    assert_eq!(
        db.wallets().get(addr).await.unwrap().unwrap().mode,
        Mode::Shadow,
        "nothing changes before the press"
    );

    let data = ask.buttons[0][0].1.clone();
    assert!(
        data.len() <= 64,
        "Telegram truncates callback_data past 64 bytes: {data}"
    );

    let done = press(&db, &data).await;
    assert!(done.text.contains("REAL MONEY"), "{}", done.text);
    assert_eq!(
        db.wallets().get(addr).await.unwrap().unwrap().mode,
        Mode::Live
    );
}

#[tokio::test]
async fn coming_back_to_shadow_needs_no_confirmation() {
    // A confirmation protects against spending money, not against stopping.
    let addr = "0x00000000000000000000000000000000000000a4";
    let db = fresh(addr).await;
    say(&db, &format!("/add {addr}")).await;
    db.wallets().set_mode(addr, Mode::Live).await.unwrap();

    let reply = say(&db, &format!("/mode {addr} shadow")).await;
    assert!(reply.buttons.is_empty());
    assert_eq!(
        db.wallets().get(addr).await.unwrap().unwrap().mode,
        Mode::Shadow
    );
}

#[tokio::test]
async fn the_stake_waits_for_a_button_and_the_slippage_does_not() {
    let addr = "0x00000000000000000000000000000000000000a5";
    let db = fresh(addr).await;
    say(&db, &format!("/add {addr}")).await;

    let ask = say(&db, &format!("/stake {addr} 25")).await;
    assert!(
        !ask.buttons.is_empty(),
        "the stake is the size of every trade"
    );
    assert_eq!(
        db.wallets().get(addr).await.unwrap().unwrap().stake_usd,
        dec!(0)
    );

    press(&db, &ask.buttons[0][0].1.clone()).await;
    assert_eq!(
        db.wallets().get(addr).await.unwrap().unwrap().stake_usd,
        dec!(25)
    );

    // The command speaks in percent while a fraction is stored: 5.5% is 0.055.
    say(&db, &format!("/slippage {addr} 5.5")).await;
    assert_eq!(
        db.wallets()
            .get(addr)
            .await
            .unwrap()
            .unwrap()
            .max_slippage_pct,
        dec!(0.055)
    );
}

#[tokio::test]
async fn every_change_leaves_a_trace_naming_the_chat() {
    // Every change is a row in `wallet_events`. The actor has to name the chat: "operator"
    // does not answer who exactly moved it to live.
    let addr = "0x00000000000000000000000000000000000000a6";
    let db = fresh(addr).await;
    say(&db, &format!("/add {addr}")).await;
    say(&db, &format!("/slippage {addr} 3")).await;

    let actor: String = sqlx::query_scalar(
        "SELECT actor FROM wallet_events WHERE wallet = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(addr)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(actor, format!("telegram:{CHAT}"));
}

#[tokio::test]
async fn a_kill_waits_for_a_button_and_a_resume_does_not() {
    // The stop is one per installation — the test about it lives in a schema of its own.
    let db = garnet_db::testing::isolated_db("tg_kill").await.unwrap();

    let ask = say(&db, "/kill").await;
    assert!(!ask.buttons.is_empty(), "/kill asks for confirmation");
    assert!(
        !db.controls().manual_stop().await.unwrap(),
        "we are not halted before the press"
    );

    press(&db, &ask.buttons[0][0].1.clone()).await;
    assert!(db.controls().manual_stop().await.unwrap());

    // Lifting the stop is a return to normal and asks no questions.
    let resumed = say(&db, "/resume").await;
    assert!(resumed.buttons.is_empty());
    assert!(!db.controls().manual_stop().await.unwrap());
}

#[tokio::test]
async fn a_refusal_changes_nothing() {
    let addr = "0x00000000000000000000000000000000000000a8";
    let db = fresh(addr).await;
    say(&db, &format!("/add {addr}")).await;
    say(&db, &format!("/mode {addr} live")).await;

    let no = press(&db, "no").await;
    assert!(no.text.contains("Cancelled"), "{}", no.text);
    assert_eq!(
        db.wallets().get(addr).await.unwrap().unwrap().mode,
        Mode::Shadow
    );
}

#[tokio::test]
async fn the_wallet_list_says_what_matters() {
    let addr = "0x00000000000000000000000000000000000000a9";
    let db = fresh(addr).await;
    say(&db, &format!("/add {addr} whale-9")).await;
    press(
        &db,
        &say(&db, &format!("/stake {addr} 25")).await.buttons[0][0]
            .1
            .clone(),
    )
    .await;

    let list = say(&db, "/wallets").await;
    assert!(list.text.contains("whale-9"), "{}", list.text);
    assert!(
        list.text.contains("25"),
        "the stake is visible: {}",
        list.text
    );
    assert!(
        list.text.contains("paper"),
        "the mode is visible: {}",
        list.text
    );
}

#[tokio::test]
async fn an_unknown_wallet_is_named_not_swallowed() {
    let db = fresh("0x0000000000000000000000000000000000000aaa").await;
    let reply = say(&db, "/wallet 0x0000000000000000000000000000000000000aaa").await;
    assert!(reply.text.contains("was not found"), "{}", reply.text);
}

#[tokio::test]
async fn a_malformed_address_never_reaches_the_database() {
    let db = fresh("0xnope").await;
    let reply = say(&db, "/add 0xnope").await;
    assert!(
        reply.text.contains("does not look like an address"),
        "{}",
        reply.text
    );
}

#[tokio::test]
async fn positions_lists_what_we_hold_in_words() {
    let db = garnet_db::testing::isolated_db("tg_pos").await.unwrap();
    db.wallets().add("0xp1", Some("whale-P")).await.unwrap();
    db.positions()
        .apply_buy(
            "0xp1",
            "tok_pos",
            Mode::Live,
            dec!(59.5),
            dec!(25),
            dec!(0.3),
        )
        .await
        .unwrap();

    let reply = say(&db, "/positions").await;
    assert!(reply.text.contains("whale-P"), "{}", reply.text);
    assert!(
        reply.text.contains("25"),
        "the amount invested is visible: {}",
        reply.text
    );
    assert!(reply.text.contains("Real money"), "{}", reply.text);
}

#[tokio::test]
async fn positions_says_so_when_there_are_none() {
    // An empty reply reads as a breakage: the list has to say that it is empty.
    let db = garnet_db::testing::isolated_db("tg_pos_empty")
        .await
        .unwrap();
    let reply = say(&db, "/positions").await;
    assert!(reply.text.contains("no open positions"), "{}", reply.text);
}

#[tokio::test]
async fn pnl_keeps_the_two_modes_in_separate_columns() {
    // Adding the paper result to the real one destroys the only comparison shadow exists
    // for.
    let db = garnet_db::testing::isolated_db("tg_pnl").await.unwrap();
    db.wallets().add("0xq1", None).await.unwrap();
    for (mode, payout) in [(Mode::Live, dec!(4)), (Mode::Shadow, dec!(9))] {
        let p = db
            .positions()
            .apply_buy("0xq1", "tok_pnl", mode, dec!(1), dec!(1), Decimal::ZERO)
            .await
            .unwrap();
        db.settlements()
            .record(p.id, "tok_pnl", "Up", true, payout, None)
            .await
            .unwrap();
        db.positions().close(p.id).await.unwrap();
    }

    let reply = say(&db, "/pnl all").await;
    assert!(reply.text.contains("Real money"), "{}", reply.text);
    assert!(reply.text.contains("Paper"), "{}", reply.text);
    assert!(
        reply.text.contains("3"),
        "the live result is +3: {}",
        reply.text
    );
    assert!(
        reply.text.contains("8"),
        "the shadow result is +8: {}",
        reply.text
    );
}

#[tokio::test]
async fn pnl_defaults_to_the_day_and_names_the_period() {
    let db = garnet_db::testing::isolated_db("tg_pnl_period")
        .await
        .unwrap();
    let reply = say(&db, "/pnl").await;
    assert!(
        reply.text.contains("24 hours"),
        "the period is named: {}",
        reply.text
    );

    let week = say(&db, "/pnl week").await;
    assert!(week.text.contains("week"), "{}", week.text);

    let bad = say(&db, "/pnl month").await;
    assert!(
        bad.text.contains("day"),
        "an unknown period is explained: {}",
        bad.text
    );
}

#[tokio::test]
async fn signals_show_skips_with_their_reason() {
    let db = garnet_db::testing::isolated_db("tg_sig").await.unwrap();
    db.wallets().add("0xs1", Some("whale-S")).await.unwrap();
    let trade = db
        .trades()
        .record(&garnet_db::NewLeaderTrade {
            wallet: "0xs1".into(),
            tx_hash: "0xsig".into(),
            token_id: "tok_s".into(),
            side: garnet_db::Side::Buy,
            price: dec!(0.4),
            size: dec!(100),
            ts_trade: chrono::Utc::now(),
            source: garnet_db::Source::Rtds,
            market_text: "Lakers vs Celtics".into(),
            outcome_text: "Lakers to win".into(),
        })
        .await
        .unwrap()
        .fresh()
        .unwrap();
    db.signals()
        .record(
            trade.id,
            "0xs1",
            Mode::Live,
            "skip:slippage_exceeded",
            dec!(25),
            Some(dec!(0.46)),
            Some(dec!(0.52)),
            Some(dec!(0.45)),
        )
        .await
        .unwrap();

    let reply = say(&db, "/signals").await;
    assert!(
        reply.text.contains("moved past the slippage threshold"),
        "the reason in words rather than as a label: {}",
        reply.text
    );
    assert!(
        reply.text.contains("Lakers to win"),
        "the outcome in words: {}",
        reply.text
    );
    // A skip has no stake, and "for $0" reads as an arithmetic error.
    assert!(
        !reply.text.contains("for $0"),
        "a skip is not a stake: {}",
        reply.text
    );
    // "Past the threshold" without figures does not say how far past: a threshold is raised
    // for a tenth of a cent, not for twenty percent.
    assert!(
        reply.text.contains("0.52") && reply.text.contains("0.46"),
        "the miss must be visible in both halves: {}",
        reply.text
    );
}

/// A refusal without a price and a refusal on price are different things, and "the book is
/// empty" must not be shown as "expensive": until 06.09.2026 an empty book was substituted
/// with one and reached the reader exactly that way.
#[tokio::test]
async fn a_skip_without_a_book_says_so() {
    let db = garnet_db::testing::isolated_db("tg_sig_nb").await.unwrap();
    db.wallets().add("0xs2", Some("whale-N")).await.unwrap();
    let trade = db
        .trades()
        .record(&garnet_db::NewLeaderTrade {
            wallet: "0xs2".into(),
            tx_hash: "0xsig2".into(),
            token_id: "tok_s".into(),
            side: garnet_db::Side::Buy,
            price: dec!(0.4),
            size: dec!(100),
            ts_trade: chrono::Utc::now(),
            source: garnet_db::Source::Rtds,
            market_text: "Lakers vs Celtics".into(),
            outcome_text: "Lakers to win".into(),
        })
        .await
        .unwrap()
        .fresh()
        .unwrap();
    db.signals()
        .record(
            trade.id,
            "0xs2",
            Mode::Live,
            "skip:market_not_tradable",
            Decimal::ZERO,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let reply = say(&db, "/signals").await;
    assert!(
        !reply.text.contains("slippage threshold"),
        "the absence of a price is not slippage: {}",
        reply.text
    );
    assert!(
        reply.text.contains("nobody to buy from") || reply.text.contains("not tradable"),
        "{}",
        reply.text
    );
}

#[tokio::test]
async fn the_scanner_says_it_does_not_exist_yet() {
    // Silence on a known command looks like a broken bot rather than a missing scanner.

    let db = garnet_db::testing::isolated_db("tg_scan").await.unwrap();
    let reply = say(&db, "/scan run").await;
    assert!(
        reply.text.to_lowercase().contains("scanner"),
        "{}",
        reply.text
    );
}

#[tokio::test]
async fn the_wallet_list_carries_the_daily_result() {
    let db = garnet_db::testing::isolated_db("tg_list_pnl")
        .await
        .unwrap();
    db.wallets().add("0xw1", Some("whale-D")).await.unwrap();
    let p = db
        .positions()
        .apply_buy("0xw1", "tok_d", Mode::Live, dec!(1), dec!(1), Decimal::ZERO)
        .await
        .unwrap();
    db.settlements()
        .record(p.id, "tok_d", "Up", true, dec!(6), None)
        .await
        .unwrap();
    db.positions().close(p.id).await.unwrap();

    let reply = say(&db, "/wallets").await;
    assert!(
        reply.text.contains("+$5"),
        "the 24-hour result is in the list: {}",
        reply.text
    );
}

#[tokio::test]
async fn the_slippage_is_typed_in_percent_and_stored_as_a_fraction() {
    // Found by a live check on 2026-09-04: the command is described as `<%>` while
    // `buy_limit` computes `price x (1 + max_slippage_pct)`, that is, a fraction is stored.
    // Stored as it comes, "5" would mean 500% slippage, while shown as "0.0200%" it would
    // be a display error by exactly a factor of a hundred.
    let addr = "0x00000000000000000000000000000000000000b5";
    let db = fresh(addr).await;
    say(&db, &format!("/add {addr}")).await;

    let reply = say(&db, &format!("/slippage {addr} 2")).await;
    assert!(
        reply.text.contains("2%"),
        "the reply speaks in percent: {}",
        reply.text
    );
    assert_eq!(
        db.wallets()
            .get(addr)
            .await
            .unwrap()
            .unwrap()
            .max_slippage_pct,
        dec!(0.02),
        "the database holds a fraction, not percent"
    );

    let card = say(&db, &format!("/wallet {addr}")).await;
    assert!(
        card.text.contains("2%"),
        "the card speaks in percent too: {}",
        card.text
    );
    assert!(
        !card.text.contains("0.02%"),
        "and not as a fraction: {}",
        card.text
    );
}

#[tokio::test]
async fn an_absurd_slippage_is_refused() {
    // 500% slippage means "buy at any price": that is not a setting but a typo, and it must
    // not be accepted.
    let addr = "0x00000000000000000000000000000000000000b6";
    let db = fresh(addr).await;
    say(&db, &format!("/add {addr}")).await;

    let reply = say(&db, &format!("/slippage {addr} 500")).await;
    assert!(
        reply.text.contains("100"),
        "the limit is named: {}",
        reply.text
    );
    assert_eq!(
        db.wallets()
            .get(addr)
            .await
            .unwrap()
            .unwrap()
            .max_slippage_pct,
        dec!(0.15),
        "the value stayed as it was"
    );
}

#[tokio::test]
async fn the_numbers_are_readable() {
    // "$1.000000" and "slippage 0.0200%" are not numbers but an internal representation.
    // A schema of its own: the list shows every wallet, including other tests'.
    let addr = "0x00000000000000000000000000000000000000b7";
    let db = garnet_db::testing::isolated_db("tg_numbers").await.unwrap();
    let _ = addr;
    say(&db, &format!("/add {addr}")).await;
    press(
        &db,
        &say(&db, &format!("/stake {addr} 1")).await.buttons[0][0]
            .1
            .clone(),
    )
    .await;

    let list = say(&db, "/wallets").await;
    assert!(
        list.text.contains("$1 "),
        "the stake reads properly: {}",
        list.text
    );
    assert!(
        !list.text.contains("1.000000"),
        "trailing zeros: {}",
        list.text
    );
}

#[tokio::test]
async fn health_says_whether_the_core_is_alive() {
    // Feed lag lives in the trading process and is invisible to the bot; its trace in the
    // database is the age of the equity snapshot, which is written by a timer.
    let db = garnet_db::testing::isolated_db("tg_health").await.unwrap();
    let empty = say(&db, "/health").await;
    assert!(
        empty.text.contains("may not have started") || empty.text.contains("never yet"),
        "a silent core must be visible: {}",
        empty.text
    );

    db.equity()
        .record(Mode::Shadow, dec!(1000), Decimal::ZERO, 0)
        .await
        .unwrap();
    let live = say(&db, "/health").await;
    assert!(live.text.contains("Core last reported"), "{}", live.text);
}

#[tokio::test]
async fn balance_shows_where_the_day_went() {
    let db = garnet_db::testing::isolated_db("tg_balance").await.unwrap();
    db.equity()
        .record(Mode::Live, dec!(100), Decimal::ZERO, 0)
        .await
        .unwrap();
    db.equity()
        .record(Mode::Live, dec!(120), Decimal::ZERO, 0)
        .await
        .unwrap();

    let reply = say(&db, "/balance").await;
    assert!(
        reply.text.contains("24h +$20"),
        "the 24-hour delta: {}",
        reply.text
    );
}

#[tokio::test]
async fn balance_admits_when_the_total_is_incomplete() {
    // A position without a price does not enter the value, and the total is then
    // understated. Measured 2026-09-04: the 24-hour delta showed +12.55 against +12.28
    // realised. Inventing a price is not allowed, and neither is passing an incomplete
    // total off as a complete one.
    let db = garnet_db::testing::isolated_db("tg_unpriced")
        .await
        .unwrap();
    db.equity()
        .record(Mode::Live, dec!(100), Decimal::ZERO, 2)
        .await
        .unwrap();

    let reply = say(&db, "/balance").await;
    assert!(
        reply.text.contains("without a price"),
        "the incompleteness of the total must be visible: {}",
        reply.text
    );
    assert!(
        reply.text.contains('2'),
        "and how many exactly: {}",
        reply.text
    );
}

#[tokio::test]
async fn the_app_command_opens_the_mini_app() {
    // A mini-app opens only from a `web_app` button: a link in the text opens the browser,
    // and the browser does not provide `initData` — so the dashboard honestly refuses.
    let db = garnet_db::testing::isolated_db("tg_app").await.unwrap();
    garnet_telegram::set_dashboard_url("https://garnet.example/");
    let reply = say(&db, "/app").await;
    let (_, data) = &reply.buttons[0][0];
    assert!(
        data.starts_with("webapp:https://"),
        "the button carries the address: {data}"
    );
}

#[tokio::test]
async fn a_change_made_from_the_console_says_so() {
    // The `--say` mode takes the same path as the chat, but its author differs: signing a
    // console edit as "telegram:0" would record a chat that does not exist in the audit
    // trail.
    let db = garnet_db::testing::isolated_db("tg_cli_actor")
        .await
        .unwrap();
    let addr = "0x00000000000000000000000000000000000000c1";
    db.wallets().add(addr, None).await.unwrap();
    handle(
        &db,
        &Incoming::Message {
            chat_id: 0,
            text: format!("/slippage {addr} 2"),
        },
    )
    .await
    .unwrap();

    let actor: String = sqlx::query_scalar(
        "SELECT actor FROM wallet_events WHERE wallet = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(addr)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(actor, "cli");
}

/// The ceiling on one Bot API message is 4096 **characters**.
const TELEGRAM_LIMIT: usize = 4096;

/// A wallet with `n` open positions: that is how many there were in production.
async fn wallet_with_positions(addr: &str, n: usize) -> Db {
    // A schema of its own: `fresh` cleans one wallet, while neighbouring tests' positions
    // remain and break any count.
    let db = garnet_db::testing::isolated_db("tg_positions")
        .await
        .unwrap();
    db.wallets().add(addr, Some("whale-1")).await.unwrap();
    for i in 0..n {
        db.positions()
            .apply_buy(
                addr,
                &format!("tok_{i}"),
                Mode::Shadow,
                dec!(50),
                dec!(25),
                dec!(0.3),
            )
            .await
            .unwrap();
    }
    db
}

#[tokio::test]
async fn positions_fit_in_one_telegram_message() {
    // 05.09.2026: 201 open positions produced 13,607 bytes, the Bot API rejected the whole
    // message, and `/positions` looked like a command that did not work.
    let addr = "0x00000000000000000000000000000000000000f1";
    let db = wallet_with_positions(addr, 300).await;

    let reply = say(&db, "/positions").await;

    assert!(
        reply.text.chars().count() <= TELEGRAM_LIMIT,
        "Telegram will not accept a reply {} characters long",
        reply.text.chars().count()
    );
}

#[tokio::test]
async fn positions_summarise_before_listing() {
    let addr = "0x00000000000000000000000000000000000000f2";
    let db = wallet_with_positions(addr, 40).await;

    let reply = say(&db, "/positions").await;

    assert!(
        reply.text.contains("40"),
        "the total number of positions is named: {}",
        reply.text
    );
    assert!(
        reply.text.contains("whale-1"),
        "the per-wallet total: {}",
        reply.text
    );
    assert!(
        reply.text.contains("more"),
        "the remainder of the list is named: {}",
        reply.text
    );
}

#[tokio::test]
async fn an_empty_book_says_so_instead_of_printing_nothing() {
    let db = garnet_db::testing::isolated_db("tg_empty").await.unwrap();

    let reply = say(&db, "/positions").await;
    assert!(
        reply.text.contains("There are no open positions"),
        "{}",
        reply.text
    );
}

#[tokio::test]
async fn every_long_reply_is_clamped() {
    // One reason for refusal across every list: the Bot API does not trim a long reply, it
    // rejects it, and the command looks broken.
    let addr = "0x00000000000000000000000000000000000000f4";
    let db = wallet_with_positions(addr, 300).await;

    for cmd in [
        "/positions",
        "/positions all",
        "/signals 500",
        "/wallets",
        "/pnl all",
    ] {
        let reply = say(&db, cmd).await;
        assert!(
            reply.text.chars().count() <= TELEGRAM_LIMIT,
            "{cmd}: {} characters",
            reply.text.chars().count()
        );
    }
}

#[tokio::test]
async fn balance_names_the_money_in_words() {
    let db = garnet_db::testing::isolated_db("tg_words").await.unwrap();
    db.equity()
        .record(Mode::Shadow, dec!(-12577.75), dec!(9560), 15)
        .await
        .unwrap();

    let reply = say(&db, "/balance").await;

    assert!(
        reply.text.contains("Paper") || reply.text.contains("paper"),
        "{}",
        reply.text
    );
    assert!(
        !reply.text.contains("shadow"),
        "the jargon is gone: {}",
        reply.text
    );
}

#[tokio::test]
async fn balance_does_not_pass_an_idle_live_account_off_as_a_portfolio() {
    // The operator saw a LIVE line and concluded the bot was trading real money. There are
    // no live wallets — and that is what has to be said.
    let addr = "0x00000000000000000000000000000000000000f6";
    let db = garnet_db::testing::isolated_db("tg_idle_live")
        .await
        .unwrap();
    db.wallets().add(addr, Some("whale-1")).await.unwrap();

    let reply = say(&db, "/balance").await;

    assert!(
        reply.text.contains("no live wallets") || reply.text.contains("There are no live wallets"),
        "in plain words: {}",
        reply.text
    );
}

#[tokio::test]
async fn an_exit_signal_is_not_shown_as_a_zero_dollar_bet() {
    // An exit has no stake size: `signals.target_size_usd` is zero there, and "for $0" reads
    // as an arithmetic error rather than as a sale following the leader.
    let db = garnet_db::testing::isolated_db("tg_exit_signal")
        .await
        .unwrap();
    db.wallets().add("0xexit1", Some("whale-E")).await.unwrap();
    let trade = db
        .trades()
        .record(&garnet_db::NewLeaderTrade {
            wallet: "0xexit1".into(),
            tx_hash: "0xtxexit".into(),
            token_id: "tok_exit".into(),
            side: garnet_db::Side::Sell,
            price: dec!(0.42),
            size: dec!(60),
            ts_trade: chrono::Utc::now(),
            source: garnet_db::Source::Rtds,
            market_text: "Lakers vs Boston".into(),
            outcome_text: "Lakers to win".into(),
        })
        .await
        .unwrap()
        .fresh()
        .expect("the trade was recorded");
    db.signals()
        .record(
            trade.id,
            "0xexit1",
            Mode::Shadow,
            "copy",
            Decimal::ZERO,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let reply = say(&db, "/signals 5").await;

    assert!(
        !reply.text.contains("for $0"),
        "an exit is not a zero stake: {}",
        reply.text
    );
    assert!(
        reply.text.contains("Exited"),
        "the sale has to be named: {}",
        reply.text
    );
}

/// Health names the circuits separately.
///
/// On 06.09.2026, on a cold start, it printed "ok" while the socket brought nothing for six
/// minutes: a fresh safety-net poll counted as flow in general. The poll runs every few
/// seconds and always looks alive.
#[tokio::test]
async fn health_separates_the_socket_from_the_safety_poll() {
    let db = garnet_db::testing::isolated_db("tg_health2").await.unwrap();
    db.wallets().add("0xh1", None).await.unwrap();
    db.trades()
        .record(&garnet_db::NewLeaderTrade {
            wallet: "0xh1".into(),
            tx_hash: "0xh".into(),
            token_id: "tok_h".into(),
            side: garnet_db::Side::Buy,
            price: dec!(0.4),
            size: dec!(100),
            ts_trade: chrono::Utc::now(),
            source: garnet_db::Source::Poll,
            market_text: "M".into(),
            outcome_text: "O".into(),
        })
        .await
        .unwrap();

    let reply = say(&db, "/health").await;
    assert!(
        reply.text.contains("Socket brought a trade: none yet"),
        "{}",
        reply.text
    );
    assert!(reply.text.contains("Safety-net poll:"), "{}", reply.text);
    assert!(
        !reply.text.contains("Last leader trade"),
        "one counter for two circuits was precisely the defect: {}",
        reply.text
    );
}

/// The copy-quality report is read by eye, and its chief risk is showing a zero where there
/// is nothing to measure. Both cases are checked by the words the operator sees rather than
/// by the fields of a struct.
mod matchup {
    use super::*;
    use garnet_db::{NewLeaderTrade, Source};
    use garnet_types::Side;

    async fn iso(tag: &str) -> Db {
        garnet_db::testing::isolated_db(tag).await.unwrap()
    }

    #[tokio::test]
    async fn an_empty_report_says_so_in_words_and_not_in_zeros() {
        let db = iso("tg_mu_empty").await;
        let r = say(&db, "/matchup all").await;
        assert!(
            r.text.contains("Nothing to compare"),
            "an empty report must say so in words rather than show zeros: {}",
            r.text
        );
        assert!(!r.text.contains("0¢"), "zero cents cannot appear here");
    }

    #[tokio::test]
    async fn the_entry_difference_is_named_dearer_or_cheaper_not_signed() {
        let db = iso("tg_mu_words").await;
        let addr = "0x00000000000000000000000000000000000000c1";
        db.wallets().add(addr, Some("whale")).await.unwrap();
        db.trades()
            .record(&NewLeaderTrade {
                wallet: addr.into(),
                tx_hash: "0xtx_c1".into(),
                token_id: "tok_c1".into(),
                side: Side::Buy,
                price: dec!(0.40),
                size: dec!(100),
                ts_trade: chrono::Utc::now(),
                source: Source::Rtds,
                market_text: "Who will win".into(),
                outcome_text: "Up".into(),
            })
            .await
            .unwrap();
        // We entered at 0.50 — ten cents more expensive than the leader.
        db.positions()
            .apply_buy(addr, "tok_c1", Mode::Shadow, dec!(10), dec!(5), dec!(0))
            .await
            .unwrap();

        let r = say(&db, "/matchup all").await;
        assert!(
            r.text.contains("10¢ more expensive"),
            "the direction is named in words; the sign \"+10\" reads the other way: {}",
            r.text
        );
        // The position is still open: there is no return gap, and inventing one is not
        // allowed.
        assert!(
            r.text.contains("no closed positions yet"),
            "a gap is not computed without closed positions: {}",
            r.text
        );
    }
}

/// A leader merge we did not exit on is an alarm: they left the position while we stayed in
/// it (invariant 40).
#[tokio::test]
async fn health_names_a_leader_merge_we_did_not_follow() {
    let db = garnet_db::testing::isolated_db("tg_stuck_merge")
        .await
        .unwrap();
    let addr = "0x00000000000000000000000000000000000000e1";
    db.wallets().add(addr, None).await.unwrap();

    let quiet = say(&db, "/health").await;
    assert!(
        !quiet.text.contains("Leader merges"),
        "quiet while there is nothing to say"
    );

    db.actions()
        .insert_new(&garnet_db::NewLeaderAction {
            wallet: addr.into(),
            tx_hash: "0xmerge1".into(),
            condition_id: "0xcond1".into(),
            kind: "merge".into(),
            size: dec!(10),
            ts_action: chrono::Utc::now() - chrono::Duration::hours(3),
            source: garnet_db::Source::Rtds,
        })
        .await
        .unwrap();

    let loud = say(&db, "/health").await;
    assert!(
        loud.text.contains("Leader merges with no exit of ours: 1"),
        "an unhandled merge must be visible: {}",
        loud.text
    );
}

/// The circuit report is read to decide the fate of the safety-net poll. A circuit must not
/// be blamed for its place in the alphabet: when nobody has a unique trade, that means "they
/// duplicate each other", not "switch both off".
mod sources {
    use super::*;
    use garnet_db::{NewLeaderTrade, Source};
    use garnet_types::Side;

    fn mk(tx: &str, source: Source) -> NewLeaderTrade {
        NewLeaderTrade {
            wallet: "0xsrc".into(),
            tx_hash: tx.into(),
            token_id: "tok_a".into(),
            side: Side::Buy,
            price: dec!(0.42),
            size: dec!(100),
            ts_trade: chrono::Utc::now(),
            source,
            market_text: "m".into(),
            outcome_text: "o".into(),
        }
    }

    async fn seeded(tag: &str) -> Db {
        let db = garnet_db::testing::isolated_db(tag).await.unwrap();
        db.wallets().add("0xsrc", None).await.unwrap();
        db
    }

    #[tokio::test]
    async fn an_empty_report_says_so_instead_of_showing_zeros() {
        let db = seeded("tg_src_empty").await;
        let r = say(&db, "/sources all").await;
        assert!(r.text.contains("There are no sightings yet"), "{}", r.text);
    }

    #[tokio::test]
    async fn when_no_circuit_is_unique_it_does_not_blame_just_one() {
        let db = seeded("tg_src_tie").await;
        for tx in ["0x1", "0x2"] {
            db.trades().record(&mk(tx, Source::Rtds)).await.unwrap();
            db.trades().record(&mk(tx, Source::Poll)).await.unwrap();
        }

        let r = say(&db, "/sources all").await;
        assert!(
            r.text.contains("see the same thing"),
            "both circuits duplicate each other — blaming one is not allowed: {}",
            r.text
        );
        assert!(
            !r.text.contains("brought no trade"),
            "a personal accusation is wrong here: {}",
            r.text
        );
    }

    #[tokio::test]
    async fn a_circuit_that_earns_its_keep_is_not_accused() {
        let db = seeded("tg_src_earns").await;
        db.trades().record(&mk("0x1", Source::Rtds)).await.unwrap();
        db.trades().record(&mk("0x1", Source::Poll)).await.unwrap();
        // This one was brought by the poll alone — so it catches what the socket loses.
        db.trades().record(&mk("0x2", Source::Poll)).await.unwrap();

        let r = say(&db, "/sources all").await;
        assert!(
            r.text.contains("Socket brought no trade"),
            "the socket has no unique trades — that has to be said: {}",
            r.text
        );
        assert!(
            !r.text.contains("Safety-net poll brought no"),
            "the poll brought a unique one and is not subject to accusation: {}",
            r.text
        );
    }
}

/// The emergency-close dialogue (invariant 46): a button is pressed by accident, a phrase is
/// not.
mod flatten {
    use super::*;

    async fn iso(tag: &str) -> Db {
        garnet_db::testing::isolated_db(tag).await.unwrap()
    }

    #[tokio::test]
    async fn plain_text_says_nothing_when_no_phrase_is_expected() {
        // Answering every remark means teaching the operator not to read the replies — and
        // to miss the one that matters.
        let db = iso("tg_fl_quiet").await;
        let r = handle(
            &db,
            &Incoming::Message {
                chat_id: CHAT,
                text: "panic garnet".into(),
            },
        )
        .await
        .unwrap();
        assert!(
            r.is_none(),
            "nobody was waiting for a phrase — the bot stays silent"
        );
    }

    #[tokio::test]
    async fn the_declaration_alone_closes_nothing() {
        let db = iso("tg_fl_declare").await;
        let r = say(&db, "/flatten panic").await;
        assert!(
            r.text.contains("panic garnet"),
            "the phrase is named: {}",
            r.text
        );
        assert!(
            db.controls().flatten_approved().await.unwrap().is_none(),
            "a declaration by itself approves nothing"
        );
        assert!(matches!(
            db.controls().flatten_intent().await.unwrap(),
            garnet_db::StoredIntent::Some { .. }
        ));
    }

    #[tokio::test]
    async fn the_right_phrase_approves_and_a_wrong_one_cancels() {
        let db = iso("tg_fl_phrase").await;
        db.controls()
            .set_manual_stop(true, "operator")
            .await
            .unwrap();

        say(&db, "/flatten panic").await;
        let bad = say(&db, "panik garnet").await;
        assert!(bad.text.contains("Refused"), "{}", bad.text);
        assert!(
            db.controls().flatten_intent().await.unwrap() == garnet_db::StoredIntent::None,
            "a wrong phrase clears the intent: a second attempt is a second decision"
        );
        assert!(db.controls().flatten_approved().await.unwrap().is_none());

        say(&db, "/flatten panic").await;
        let ok = say(&db, "panic garnet").await;
        assert!(ok.text.contains("Accepted"), "{}", ok.text);
        let (mode, actor) = db.controls().flatten_approved().await.unwrap().unwrap();
        assert_eq!(mode, "panic");
        assert!(
            actor.contains("telegram"),
            "the trace keeps who decided: {actor}"
        );
    }

    #[tokio::test]
    async fn panic_on_running_trading_is_refused_even_with_the_right_phrase() {
        let db = iso("tg_fl_running").await;
        say(&db, "/flatten panic").await;
        let r = say(&db, "panic garnet").await;
        assert!(r.text.contains("is not stopped"), "{}", r.text);
        assert!(db.controls().flatten_approved().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn hybrid_asks_for_the_forfeit_before_it_asks_for_the_phrase() {
        let db = iso("tg_fl_hybrid").await;

        let ask = say(&db, "/flatten hybrid").await;
        assert!(
            !ask.buttons.is_empty(),
            "the forfeiture of the payout is asked separately"
        );
        assert!(ask.text.contains("forfeiture"), "{}", ask.text);
        assert!(
            db.controls().flatten_intent().await.unwrap() == garnet_db::StoredIntent::None,
            "there is no intent before the forfeiture is confirmed"
        );

        let prompt = press(&db, &ask.buttons[0][0].1.clone()).await;
        assert!(prompt.text.contains("hybrid garnet"), "{}", prompt.text);

        // The button records consent, but the phrase still does the closing.
        assert!(db.controls().flatten_approved().await.unwrap().is_none());
        let done = say(&db, "hybrid garnet").await;
        assert!(done.text.contains("Accepted"), "{}", done.text);
        assert_eq!(
            db.controls().flatten_approved().await.unwrap().unwrap().0,
            "hybrid"
        );
    }

    #[tokio::test]
    async fn an_expired_intent_is_refused() {
        let db = iso("tg_fl_expired").await;
        db.controls()
            .set_manual_stop(true, "operator")
            .await
            .unwrap();
        db.controls()
            .set_flatten_intent(
                "panic",
                chrono::Utc::now() - chrono::Duration::seconds(120),
                false,
                "operator",
            )
            .await
            .unwrap();

        let r = say(&db, "panic garnet").await;
        assert!(r.text.contains("has expired"), "{}", r.text);
        assert!(db.controls().flatten_approved().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_corrupt_intent_closes_the_gate_instead_of_opening_it() {
        let db = iso("tg_fl_corrupt").await;
        sqlx::query(
            "INSERT INTO controls (key, value, actor) VALUES ('flatten_intent', 'junk', 'test')",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let r = say(&db, "panic garnet").await.text;
        assert!(r.contains("unreadable"), "{r}");
        assert!(db.controls().flatten_approved().await.unwrap().is_none());
    }
}

/// The summary `fire_limit` is set from (invariant 44).
mod firerate {
    use super::*;
    use chrono::{TimeZone, Utc};
    use garnet_db::{NewLeaderTrade, Source};
    use garnet_types::Side;

    async fn copy_at(db: &Db, wallet: &str, secs: i64, tag: &str) {
        let t = db
            .trades()
            .record(&NewLeaderTrade {
                wallet: wallet.into(),
                tx_hash: format!("0x{tag}"),
                token_id: "tok_a".into(),
                side: Side::Buy,
                price: dec!(0.5),
                size: dec!(1),
                ts_trade: Utc::now(),
                source: Source::Rtds,
                market_text: "m".into(),
                outcome_text: "o".into(),
            })
            .await
            .unwrap()
            .fresh()
            .unwrap();
        let s = db
            .signals()
            .record(t.id, wallet, Mode::Live, "copy", dec!(10), None, None, None)
            .await
            .unwrap();
        sqlx::query("UPDATE signals SET ts_signal = $1 WHERE id = $2")
            .bind(Utc.timestamp_opt(1_789_000_000 + secs, 0).unwrap())
            .bind(s.id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn without_copies_it_says_so_instead_of_showing_zeros() {
        let db = garnet_db::testing::isolated_db("tg_fr_empty")
            .await
            .unwrap();
        let r = say(&db, "/firerate").await;
        assert!(r.text.contains("There are no copies yet"), "{}", r.text);
        assert!(
            !r.text.contains("refused"),
            "there is no counterfactual without data"
        );
    }

    #[tokio::test]
    async fn the_counterfactual_tells_the_operator_what_each_limit_would_cost() {
        // Without this there is nothing to set the threshold from: the distribution
        // describes, while the answer to "how much would it have refused" decides.
        let db = garnet_db::testing::isolated_db("tg_fr_cf").await.unwrap();
        db.wallets().add("0xw1", None).await.unwrap();
        for g in 1..=9i64 {
            copy_at(&db, "0xw1", g * 3, &format!("c{g}")).await;
        }

        let r = say(&db, "/firerate").await;
        assert!(r.text.contains("longest 9"), "{}", r.text);
        // Nine copies in the window: a limit of 1 refuses eight, a limit of 10 refuses none.
        assert!(r.text.contains("1 → refused 8"), "{}", r.text);
        assert!(r.text.contains("10 → refused 0"), "{}", r.text);
    }

    #[tokio::test]
    async fn a_skip_reason_reads_as_words_not_as_a_label() {
        let db = garnet_db::testing::isolated_db("tg_fr_words")
            .await
            .unwrap();
        db.wallets().add("0xw1", None).await.unwrap();
        let t = db
            .trades()
            .record(&NewLeaderTrade {
                wallet: "0xw1".into(),
                tx_hash: "0xrl".into(),
                token_id: "tok_a".into(),
                side: Side::Buy,
                price: dec!(0.5),
                size: dec!(1),
                ts_trade: Utc::now(),
                source: Source::Rtds,
                market_text: "m".into(),
                outcome_text: "o".into(),
            })
            .await
            .unwrap()
            .fresh()
            .unwrap();
        db.signals()
            .record(
                t.id,
                "0xw1",
                Mode::Live,
                "skip:rate_limited",
                Decimal::ZERO,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let r = say(&db, "/signals").await;
        assert!(
            r.text.contains("firing a burst"),
            "the seventh reason is readable: {}",
            r.text
        );
    }
}

/// The third circuit is counted separately (invariant 31): a circuit whose flow is added to
/// another's can be neither checked nor switched off.
#[tokio::test]
async fn health_counts_the_chain_circuit_apart_and_stays_quiet_until_it_speaks() {
    use garnet_db::{NewLeaderTrade, Source};
    use garnet_types::Side;

    let db = garnet_db::testing::isolated_db("tg_chain_health")
        .await
        .unwrap();
    db.wallets().add("0xch", None).await.unwrap();

    let quiet = say(&db, "/health").await;
    assert!(
        !quiet.text.contains("Chain logs"),
        "a disabled circuit must not look broken: {}",
        quiet.text
    );

    let mk = |tx: &str, src| NewLeaderTrade {
        wallet: "0xch".into(),
        tx_hash: tx.into(),
        token_id: "tok_a".into(),
        side: Side::Buy,
        price: dec!(0.5),
        size: dec!(1),
        ts_trade: chrono::Utc::now(),
        source: src,
        market_text: "m".into(),
        outcome_text: "o".into(),
    };
    db.trades()
        .record(&mk("0xrtds", Source::Rtds))
        .await
        .unwrap();
    db.trades()
        .record(&mk("0xchain", Source::Chain))
        .await
        .unwrap();

    let loud = say(&db, "/health").await;
    assert!(loud.text.contains("Chain logs"), "{}", loud.text);
    assert!(
        loud.text.contains("Socket brought a trade"),
        "the first two did not go anywhere"
    );
    assert!(loud.text.contains("Safety-net poll"));
}
