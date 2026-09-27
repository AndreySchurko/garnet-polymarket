//! Who is entitled to control the bot.

use garnet_telegram::guard::allowed;

#[test]
fn only_listed_chats_are_answered() {
    assert!(allowed(&[42, 7], 42));
    assert!(!allowed(&[42, 7], 43));
}

#[test]
fn an_empty_whitelist_answers_nobody() {
    // An empty list means "nobody", not "everybody". The opposite default would mean that a
    // forgotten line of config opens control over money to anyone who found the bot by
    // searching.
    assert!(!allowed(&[], 42));
}

use garnet_telegram::guard::owners;

#[test]
fn the_environment_overrides_the_config() {
    // The operator keeps the ids where the token is — in `.env`, which is not in the
    // repository. The config stays the default for anyone who did not set them.
    assert_eq!(owners(&[1, 2], Some("6218292348")), vec![6_218_292_348]);
    assert_eq!(owners(&[1, 2], None), vec![1, 2]);
    assert_eq!(
        owners(&[1, 2], Some("")),
        vec![1, 2],
        "an empty variable is not a list"
    );
}

#[test]
fn a_list_may_be_written_the_way_people_write_lists() {
    assert_eq!(owners(&[], Some("42, 7 ,9")), vec![42, 7, 9]);
}

#[test]
fn garbage_in_the_list_is_dropped_not_fatal() {
    // A typo in one id must not deprive the operator of the controls entirely.
    assert_eq!(owners(&[], Some("42,oops,7")), vec![42, 7]);
}
