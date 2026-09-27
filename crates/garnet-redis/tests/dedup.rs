//! The fast path of the dedup. The test skips itself without a live Redis: it
//! checks the behaviour of the key, not the presence of infrastructure.

use garnet_redis::{dedup_key, GarnetRedis};

const URL: &str = "redis://127.0.0.1:6380";

async fn redis_or_skip() -> Option<GarnetRedis> {
    match tokio::time::timeout(std::time::Duration::from_secs(2), GarnetRedis::connect(URL)).await {
        Ok(Ok(r)) => Some(r),
        _ => {
            eprintln!("Redis unreachable at {URL}: test skipped");
            None
        }
    }
}

#[test]
fn key_matches_the_database_constraint() {
    // The key must match UNIQUE (tx_hash, wallet, token_id, side): were they to
    // diverge, the fast path would start rejecting something other than what the
    // database rejects.
    assert_eq!(
        dedup_key("0xaaa", "0xw", "tok", "buy"),
        "dedup:0xaaa:0xw:tok:buy"
    );
}

#[tokio::test]
async fn first_claim_wins_and_second_is_refused() {
    let Some(r) = redis_or_skip().await else {
        return;
    };
    let tx = format!(
        "0x{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    assert!(
        r.try_claim_trade(&tx, "0xw", "tok", "buy", 60)
            .await
            .unwrap(),
        "the first one claims it"
    );
    assert!(
        !r.try_claim_trade(&tx, "0xw", "tok", "buy", 60)
            .await
            .unwrap(),
        "the second one is refused"
    );
}

#[tokio::test]
async fn a_different_side_is_a_different_trade() {
    let Some(r) = redis_or_skip().await else {
        return;
    };
    let tx = format!(
        "0x{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    assert!(r
        .try_claim_trade(&tx, "0xw", "tok", "buy", 60)
        .await
        .unwrap());
    assert!(
        r.try_claim_trade(&tx, "0xw", "tok", "sell", 60)
            .await
            .unwrap(),
        "a buy and a sell in one transaction are different trades"
    );
}
