//! The third detection circuit: Polygon logs.
//!
//! On 31.08.2026 the `activity/trades` topic went down platform-wide for hours, from
//! every IP at once, and the safety net was the `/activity` poll — slow, and it was
//! exactly what brought 28 duplicates up to 3.4 days old and 69 `market_not_tradable`
//! refusals. A subscription to chain logs does not depend on Polymarket's
//! infrastructure at all: the node wakes us when a leader trades, and the filtering
//! happens on the node's side.
//!
//! **The circuit is more reliable than RTDS, but not faster.** A log appears once the
//! settlement transaction is in a block, and the leader's order was matched by the CLOB
//! before that. No claim about "getting ahead of the leader" follows from this — the
//! mechanics do not allow it.
//!
//! The decoding was written from scratch against the **verified V2 ABI** (Sourcify,
//! Polygon, 19.09.2026) rather than carried over from an existing project on V1
//! contracts, where the event has a different shape — see `OrderFilled` below.

use rust_decimal::Decimal;

/// `keccak256("OrderFilled(bytes32,address,address,uint8,uint256,uint256,uint256,uint256,bytes32,bytes32)")`
///
/// Verified against the live network: this topic yields 16,057 logs over 300 blocks on
/// the V2 exchange. The topic from the **V1** signature (five data words,
/// `makerAssetId` / `takerAssetId`) yields none at all.
pub const ORDER_FILLED_TOPIC0: &str =
    "0xd543adfd945773f1a62f74f0ee55a5e3b9b1a28262980ba90b1a89f2ea84d8ee";

/// The order's side, as the contract encodes it.
///
/// In V2 the side is an **explicit field** `side: uint8`, not an inference from which
/// `assetId` is zero. Inferring it from a zero `assetId` is V1, and carrying that into
/// V2 would give a decoder that parses and answers the wrong question.
const SIDE_BUY: u8 = 0;
const SIDE_SELL: u8 = 1;

/// Six decimal places, both for the collateral and for the shares.
const SCALE: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

/// The fill of one order, lifted from a log.
///
/// There is deliberately no time here: the log does not carry it, and taking it from our
/// clock is not allowed — the slice window closes by the **leader's clock** (invariant
/// 29), and substituting our own time would turn it into a measurement of our own
/// latency. The timestamp comes from the block, and the transport fetches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainFill {
    /// The order's owner — whoever's order was filled. **This is the leader.**
    pub maker: String,
    /// The counterparty. Equal to the exchange's address when the log belongs to the
    /// aggressor's leg.
    pub taker: String,
    pub tx_hash: String,
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub fee_usd: Decimal,
    pub block_number: u64,
}

/// Decode one log. `None` — the log is not about an order fill.
///
/// The contract address is not checked here: the address filter is set on the node's
/// side, where it is cheaper, and duplicating it here would mean having two places
/// where the list of exchanges can drift apart.
#[must_use]
pub fn parse_log(log: &serde_json::Value) -> Option<ChainFill> {
    let topics = log["topics"].as_array()?;
    if topics.len() != 4 || !eq_hex(topics.first()?.as_str()?, ORDER_FILLED_TOPIC0) {
        return None;
    }

    let words = data_words(log["data"].as_str()?)?;
    if words.len() != 7 {
        // Seven words means V2. Any other count means a different contract version,
        // and this function must not decode it: the fields sit elsewhere.
        return None;
    }

    let side = match u8_of(&words[0])? {
        SIDE_BUY => Side::Buy,
        SIDE_SELL => Side::Sell,
        _ => return None,
    };
    let token_id = dec_of(&words[1])?.to_string();
    let maker_amount = scaled(&words[2])?;
    let taker_amount = scaled(&words[3])?;
    let fee_usd = scaled(&words[4])?;

    if maker_amount <= Decimal::ZERO || taker_amount <= Decimal::ZERO {
        return None;
    }

    // The side decides what is what. On a buy the maker gives up collateral and receives
    // shares; on a sale it is the other way round. Mixing them up yields a price that is
    // the inverse of the real one — and it would look plausible.
    let (size, price) = match side {
        Side::Buy => (taker_amount, maker_amount.checked_div(taker_amount)?),
        Side::Sell => (maker_amount, taker_amount.checked_div(maker_amount)?),
    };

    Some(ChainFill {
        maker: addr_of(topics.get(2)?.as_str()?)?,
        taker: addr_of(topics.get(3)?.as_str()?)?,
        tx_hash: log["transactionHash"].as_str()?.to_lowercase(),
        token_id,
        side,
        price,
        size,
        fee_usd,
        block_number: u64_of(log["blockNumber"].as_str()?)?,
    })
}

