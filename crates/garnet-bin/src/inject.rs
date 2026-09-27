//! `garnet-core --inject` — a hand-made leader trade into the pipeline.
//!
//! A validation tool and a permanent debugging tool. The frame takes the same path as a real
//! one: `App::on_frame` -> detection -> dedup -> decision -> slippage -> order -> position.
//! Submitting an order directly to the client would check the signature and FAK and would
//! say nothing about the accounting.

use garnet_db::Side;
use rust_decimal::Decimal;

/// Build an RTDS frame with one trade.
///
/// The shape mirrors a live frame: the parser reads only `asset`, `side`, `price`, `size`,
/// `proxyWallet`, `transactionHash` and `timestamp`, because in 7.8% of frames the metadata
/// is empty. A frame built to the wrong shape would produce "nothing happened", and that
/// would look like an execution failure.
#[must_use]
pub fn synthesize(
    wallet: &str,
    token_id: &str,
    side: Side,
    price: Decimal,
    size: Decimal,
    tx_hash: &str,
) -> serde_json::Value {
    let now = chrono::Utc::now().timestamp();
    serde_json::json!({
        "topic": "activity",
        "type": "trades",
        "timestamp": now * 1000,
        "payload": {
            "proxyWallet": wallet,
            "asset": token_id,
            "side": match side { Side::Buy => "BUY", Side::Sell => "SELL" },
            "price": price.to_string(),
            "size": size.to_string(),
            "timestamp": now,
            "transactionHash": tx_hash,
        }
    })
}

/// A unique hash for one injection.
///
/// The dedup is keyed by `(tx_hash, wallet, token_id, side)`, so a repeat smoke test with
/// the same hash would be rejected as a duplicate — and that would look like an execution
/// failure rather than a protection that worked.
#[must_use]
pub fn fresh_tx_hash() -> String {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("0xinject{ns:032x}")
}

/// The gate before real money is spent.
///
/// # Errors
///
/// A live injection without an explicit `--yes`: a confirmation in shadow would be a ritual,
/// and a ritual teaches people to press "yes" without looking.
pub fn confirm(live: bool, yes: bool) -> anyhow::Result<()> {
    if live && !yes {
        anyhow::bail!("a live injection spends real money: repeat it with --yes");
    }
    Ok(())
}
