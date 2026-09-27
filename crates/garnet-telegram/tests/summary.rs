//! The daily summary: when to send it and what goes in it.

use chrono::{TimeZone as _, Utc};
use garnet_telegram::summary::{next_run, parse_at};

#[test]
fn the_next_run_is_today_when_the_hour_is_still_ahead() {
    let now = Utc.with_ymd_and_hms(2026, 9, 4, 8, 0, 0).unwrap();
    let at = next_run(now, 21, 0);
    assert_eq!(at, Utc.with_ymd_and_hms(2026, 9, 4, 21, 0, 0).unwrap());
}

#[test]
fn a_missed_hour_moves_to_tomorrow_instead_of_firing_at_once() {
    // Otherwise a restart of the process after the appointed hour sends the summary again,
    // and every restart means one more.
    let now = Utc.with_ymd_and_hms(2026, 9, 4, 22, 30, 0).unwrap();
    let at = next_run(now, 21, 0);
    assert_eq!(at, Utc.with_ymd_and_hms(2026, 9, 5, 21, 0, 0).unwrap());
}

#[test]
fn the_exact_minute_counts_as_passed() {
    let now = Utc.with_ymd_and_hms(2026, 9, 4, 21, 0, 0).unwrap();
    assert_eq!(
        next_run(now, 21, 0),
        Utc.with_ymd_and_hms(2026, 9, 5, 21, 0, 0).unwrap()
    );
}

#[test]
fn an_empty_setting_means_no_summary() {
    // Disabling the summary means clearing the string, not hunting for a flag.
    assert_eq!(parse_at(""), None);
    assert_eq!(parse_at("  "), None);
}

#[test]
fn the_hour_is_read_the_way_people_write_it() {
    assert_eq!(parse_at("21:00"), Some((21, 0)));
    assert_eq!(parse_at("07:45"), Some((7, 45)));
    assert_eq!(parse_at("7:45"), Some((7, 45)));
}

#[test]
fn a_broken_hour_is_refused_rather_than_guessed() {
    // "25:00" with a silent shift to one in the morning would mean a summary arriving when
    // it is not expected.
    assert_eq!(parse_at("25:00"), None);
    assert_eq!(parse_at("21:60"), None);
    assert_eq!(parse_at("in the evening"), None);
}