/// The topic filter for `eth_subscribe`: the leader's address, padded to 32 bytes.
///
/// It goes in the **third** position (`topics[2]`, the `maker` field) — and **a second
/// filter on `taker` is not needed**. Verified against the live network on 19.09.2026:
/// the aggressor has an `OrderFilled` of its own, where it is the maker while `taker` is
/// the exchange's address. That is, every participant lands in `topics[2]` in their own
/// leg regardless of role, and of the 1877 addresses encountered in `topics[3]`, 1876
/// also appear in `topics[2]`.
#[must_use]
pub fn wallet_topic(address: &str) -> String {
    let a = address.trim_start_matches("0x").to_lowercase();
    format!("0x{}{a}", "0".repeat(64 - a.len()))
}

fn eq_hex(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn data_words(data: &str) -> Option<Vec<String>> {
    let d = data.strip_prefix("0x")?;
    if d.len() % 64 != 0 {
        return None;
    }
    Some(
        (0..d.len() / 64)
            .map(|i| d[i * 64..(i + 1) * 64].to_string())
            .collect(),
    )
}

fn u8_of(word: &str) -> Option<u8> {
    u8::try_from(u128::from_str_radix(word.trim_start_matches('0'), 16).unwrap_or(0)).ok()
}

fn u64_of(hex: &str) -> Option<u64> {
    u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok()
}

/// The token identifier as a decimal string.
///
/// A `uint256` fits neither in a `u128` nor in a `Decimal`, and we have to compare it
/// with the `asset` field from Polymarket's feed — that is, with a decimal string.
/// Rounding it is not allowed: an identifier that has lost its low digits points at a
/// different token, and the miss would be indistinguishable from "not our market".
///
/// Limbs of 10^9 in a `u64`: multiplying by 16 takes the intermediate value up to about
/// 1.6e10, which does not fit in a `u32` — the first revision of this function
/// overflowed on exactly that.
fn dec_of(word: &str) -> Option<String> {
    const LIMB: u64 = 1_000_000_000;
    let mut acc: Vec<u64> = vec![0];
    for ch in word.chars() {
        let mut carry = u64::from(ch.to_digit(16)?);
        for limb in &mut acc {
            let v = *limb * 16 + carry;
            *limb = v % LIMB;
            carry = v / LIMB;
        }
        while carry > 0 {
            acc.push(carry % LIMB);
            carry /= LIMB;
        }
    }
    let mut s = acc.pop()?.to_string();
    for limb in acc.iter().rev() {
        s.push_str(&format!("{limb:09}"));
    }
    Some(s)
}

/// An amount with six decimal places.
///
/// The conversion is deliberately **fallible**, although clippy suggests the infallible
/// `Decimal::from`. That panics on overflow (verified: `Decimal::from(u128::MAX)` brings
/// the process down), and what arrives here is a word from someone else's log — that is,
/// a value we do not choose. A panic in the ingest circuit takes out the whole circuit
/// for the sake of one unusable log; `None` costs exactly that log.
#[allow(clippy::unnecessary_fallible_conversions)]
fn scaled(word: &str) -> Option<Decimal> {
    let raw = u128::from_str_radix(word.trim_start_matches('0'), 16).unwrap_or(0);
    let mut d = Decimal::try_from(raw).ok()?;
    d.set_scale(SCALE).ok()?;
    Some(d)
}

fn addr_of(topic: &str) -> Option<String> {
    let t = topic.strip_prefix("0x")?;
    if t.len() != 64 {
        return None;
    }
    Some(format!("0x{}", &t[24..]).to_lowercase())
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// The `eth_subscribe` subscription frame.
///
/// The filter is set **on the node's side**: exchange addresses and topics. That is the
/// whole point of the circuit — the node wakes us when a leader trades, not when anyone
/// at all trades. Without a wallet filter we would have to accept the platform's entire
/// feed: measured 19.09.2026 — 7255 fills over 120 blocks, that is about thirty a
/// second.
///
/// Topic positions: the zeroth is the event, the first (`orderHash`) is not filtered,
/// the second is the **maker**. The third (`taker`) is not needed: the aggressor has a
/// log of its own where it is the maker (verified against the live network).
#[must_use]
pub fn subscribe_frame(id: u64, exchanges: &[String], wallets: &[String]) -> String {
    let topic2: Vec<String> = wallets.iter().map(|w| wallet_topic(w)).collect();
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "eth_subscribe",
        "params": ["logs", {
            "address": exchanges,
            "topics": [ORDER_FILLED_TOPIC0, serde_json::Value::Null, topic2],
        }],
    })
    .to_string()
}

