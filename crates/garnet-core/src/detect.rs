//! Detecting leader trades.
//!
//! Two circuits: the RTDS socket and the safety-net `/activity` poll. Both write to
//! one table, and which copy arrives first does not matter: the key
//! `(tx_hash, wallet, token_id, side)` will not let the second one in.
//!
//! Nothing is ever sent to the socket: any keepalive kills the subscription.

use crate::market_meta::MarketMeta;
use anyhow::Context;
use chrono::{DateTime, TimeZone, Utc};
use garnet_db::{Db, LeaderActionRow, LeaderTrade, NewLeaderAction, NewLeaderTrade, Side, Source};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use std::collections::HashSet;

/// A source of market metadata. In production the CLOB plus a cache, in tests a stub.
pub trait MarketSource {
    fn get(
        &self,
        token_id: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<MarketMeta>> + Send;
}

/// A parsed frame, before enrichment with metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTrade {
    pub wallet: String,
    pub tx_hash: String,
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub ts_trade: DateTime<Utc>,
}

/// A leader action that is not a trade but does change the position.
///
/// Invariant 40. Merging a pair is an exit at $1 on both legs, and neither RTDS nor
/// `/activity` reports it as a `TRADE`. Until 19.09.2026 such rows were discarded,
/// and a position the leader had merged out of was held by us until resolution with
/// a `leader_observed_size` that no longer meant anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Merged a pair: exited both legs at $1.
    Merge,
    /// A split: entered both legs at once, paying $1 for the pair.
    Split,
    /// Redeemed something resolved. It does not touch our position — our own
    /// settlement closes that.
    Redeem,
}

impl ActionKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::Merge => "merge",
            ActionKind::Split => "split",
            ActionKind::Redeem => "redeem",
        }
    }
}

/// A non-trade by the leader. The key is the **condition**, not the token: a merge
/// burns both legs at once, and it does not have one token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAction {
    pub wallet: String,
    pub tx_hash: String,
    pub condition_id: String,
    pub kind: ActionKind,
    pub size: Decimal,
    pub ts_action: DateTime<Utc>,
}

/// What a feed row brought: a trade, or an action that is not a trade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaderAction {
    Trade(RawTrade),
    Other(RawAction),
}

