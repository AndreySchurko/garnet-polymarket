//! Market metadata and the order book by `token_id`.
//!
//! An RTDS frame reliably carries only `asset` — the token itself: in 7.8% of frames
//! `conditionId`, `outcome`, `slug` and `title` are empty. Hence the path:
//!
//! 1. `GET /book?token_id=` — returns the book and the `market` field, that is, the
//!    `condition_id`. This is the only way to get from a token to a condition;
//! 2. `GET /markets/<condition_id>` — `tokens[]`, the source of the outcome label, its
//!    position and the `winner` flag;
//! 3. Gamma `/markets?condition_ids=` — `feeSchedule`. The CLOB does not have it, and its
//!    `taker_base_fee` equals 1000 for every market and is not a rate.
//!
//! The metadata is cached: it changes rarely, and an extra trip to the API is extra latency
//! on the signal path, where we count milliseconds.

use crate::app::BookSource;
use garnet_core::book::Book;
use garnet_core::detect::MarketSource;
use garnet_core::market_meta::{
    parse_clob_market, parse_condition_id, parse_fee_schedule, MarketMeta,
};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a cache entry lives. A market's resolution changes once, but we have to learn
/// about it within the hour, not a day later.
const CACHE_TTL: Duration = Duration::from_secs(600);

pub struct HttpMarkets {
    http: reqwest::Client,
    clob_host: String,
    gamma_host: String,
    cache: Mutex<HashMap<String, (MarketMeta, Instant)>>,
}

impl HttpMarkets {
    pub fn new(
        clob_host: impl Into<String>,
        gamma_host: impl Into<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            // Gamma answers 403 to Python's default User-Agent; we set our own explicitly so
            // as not to depend on a library's defaults.
            http: reqwest::Client::builder()
                .user_agent("garnet/2.0")
                .timeout(Duration::from_secs(10))
                .build()?,
            clob_host: clob_host.into(),
            gamma_host: gamma_host.into(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    async fn raw_book(&self, token_id: &str) -> anyhow::Result<serde_json::Value> {
        let url = format!("{}/book?token_id={}", self.clob_host, token_id);
        Ok(self.http.get(url).send().await?.json().await?)
    }

    /// The condition for a token via the book. The fast path, and the only one while the
    /// market is tradable.
    async fn condition_from_book(&self, token_id: &str) -> anyhow::Result<String> {
        let book = self.raw_book(token_id).await?;
        book["market"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                anyhow::anyhow!("the book for token {token_id} did not name the condition")
            })
    }

    /// A Gamma row by token. The fallback path to the condition: a resolved market has no
    /// book at all — `GET /book?token_id=` answers 404 "No orderbook exists for the requested
    /// token id", and settlement is left without a `winner`.
    ///
    /// `closed=true` is mandatory: without the flag Gamma does not show a closed market and
    /// answers with an empty array.
    async fn gamma_by_token(&self, token_id: &str) -> anyhow::Result<serde_json::Value> {
        Ok(self
            .http
            .get(format!(
                "{}/markets?clob_token_ids={}&closed=true",
                self.gamma_host, token_id
            ))
            .send()
            .await?
            .json()
            .await?)
    }

    /// A Gamma row by condition. The same flag for the same reason: the fee of a resolved
    /// market cannot otherwise be read, and `FeeSchedule::free()` in its place would
    /// understate the costs on every resolution.
    async fn gamma_by_condition(&self, condition_id: &str) -> anyhow::Result<serde_json::Value> {
        let open: serde_json::Value = self
            .http
            .get(format!(
                "{}/markets?condition_ids={}",
                self.gamma_host, condition_id
            ))
            .send()
            .await?
            .json()
            .await?;
        if !matches!(&open, serde_json::Value::Array(a) if a.is_empty()) {
            return Ok(open);
        }
        Ok(self
            .http
            .get(format!(
                "{}/markets?condition_ids={}&closed=true",
                self.gamma_host, condition_id
            ))
            .send()
            .await?
            .json()
            .await?)
    }

    async fn fetch_meta(&self, token_id: &str) -> anyhow::Result<MarketMeta> {
        // The book is tried first: while the market is tradable this is one request.
        let (condition_id, gamma) = match self.condition_from_book(token_id).await {
            Ok(cid) => (cid, None),
            Err(book_err) => {
                let row = self.gamma_by_token(token_id).await.map_err(|e| {
                    anyhow::anyhow!("{book_err}; Gamma by token did not answer either: {e}")
                })?;
                let cid =
                    parse_condition_id(&row).map_err(|e| anyhow::anyhow!("{book_err}; and {e}"))?;
                // The same row carries the fee — no second trip to Gamma.
                (cid, Some(row))
            }
        };

        let market: serde_json::Value = self
            .http
            .get(format!("{}/markets/{}", self.clob_host, condition_id))
            .send()
            .await?
            .json()
            .await?;
        let mut meta = parse_clob_market(&market, token_id)?;

        let gamma = match gamma {
            Some(row) => row,
            None => self.gamma_by_condition(&condition_id).await?,
        };
        meta.fee = parse_fee_schedule(&gamma)?;

        Ok(meta)
    }

    fn cached(&self, token_id: &str) -> Option<MarketMeta> {
        let guard = self.cache.lock().unwrap();
        guard
            .get(token_id)
            .filter(|(_, at)| at.elapsed() < CACHE_TTL)
            .map(|(m, _)| m.clone())
    }
}

impl MarketSource for HttpMarkets {
    async fn get(&self, token_id: &str) -> anyhow::Result<MarketMeta> {
        if let Some(m) = self.cached(token_id) {
            return Ok(m);
        }
        let meta = self.fetch_meta(token_id).await?;
        self.cache
            .lock()
            .unwrap()
            .insert(token_id.to_string(), (meta.clone(), Instant::now()));
        Ok(meta)
    }
}

impl garnet_core::equity::PriceSource for HttpMarkets {
    /// The mid of the book. A network error means "there is no price", not zero: the position
    /// lands among the unpriced ones and is visible as a number.
    async fn mid(&self, token_id: &str) -> anyhow::Result<Option<Decimal>> {
        match self.book(token_id).await {
            Ok(book) => Ok(book.mid()),
            Err(_) => Ok(None),
        }
    }
}

impl garnet_core::equity::PriceSource for SharedMarkets {
    async fn mid(&self, token_id: &str) -> anyhow::Result<Option<Decimal>> {
        self.0.mid(token_id).await
    }
}

impl BookSource for HttpMarkets {
    async fn book(&self, token_id: &str) -> anyhow::Result<Book> {
        // The book is never cached: the slippage decision is made from it.
        Book::from_clob(&self.raw_book(token_id).await?)
    }
}

/// A shared handle to a single instance.
///
/// `Detector` owns the market source, `App` owns the book source, and the cache has to be
/// one: two independent caches would diverge on a market's resolution, and settlement would
/// see something other than what the copy engine saw. The wrapper is needed because of the
/// orphan rule — a foreign trait cannot be implemented directly for `Arc`.
#[derive(Clone)]
pub struct SharedMarkets(std::sync::Arc<HttpMarkets>);

impl SharedMarkets {
    pub fn new(inner: HttpMarkets) -> Self {
        Self(std::sync::Arc::new(inner))
    }
}

impl MarketSource for SharedMarkets {
    async fn get(&self, token_id: &str) -> anyhow::Result<MarketMeta> {
        self.0.get(token_id).await
    }
}

impl BookSource for SharedMarkets {
    async fn book(&self, token_id: &str) -> anyhow::Result<Book> {
        self.0.book(token_id).await
    }
}
