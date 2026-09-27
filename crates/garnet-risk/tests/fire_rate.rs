//! The entry rate limit per wallet (invariant 44).
//!
//! The test is named after what it guards: the bucket "11+ entries per wallet at
//! once" held half the capital and 94% of the entire loss, and neither the slice
//! window nor the exposure ceiling catches it.

use garnet_risk::fire_rate::FireRate;

const W: i64 = 60;

#[test]
fn the_measured_normal_behaviour_never_trips() {
    // 88.4% of copies arrive at depth 1 — that is, alone. A limiter that fires on
    // those would be refusing almost always.
    let fr = FireRate::new(W, 4);
    for t in 0..20 {
        // One entry every two minutes: the window between them empties out.
        assert!(
            !fr.record("0xa", t * 120),
            "a lone entry must not be refused"
        );
    }
}

#[test]
fn the_2026_09_06_burst_of_twelve_is_caught() {
    // The leader fired twelve decisions within a minute across twelve markets.
    // The slice window does not trim them (different markets), the ceiling does
    // not catch them (different events).
    let fr = FireRate::new(W, 4);
    let mut taken = 0;
    for i in 0..12 {
        if !fr.record("0xa", 1_000 + i) {
            taken += 1;
        }
    }
    assert_eq!(taken, 4, "we take exactly the limit and refuse the rest");
    assert_eq!(fr.count("0xa", 1_011), 4, "refusals do not enter the queue");
}

#[test]
fn a_refused_entry_does_not_lock_the_wallet_for_a_whole_window() {
    // A refused entry spends no capital. Were we to record it in the queue, one
    // burst would hold the wallet shut for a whole window, counting its own
    // refusals as expenditure.
    let fr = FireRate::new(W, 2);
    assert!(!fr.record("0xa", 0));
    assert!(!fr.record("0xa", 1));
    assert!(fr.record("0xa", 2), "at the limit");
    assert!(fr.record("0xa", 3), "still at the limit");

    // The window is exclusive: the entry at 0 leaves exactly at 60, the entry at
    // 1 at 61. At 60 there is exactly one slot free.
    assert_eq!(
        fr.count("0xa", 60),
        1,
        "the first left, the second is still inside"
    );
    assert!(!fr.record("0xa", 60), "the freed slot is taken");
    assert!(
        fr.record("0xa", 60),
        "and immediately at the limit again: there was one slot"
    );
}

#[test]
fn a_steady_pace_inside_the_limit_never_trips() {
    // The window rolls. A wallet entering exactly at the pace limit must keep
    // working.
    let fr = FireRate::new(W, 3);
    // Three entries per window, again and again: one every 20 seconds.
    for i in 0..30 {
        let t = i * 21;
        assert!(
            !fr.record("0xa", t),
            "a steady pace must not be refused (i={i})"
        );
    }
}

#[test]
fn wallets_are_independent() {
    // What is limited is the rate of spending per WALLET: one leader's queue must
    // not shut the door on entries behind another.
    let fr = FireRate::new(W, 2);
    assert!(!fr.record("0xa", 0));
    assert!(!fr.record("0xa", 1));
    assert!(fr.record("0xa", 2), "the first exhausted its limit");

    assert!(
        !fr.record("0xb", 2),
        "the second was not the one exhausting it"
    );
    assert!(!fr.record("0xb", 3));
    assert!(fr.record("0xb", 4));
}

#[test]
fn the_window_edge_is_exclusive() {
    // An entry exactly `window` ago is already outside. An inclusive boundary
    // would make the window a second longer than advertised, and a threshold set
    // from a measurement would then mean something other than what was measured.
    let fr = FireRate::new(W, 1);
    assert!(!fr.record("0xa", 100));
    assert!(
        fr.record("0xa", 159),
        "a second before the edge — inside the window"
    );

    let fr2 = FireRate::new(W, 1);
    assert!(!fr2.record("0xa", 100));
    assert!(
        !fr2.record("0xa", 160),
        "exactly on the edge — already outside"
    );
}

#[test]
fn zero_disables_the_refusal_but_not_the_counting() {
    // A threshold is assigned from a measurement, not from a guess (invariant 33),
    // and the queue depth has to be observed BEFORE the refusal is switched on. A
    // disabled limiter that counts nothing leaves the operator exactly where they
    // were — with a threshold there is nowhere to get.
    let fr = FireRate::new(W, 0);
    assert!(fr.disabled());
    for i in 0..12 {
        assert!(
            !fr.record("0xa", 1_000 + i),
            "a disabled limiter never refuses"
        );
    }
    assert_eq!(fr.count("0xa", 1_011), 12, "but it counts");
}

#[test]
fn a_zero_window_disables_it_too() {
    // A window of zero length means a refusal on every second signal within the
    // same second. Such a setting is almost certainly a typo, and it is obliged to
    // behave as disabled rather than as the harshest setting possible.
    let fr = FireRate::new(0, 5);
    assert!(fr.disabled());
    for i in 0..10 {
        assert!(!fr.record("0xa", 1_000 + i));
    }
}

/// The counterfactual for a threshold: how much a limit would have refused over
/// history that has already happened.
mod counterfactual {
    use super::*;
    use garnet_risk::fire_rate::would_refuse;

    fn burst(n: i64) -> Vec<(String, i64)> {
        (0..n).map(|i| ("0xa".to_string(), 1_000 + i)).collect()
    }

    #[test]
    fn a_replay_is_not_the_same_as_counting_depths() {
        // The naive count of "depth greater than the limit" overstates refusals: a
        // refused entry does not enter the queue and does NOT deepen the ones that
        // follow. Here that is visible as a number.
        //
        // A queue of six consecutive entries, limit 2. By depths, "refused" would
        // be four (depths 3,4,5,6). The replay also gives four — because the queue
        // never empties. It is when it does empty that the answers diverge.
        assert_eq!(would_refuse(&burst(6), W, 2), 4);

        // Entries at 0,1,2 and then at 61,62,63 — with a window of 60 the second
        // triple lands in an already empty window. By depths computed without
        // accounting for refusals, the second triple would look like a
        // continuation of the first.
        let split: Vec<(String, i64)> = [0, 1, 2, 61, 62, 63]
            .iter()
            .map(|t| ("0xa".to_string(), *t))
            .collect();
        assert_eq!(would_refuse(&split, W, 2), 2, "one refusal in each triple");
    }

    #[test]
    fn a_limit_above_the_longest_queue_refuses_nothing() {
        assert_eq!(would_refuse(&burst(9), W, 10), 0);
        assert_eq!(
            would_refuse(&burst(9), W, 9),
            0,
            "a limit on the boundary takes everything"
        );
        assert_eq!(would_refuse(&burst(9), W, 8), 1);
    }

    #[test]
    fn zero_refuses_nothing_because_zero_is_off() {
        // Otherwise the counterfactual would show "zero refuses everything", and
        // the operator would read the disabled state as the harshest one.
        assert_eq!(would_refuse(&burst(9), W, 0), 0);
    }

    #[test]
    fn wallets_do_not_share_a_limit_in_the_replay_either() {
        let mixed: Vec<(String, i64)> = (0..6)
            .map(|i| {
                let w = if i % 2 == 0 { "0xa" } else { "0xb" };
                (w.to_string(), 1_000 + i)
            })
            .collect();
        // Three entries each, limit 2 — one refusal per wallet.
        assert_eq!(would_refuse(&mixed, W, 2), 2);
    }
}