/// Parses an RTDS frame into the trades it carries.
///
/// An empty result means "this frame is not about trades". A frame may carry a batch;
/// one unusable trade inside a batch costs only itself.
pub fn parse_frame(frame: &serde_json::Value) -> Vec<LeaderAction> {
    if frame["topic"].as_str() != Some("activity") || frame["type"].as_str() != Some("trades") {
        return Vec::new();
    }
    match &frame["payload"] {
        serde_json::Value::Array(items) => items.iter().filter_map(parse_row).collect(),
        one @ serde_json::Value::Object(_) => parse_row(one).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Trades only from a frame — for places where an action is out of place.
#[must_use]
pub fn trades_only(parsed: Vec<LeaderAction>) -> Vec<RawTrade> {
    parsed
        .into_iter()
        .filter_map(|a| match a {
            LeaderAction::Trade(t) => Some(t),
            LeaderAction::Other(_) => None,
        })
        .collect()
}

/// Parses the `/activity?user=` response — the safety-net detection circuit.
///
/// The same endpoint returns redemptions, splits and merges. None of them is a trade,
/// but **they do change the position**, and until 19.09.2026 they were silently
/// discarded by the `type == "TRADE"` filter (invariant 40).
///
/// The fields match those of an RTDS frame, so the parsing is shared: a divergence
/// between the two circuits would mean one and the same trade arriving under two
/// different keys with the dedup letting it through.
pub fn parse_activity(rows: &serde_json::Value) -> Vec<LeaderAction> {
    rows.as_array()
        .map(|items| items.iter().filter_map(parse_row).collect())
        .unwrap_or_default()
}

/// A feed row into a trade or into an action.
///
/// The parsing goes by `type`, not by `side`: on non-trades `side` is empty, and
/// leaning on its emptiness means treating any row with a broken field as a merge.
///
/// **The two circuits label a row with different fields**, and that is not cosmetic.
/// `/activity` puts the kind of event in `type` and leaves `side` empty on
/// non-trades. In an RTDS frame a row has no `type` field at all — there the kind sits
/// directly in `side`, next to `BUY` and `SELL` (fixture `rtds_batch.json`: `"side":
/// "MERGE"`). Parsing by `type` alone would bring down the whole socket: every RTDS
/// row lacks it, and nothing would be a trade any more.
///
/// Hence also the answer to a question the plan left open: the socket **does** carry
/// merges, and the second branch is not dead.
fn parse_row(p: &serde_json::Value) -> Option<LeaderAction> {
    let tag = p["type"].as_str().or_else(|| p["side"].as_str())?;
    let kind = match tag.to_uppercase().as_str() {
        "TRADE" | "BUY" | "SELL" => return parse_trade(p).map(LeaderAction::Trade),
        "MERGE" => ActionKind::Merge,
        "SPLIT" => ActionKind::Split,
        "REDEEM" => ActionKind::Redeem,
        _ => return None,
    };

    let size = num(&p["size"])?;
    // An event without a size says nothing about the position: there is nothing to
    // act on, and recording it means creating a row that nothing can explain.
    if size <= Decimal::ZERO {
        return None;
    }

    Some(LeaderAction::Other(RawAction {
        wallet: p["proxyWallet"].as_str()?.to_lowercase(),
        tx_hash: p["transactionHash"].as_str()?.to_string(),
        // The partition is computed by condition. A real merge never comes without
        // one, and it cannot be guessed from the token: the token here is either
        // empty or one leg out of two.
        condition_id: p["conditionId"]
            .as_str()
            .filter(|s| !s.is_empty())?
            .to_string(),
        kind,
        size,
        ts_action: Utc.timestamp_opt(p["timestamp"].as_i64()?, 0).single()?,
    }))
}

/// One trade out of a payload. The metadata (`conditionId`, `outcome`, `title`,
/// `slug`) is deliberately not read here: in 7.8% of frames it is empty, and the only
/// reliable field is `asset`.
fn parse_trade(p: &serde_json::Value) -> Option<RawTrade> {
    let side = match p["side"].as_str()?.to_uppercase().as_str() {
        "BUY" => Side::Buy,
        "SELL" => Side::Sell,
        // MERGE/SPLIT/REDEEM no longer reach here — `parse_row` took them by the
        // `type` field. What remains is genuine junk: a broken side.
        _ => return None,
    };

    let price = num(&p["price"])?;
    let size = num(&p["size"])?;
    if price <= Decimal::ZERO || size <= Decimal::ZERO {
        return None;
    }

    Some(RawTrade {
        wallet: p["proxyWallet"].as_str()?.to_lowercase(),
        tx_hash: p["transactionHash"].as_str()?.to_string(),
        token_id: p["asset"].as_str().filter(|s| !s.is_empty())?.to_string(),
        side,
        price,
        size,
        ts_trade: Utc.timestamp_opt(p["timestamp"].as_i64()?, 0).single()?,
    })
}

fn num(v: &serde_json::Value) -> Option<Decimal> {
    match v {
        serde_json::Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        serde_json::Value::String(s) => Decimal::from_str(s).ok(),
        _ => None,
    }
}

fn lowercased(wallets: impl IntoIterator<Item = String>) -> HashSet<String> {
    wallets.into_iter().map(|w| w.to_lowercase()).collect()
}

/// What was recorded for the first time. Trades and non-trades are kept apart: they
/// are handled by different paths, and putting them in one list would force the caller
/// to take them apart again.
#[derive(Debug, Default)]
pub struct Ingested {
    pub trades: Vec<LeaderTrade>,
    pub actions: Vec<LeaderActionRow>,
}

pub struct Detector<M: MarketSource> {
    db: Db,
    markets: M,
    /// The wallets assigned by the operator. Changes on the fly: assignment is the
    /// only control the bot has, and demanding a restart for it would make that
    /// control useless.
    watched: std::sync::RwLock<HashSet<String>>,
}

impl<M: MarketSource> Detector<M> {
    pub fn new(db: Db, markets: M, watched: impl IntoIterator<Item = String>) -> Self {
        Self {
            db,
            markets,
            watched: std::sync::RwLock::new(lowercased(watched)),
        }
    }

    /// Replace the list of watched wallets.
    pub fn set_watched(&self, wallets: impl IntoIterator<Item = String>) {
        *self.watched.write().unwrap() = lowercased(wallets);
    }

    /// How many wallets are under observation.
    pub fn watched_count(&self) -> usize {
        self.watched.read().unwrap().len()
    }

    /// Market metadata through the same source as detection.
    pub async fn market(&self, token_id: &str) -> anyhow::Result<crate::market_meta::MarketMeta> {
        self.markets.get(token_id).await
    }

    /// A frame from the socket. Returns what was recorded for the first time.
    pub async fn on_frame(&self, frame: &serde_json::Value) -> anyhow::Result<Ingested> {
        self.ingest(parse_frame(frame), Source::Rtds).await
    }

    /// Rows from the safety-net `/activity?user=` poll.
    pub async fn ingest_poll(&self, rows: Vec<LeaderAction>) -> anyhow::Result<Ingested> {
        self.ingest(rows, Source::Poll).await
    }

    /// Fills lifted from Polygon logs.
    ///
    /// The third circuit writes to **the same** table and collapses on the same key
    /// `(tx_hash, wallet, token_id, side)`: the log carries the same
    /// `transactionHash` as the socket frame (invariant 41). It is not a third source
    /// of truth — it is a third delivery of one truth.
    pub async fn ingest_chain(&self, rows: Vec<LeaderAction>) -> anyhow::Result<Ingested> {
        self.ingest(rows, Source::Chain).await
    }

    async fn ingest(&self, parsed: Vec<LeaderAction>, source: Source) -> anyhow::Result<Ingested> {
        let mut out = Ingested::default();
        for item in parsed {
            let r = match item {
                LeaderAction::Trade(t) => t,
                LeaderAction::Other(a) => {
                    // A row for a wallet we do not watch is someone else's feed: the
                    // same filter as for trades, and for the same reason.
                    if !self.watched.read().unwrap().contains(&a.wallet) {
                        continue;
                    }
                    let row = self
                        .db
                        .actions()
                        .insert_new(&NewLeaderAction {
                            wallet: a.wallet,
                            tx_hash: a.tx_hash,
                            condition_id: a.condition_id,
                            kind: a.kind.as_str().to_string(),
                            size: a.size,
                            ts_action: a.ts_action,
                            source,
                        })
                        .await?;
                    if let Some(row) = row {
                        out.actions.push(row);
                    }
                    continue;
                }
            };
            if !self.watched.read().unwrap().contains(&r.wallet) {
                continue;
            }
            // The metadata is recovered by token_id — the only field that always
            // arrives populated.
            let meta = self
                .markets
                .get(&r.token_id)
                .await
                .with_context(|| format!("market metadata for {}", r.token_id))?;

            // The metadata is more than the text of a trade: `condition_id` is asked
            // of the book, and a resolved market has no book. The registry outlives
            // the cache, the process, and the closing of the market.
            self.db.markets().upsert(&market_row(&meta)).await?;

            let t = NewLeaderTrade {
                wallet: r.wallet,
                tx_hash: r.tx_hash,
                token_id: r.token_id,
                side: r.side,
                price: r.price,
                size: r.size,
                ts_trade: r.ts_trade,
                source,
                market_text: meta.question.clone(),
                outcome_text: meta.outcome_label.clone(),
            };

            // A sighting is recorded for both deliveries, only the first one is acted
            // upon (invariant 43).
            if let Some(row) = self.db.trades().record(&t).await?.fresh() {
                out.trades.push(row);
            }
        }
        Ok(out)
    }
}

/// `MarketMeta` into a registry row. It lives here rather than in `garnet-db`: the
/// market's vocabulary belongs to the core, while the repository knows SQL and does
/// not know trading.
fn market_row(m: &MarketMeta) -> garnet_db::NewMarket {
    garnet_db::NewMarket {
        token_id: m.token_id.clone(),
        condition_id: m.condition_id.clone(),
        question: m.question.clone(),
        outcome_label: m.outcome_label.clone(),
        category: m.category.clone(),
        game_start_time: m.game_start_time,
        end_date: m.end_date,
        neg_risk: m.neg_risk,
        fee_rate: m.fee.rate,
        fee_exponent: m.fee.exponent,
        fee_taker_only: m.fee.taker_only,
        resolved_outcome: m.resolved_outcome.clone(),
        closed: m.closed,
    }
}
