//! Parsing Telegram updates and commands.
//!
//! Everything checked here is a pure function: an update arrives as a structure we do not
//! choose, it is easy to get wrong, and no network is needed for it.

use garnet_telegram::update::{parse_command, parse_update, Command, Incoming};
use garnet_types::Mode;
use rust_decimal_macros::dec;

fn message(chat_id: i64, text: &str) -> serde_json::Value {
    serde_json::json!({
        "update_id": 1,
        "message": {
            "message_id": 7,
            "chat": { "id": chat_id, "type": "private" },
            "from": { "id": chat_id, "is_bot": false },
            "text": text
        }
    })
}

#[test]
fn a_message_becomes_a_command() {
    let got = parse_update(&message(42, "/wallets")).expect("the message was parsed");
    assert_eq!(
        got,
        Incoming::Message {
            chat_id: 42,
            text: "/wallets".into()
        }
    );
}

#[test]
fn a_button_press_carries_what_to_confirm() {
    // A button arrives in a different field and requires an `answerCallbackQuery` reply,
    // otherwise the client shows a spinner until it times out.
    let update = serde_json::json!({
        "update_id": 2,
        "callback_query": {
            "id": "cb-1",
            "from": { "id": 42 },
            "message": { "message_id": 7, "chat": { "id": 42 } },
            "data": "do:mode:0x1234:live"
        }
    });
    assert_eq!(
        parse_update(&update).unwrap(),
        Incoming::Button {
            chat_id: 42,
            message_id: 7,
            query_id: "cb-1".into(),
            data: "do:mode:0x1234:live".into(),
        }
    );
}

#[test]
fn an_update_without_a_chat_is_ignored() {
    // Telegram also sends what is none of our business: message edits, channel posts, poll
    // answers. Panicking on them is not allowed — the loop would stall.
    assert!(parse_update(&serde_json::json!({ "update_id": 3 })).is_none());
    assert!(parse_update(&serde_json::json!({
        "update_id": 4,
        "edited_message": { "chat": { "id": 42 }, "text": "/kill" }
    }))
    .is_none());
}

#[test]
fn a_command_addressed_to_the_bot_by_name_still_parses() {
    // In a group the client appends `@bot_name` to every command.
    assert_eq!(parse_command("/wallets@garnet_bot"), Command::Wallets);
}

#[test]
fn adding_a_wallet_takes_an_optional_nickname() {
    assert_eq!(
        parse_command("/add 0x00000000000000000000000000000000000000ab whale-1"),
        Command::Add {
            address: "0x00000000000000000000000000000000000000ab".into(),
            nickname: Some("whale-1".into()),
        }
    );
    assert_eq!(
        parse_command("/add 0x00000000000000000000000000000000000000ab"),
        Command::Add {
            address: "0x00000000000000000000000000000000000000ab".into(),
            nickname: None,
        }
    );
}

#[test]
fn an_address_that_is_not_an_address_is_refused_before_the_database() {
    // A junk address in the registry means a wallet that will never match an RTDS frame:
    // the bot stays silent while the operator believes it is copying.
    assert!(matches!(
        parse_command("/add 0xnope"),
        Command::Malformed(_)
    ));
    assert!(matches!(
        parse_command("/add leader"),
        Command::Malformed(_)
    ));
}

#[test]
fn the_address_case_does_not_matter() {
    // An RTDS frame brings the address in lower case, while a human copies it from an
    // explorer in checksum form. The comparison would miss.
    let Command::Add { address, .. } =
        parse_command("/add 0x00000000000000000000000000000000000000AB")
    else {
        panic!("it must parse as /add");
    };
    assert_eq!(address, "0x00000000000000000000000000000000000000ab");
}

#[test]
fn numbers_come_with_their_command() {
    assert_eq!(
        parse_command("/stake 0x00000000000000000000000000000000000000ab 25"),
        Command::Stake {
            address: "0x00000000000000000000000000000000000000ab".into(),
            usd: dec!(25),
        }
    );
    assert_eq!(
        parse_command("/slippage 0x00000000000000000000000000000000000000ab 5.5"),
        Command::Slippage {
            address: "0x00000000000000000000000000000000000000ab".into(),
            pct: dec!(5.5),
        }
    );
    assert_eq!(
        parse_command("/mode 0x00000000000000000000000000000000000000ab live"),
        Command::Mode {
            address: "0x00000000000000000000000000000000000000ab".into(),
            mode: Mode::Live,
        }
    );
    assert!(matches!(
        parse_command("/stake 0x00000000000000000000000000000000000000ab lots"),
        Command::Malformed(_)
    ));
}

#[test]
fn a_negative_stake_is_malformed() {
    // A negative stake would reach the database and would mean an order with an inverted
    // sign somewhere nobody expects one.
    assert!(matches!(
        parse_command("/stake 0x00000000000000000000000000000000000000ab -5"),
        Command::Malformed(_)
    ));
    assert!(matches!(
        parse_command("/slippage 0x00000000000000000000000000000000000000ab -1"),
        Command::Malformed(_)
    ));
}

#[test]
fn plain_text_is_carried_but_is_still_not_a_command() {
    // Since 19.09.2026 plain text reaches the handler: it means exactly one thing — the
    // emergency close's confirmation phrase (invariant 46). That did not make it a command:
    // without an expected phrase the bot stays silent about it, and that is checked in the
    // `commands` tests.
    assert_eq!(parse_command("hello"), Command::Plain("hello".into()));
    // An empty message is not text: there is nothing to reply to it with.
    assert_eq!(parse_command("   "), Command::None);
    assert!(matches!(parse_command("/jig"), Command::Unknown(_)));
}

#[test]
fn the_app_command_is_recognised() {
    assert_eq!(parse_command("/app"), Command::App);
}

#[test]
fn the_matchup_period_defaults_to_a_day_and_refuses_nonsense() {
    use garnet_telegram::update::Period;
    assert_eq!(
        parse_command("/matchup"),
        Command::Matchup {
            period: Period::Day
        }
    );
    assert_eq!(
        parse_command("/matchup week"),
        Command::Matchup {
            period: Period::Week
        }
    );
    assert_eq!(
        parse_command("/matchup all"),
        Command::Matchup {
            period: Period::All
        }
    );
    // A typo must not silently turn into "24 hours": the period changes the answer here, and
    // substituting a default for it means showing the wrong report.
    assert!(matches!(
        parse_command("/matchup yesterday"),
        Command::Malformed(_)
    ));
}

#[test]
fn the_sources_period_defaults_to_a_day_and_refuses_nonsense() {
    use garnet_telegram::update::Period;
    assert_eq!(
        parse_command("/sources"),
        Command::Sources {
            period: Period::Day
        }
    );
    assert_eq!(
        parse_command("/sources week"),
        Command::Sources {
            period: Period::Week
        }
    );
    assert_eq!(
        parse_command("/sources all"),
        Command::Sources {
            period: Period::All
        }
    );
    assert!(matches!(
        parse_command("/sources everything"),
        Command::Malformed(_)
    ));
}

#[test]
fn the_firerate_command_takes_no_arguments() {
    assert_eq!(parse_command("/firerate"), Command::FireRate);
}

#[test]
fn the_latency_period_defaults_to_a_day_and_refuses_nonsense() {
    use garnet_telegram::update::Period;
    assert_eq!(
        parse_command("/latency"),
        Command::Latency {
            period: Period::Day
        }
    );
    assert_eq!(
        parse_command("/latency week"),
        Command::Latency {
            period: Period::Week
        }
    );
    assert!(matches!(
        parse_command("/latency fast"),
        Command::Malformed(_)
    ));
}