/// The log out of an `eth_subscription` notification frame, if that is what it is.
#[must_use]
pub fn log_of_notification(raw: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if v["method"].as_str()? != "eth_subscription" {
        return None;
    }
    // The node sends removed logs on a chain reorganisation. Such a log is a
    // cancellation, not a trade: copying it means buying something that no longer
    // exists on chain.
    if v["params"]["result"]["removed"].as_bool() == Some(true) {
        return None;
    }
    Some(v["params"]["result"].clone())
}

/// Block timestamps.
///
/// A log carries no time, and `ts_trade` is the leader's time, by which the slice window
/// closes (invariant 29). The cache here is not an optimisation: trades arrive in
/// batches from one block, and without it every log in a batch would cost a separate
/// trip to the node on the very path where milliseconds are counted.
pub struct BlockClock {
    http: reqwest::Client,
    url: String,
    cache: Mutex<HashMap<u64, i64>>,
}

impl BlockClock {
    /// # Errors
    ///
    /// The HTTP client could not be built.
    pub fn new(url: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("garnet/2.0")
                .timeout(Duration::from_secs(10))
                .build()?,
            url: url.into(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// The block's timestamp, in epoch seconds.
    ///
    /// # Errors
    ///
    /// The node did not answer, or answered without a timestamp. The error is **not
    /// substituted with zero and not replaced by our clock**: a trade with an invented
    /// time would pass the assignment threshold and the slice window by the wrong clock,
    /// while looking like a real one.
    pub async fn timestamp(&self, block: u64) -> anyhow::Result<i64> {
        if let Some(t) = self.cache.lock().unwrap().get(&block) {
            return Ok(*t);
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_getBlockByNumber",
            "params": [format!("0x{block:x}"), false],
        });
        let v: serde_json::Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        let hex = v["result"]["timestamp"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("block {block}: the node returned no timestamp"))?;
        let ts = i64::from_str_radix(hex.trim_start_matches("0x"), 16)?;

        let mut cache = self.cache.lock().unwrap();
        // The block cache grows with uptime. Five hundred blocks is about twenty minutes
        // of Polygon, and we need none older than that.
        if cache.len() > 512 {
            cache.clear();
        }
        cache.insert(block, ts);
        Ok(ts)
    }
}

/// The reason the connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disconnect {
    Closed,
    Stalled,
    /// The operator changed the wallet list. The filter lives on the node's side, and a
    /// new wallet is invisible until the subscription is re-established.
    WalletsChanged,
}

/// We reconnect if the node has been silent for longer than this.
///
/// Five minutes, not forty-five seconds as with RTDS: there silence means a dead
/// subscription, here it means **the leaders were not trading**. A Polygon block takes
/// two seconds, but a fill by one particular wallet is a rare event, and a watchdog
/// that fires on a quiet market would be reconnecting all day.
pub const CHAIN_STALL_TIMEOUT: Duration = Duration::from_secs(300);

const STALL_CHECK_INTERVAL: Duration = Duration::from_secs(10);

pub struct ChainFeed {
    url: String,
    exchanges: Vec<String>,
    /// Read on every re-subscription: assigning a wallet is the bot's only control, and
    /// demanding a restart for it would make that control useless.
    watched: Arc<Mutex<Vec<String>>>,
}

impl ChainFeed {
    #[must_use]
    pub fn new(
        url: impl Into<String>,
        exchanges: Vec<String>,
        watched: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            url: url.into(),
            exchanges,
            watched,
        }
    }

    fn wallets(&self) -> Vec<String> {
        let mut v = self.watched.lock().unwrap().clone();
        v.sort();
        v
    }

    /// One connection: subscribe and read until it is closed, falls silent, or the
    /// wallet list changes.
    ///
    /// # Errors
    ///
    /// The socket did not open, or ended with an error.
    pub async fn run_once<F>(&self, mut on_log: F) -> anyhow::Result<(Disconnect, u64)>
    where
        F: FnMut(serde_json::Value),
    {
        let subscribed_with = self.wallets();
        if subscribed_with.is_empty() {
            // A subscription with no wallets would bring the platform's entire feed. We
            // wait for an assignment rather than accepting thirty logs a second for
            // nothing.
            tokio::time::sleep(Duration::from_secs(10)).await;
            return Ok((Disconnect::WalletsChanged, 0));
        }

        let (mut ws, _) = connect_async(&self.url).await?;
        ws.send(Message::Text(subscribe_frame(
            1,
            &self.exchanges,
            &subscribed_with,
        )))
        .await?;

        let mut delivered: u64 = 0;
        let mut last_data = tokio::time::Instant::now();
        let mut ticker = tokio::time::interval(STALL_CHECK_INTERVAL);
        ticker.tick().await;

        loop {
            tokio::select! {
                msg = ws.next() => {
                    let Some(msg) = msg else { return Ok((Disconnect::Closed, delivered)) };
                    match msg? {
                        Message::Text(text) => {
                            last_data = tokio::time::Instant::now();
                            if let Some(log) = log_of_notification(&text) {
                                delivered = delivered.saturating_add(1);
                                on_log(log);
                            }
                        }
                        Message::Ping(p) => ws.send(Message::Pong(p)).await?,
                        Message::Close(_) => return Ok((Disconnect::Closed, delivered)),
                        _ => {}
                    }
                }
                _ = ticker.tick() => {
                    if self.wallets() != subscribed_with {
                        return Ok((Disconnect::WalletsChanged, delivered));
                    }
                    if last_data.elapsed() >= CHAIN_STALL_TIMEOUT {
                        return Ok((Disconnect::Stalled, delivered));
                    }
                }
            }
        }
    }

    /// An endless loop with reconnection.
    pub async fn run_forever<F>(&self, mut on_log: F) -> !
    where
        F: FnMut(serde_json::Value),
    {
        let mut attempt = 0_u32;
        loop {
            match self.run_once(&mut on_log).await {
                Ok((reason, delivered)) => {
                    tracing::warn!(?reason, delivered, "the connection to the node ended");
                    // A change of wallets is not a failure: a pause before
                    // re-subscribing would mean an assigned wallet stays invisible for a
                    // minute.
                    if delivered > 0 || reason == Disconnect::WalletsChanged {
                        attempt = 0;
                    }
                    if reason == Disconnect::WalletsChanged {
                        continue;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "the connection to the node was not established")
                }
            }
            tokio::time::sleep(garnet_clob::backoff::full_jitter(
                attempt,
                Duration::from_secs(1),
                Duration::from_secs(60),
            ))
            .await;
            attempt = attempt.saturating_add(1);
        }
    }
}
