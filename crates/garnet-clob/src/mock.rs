//! [`MockClobClient`] for tests and `TEST` trading mode.
//!
//! Enabled under `#[cfg(any(test, feature = "mock"))]`.
//!
//! Records every call in an `Arc<Mutex<Vec<MockOrder>>>`.  Responses are drawn
//! from a pre-configured queue; when the queue is empty it falls back to a
//! simple success response with a generated order ID.
//!
//! # Examples
//!
//! ```rust
//! use std::sync::Arc;
//! use rust_decimal_macros::dec;
//! use garnet_clob::mock::MockClobClient;
//! use garnet_clob::traits::ClobClient;
//! use garnet_clob::types::{OrderKind, Side};
//!
//! # tokio_test::block_on(async {
//! let mock = Arc::new(MockClobClient::new());
//! let resp = mock
//!     .place_limit_order("0xno_token", Side::Buy, dec!(0.62), dec!(100), OrderKind::Gtc)
//!     .await
//!     .unwrap();
//! assert_eq!(resp.side, Side::Buy);
//! assert_eq!(mock.received_orders().await.len(), 1);
//! # });
//! ```

#![cfg(any(test, feature = "mock"))]

use std::sync::Arc;

use crate::book::{BookSnapshot, PriceLevel};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    error::ClobError,
    traits::ClobClient,
    types::{ClobOrderStatus, MarketResolution, OrderInfo, OrderResponse, Side, TradeInfo},
};

// ---------------------------------------------------------------------------
// Recorded call
// ---------------------------------------------------------------------------

/// A single call recorded by [`MockClobClient`].
#[derive(Debug, Clone)]
pub struct MockOrder {
    /// Token ID passed to `place_limit_order`.
    pub token_id: String,
    /// Order side.
    pub side: Side,
    /// Limit price.
    pub price: Decimal,
    /// Size in shares.
    pub size: Decimal,
    /// The order type. Recorded because in Garnet it is the only thing separating the
    /// copying of a taker from a maker leg.
    pub kind: crate::types::OrderKind,
    /// UTC timestamp of the call.
    pub called_at: chrono::DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Pre-configured response queue entry
// ---------------------------------------------------------------------------

/// A pre-configured entry in the response queue.
enum QueueEntry {
    Success(OrderResponse),
    Failure(ClobError),
}

// ---------------------------------------------------------------------------
// MockClobClient
// ---------------------------------------------------------------------------

/// In-memory CLOB client for unit and integration tests.
///
/// Thread-safe; wrap in `Arc` and share across tasks.
pub struct MockClobClient {
    /// All `place_limit_order` calls, in insertion order.
    recorded: Arc<Mutex<Vec<MockOrder>>>,
    /// Pre-configured responses drawn in FIFO order.
    queue: Arc<Mutex<Vec<QueueEntry>>>,
    /// Simulated open orders for `get_open_orders`.
    orders: Arc<Mutex<Vec<OrderInfo>>>,
    /// Tokens whose book has been withdrawn — see [`Self::delist`].
    delisted: Arc<Mutex<Vec<String>>>,
    /// Ticks handed to the mock by a test — see [`Self::set_tick`].
    ticks: Arc<dashmap::DashMap<String, Decimal>>,
    /// Minimum order sizes handed to the mock — see [`Self::set_min_order_size`].
    min_sizes: Arc<dashmap::DashMap<String, Decimal>>,
    /// Trades returned by `get_trades`, filtered by its `after` argument.
    trades: Arc<Mutex<Vec<TradeInfo>>>,
    /// Resolutions handed to the mock — see [`Self::resolve`]. Anything absent
    /// reports as still trading, which is the state of most markets.
    resolutions: Arc<dashmap::DashMap<String, MarketResolution>>,
}

impl MockClobClient {
    /// Create a new mock with an empty queue and no pre-configured orders.
    #[must_use]
    pub fn new() -> Self {
        Self {
            recorded: Arc::new(Mutex::new(Vec::new())),
            queue: Arc::new(Mutex::new(Vec::new())),
            orders: Arc::new(Mutex::new(Vec::new())),
            delisted: Arc::new(Mutex::new(Vec::new())),
            ticks: Arc::new(dashmap::DashMap::new()),
            min_sizes: Arc::new(dashmap::DashMap::new()),
            trades: Arc::new(Mutex::new(Vec::new())),
            resolutions: Arc::new(dashmap::DashMap::new()),
        }
    }

    /// Settle a market on `winner`, as the CLOB reports it once a market pays
    /// out.
    ///
    /// Pair it with [`Self::delist`] to reproduce the real shape of a resolved
    /// market: settled *and* with no book left to read a price from. Those two
    /// facts arriving together is exactly what the exit path used to get wrong.
    pub fn resolve(&self, condition_id: &str, winner: &str) {
        self.resolutions.insert(
            condition_id.to_owned(),
            MarketResolution {
                closed: true,
                winner: Some(winner.to_owned()),
            },
        );
    }

    /// Settle a market without naming a winner — the wire gap a caller must
    /// treat as "leave the position alone", never as a total loss.
    pub fn resolve_without_winner(&self, condition_id: &str) {
        self.resolutions.insert(
            condition_id.to_owned(),
            MarketResolution {
                closed: true,
                winner: None,
            },
        );
    }

    /// Seed the trades `get_trades` reports (it still applies the `after` cut).
    pub async fn set_trades(&self, trades: Vec<TradeInfo>) {
        *self.trades.lock().await = trades;
    }

    /// Withdraw a token's book, as the CLOB does once a market resolves.
    ///
    /// The default snapshot is returned for every token, which quietly hides the
    /// case that matters most on the exit path: a resolved market usually has no
    /// book left to price against, and code that requires one will skip the
    /// position instead of redeeming it.
    pub async fn delist(&self, token_id: &str) {
        self.delisted.lock().await.push(token_id.to_owned());
    }

    /// Enqueue a successful `place_limit_order` response.
    #[allow(clippy::similar_names)]
    pub async fn expect_success(
        &self,
        order_id: &str,
        token_id: &str,
        price: Decimal,
        size: Decimal,
        side: Side,
    ) {
        let resp = OrderResponse {
            order_id: order_id.into(),
            status: ClobOrderStatus::Open,
            token_id: token_id.into(),
            price,
            size,
            side,
            created_at: Utc::now(),
        };
        self.queue.lock().await.push(QueueEntry::Success(resp));
    }

    /// Enqueue a failure for the next `place_limit_order` call.
    pub async fn expect_failure(&self, error: ClobError) {
        self.queue.lock().await.push(QueueEntry::Failure(error));
    }

    /// Return all recorded `place_limit_order` calls.
    pub async fn received_orders(&self) -> Vec<MockOrder> {
        self.recorded.lock().await.clone()
    }

    /// Simulate a fill event by moving an order to `Matched` status.
    ///
    /// Subsequent `get_open_orders` will not include it.  Useful for
    /// integration tests that simulate the fill WS event.
    pub async fn simulate_fill(&self, order_id: &str, size_matched: Decimal) {
        let mut orders = self.orders.lock().await;
        for o in orders.iter_mut() {
            if o.order_id == order_id {
                o.size_matched = size_matched;
                o.size_remaining = o.original_size - size_matched;
                o.status = ClobOrderStatus::Matched;
                o.updated_at = Utc::now();
            }
        }
    }

    /// Simulate an exchange-side cancel: set the order's status to `Cancelled`
    /// (kept in the list so `get_order` returns it, unlike `cancel_order` which
    /// removes it). Useful for fills-watcher cancel-path tests.
    pub async fn simulate_cancel(&self, order_id: &str) {
        let mut orders = self.orders.lock().await;
        for o in orders.iter_mut() {
            if o.order_id == order_id {
                o.status = ClobOrderStatus::Cancelled;
                o.updated_at = Utc::now();
            }
        }
    }

    /// Add a simulated open order (for `get_open_orders` responses).
    #[allow(clippy::similar_names)]
    pub async fn add_open_order(
        &self,
        order_id: &str,
        token_id: &str,
        price: Decimal,
        size: Decimal,
        side: Side,
    ) {
        let info = OrderInfo {
            order_id: order_id.into(),
            token_id: token_id.into(),
            price,
            original_size: size,
            size_matched: Decimal::ZERO,
            size_remaining: size,
            side,
            status: ClobOrderStatus::Open,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        self.orders.lock().await.push(info);
    }

    /// Serve `tick` as this token's market tick.
    ///
    /// Sync, unlike the other helpers: the trait method it feeds reads a map,
    /// so making it async would put an `.await` on a pricing path that has no
    /// reason to yield.
    pub fn set_tick(&self, token_id: &str, tick: Decimal) {
        self.ticks.insert(token_id.to_owned(), tick);
    }

    /// Serve `size` as this market's smallest accepted order.
    pub fn set_min_order_size(&self, token_id: &str, size: Decimal) {
        self.min_sizes.insert(token_id.to_owned(), size);
    }

    /// Remove all simulated orders (reset state for the next test).
    pub async fn clear(&self) {
        self.recorded.lock().await.clear();
        self.queue.lock().await.clear();
        self.orders.lock().await.clear();
    }
}

impl Default for MockClobClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ClobClient for MockClobClient {
    #[allow(clippy::similar_names)]
    async fn place_limit_order(
        &self,
        token_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        kind: crate::types::OrderKind,
    ) -> Result<OrderResponse, ClobError> {
        // Record the call.
        self.recorded.lock().await.push(MockOrder {
            kind,
            token_id: token_id.into(),
            side,
            price,
            size,
            called_at: Utc::now(),
        });

        // Dequeue the next pre-configured response (FIFO).
        let mut q = self.queue.lock().await;
        let entry = if q.is_empty() {
            None
        } else {
            Some(q.remove(0))
        };
        drop(q);
        match entry {
            Some(QueueEntry::Success(resp)) => Ok(resp),
            Some(QueueEntry::Failure(e)) => Err(e),
            None => {
                // Default: auto-generate a success response.
                let order_id = format!("mock_{}", Uuid::new_v4().as_simple());
                let resp = OrderResponse {
                    order_id: order_id.clone(),
                    status: ClobOrderStatus::Open,
                    token_id: token_id.into(),
                    price,
                    size,
                    side,
                    created_at: Utc::now(),
                };
                // Register in the open-orders list.
                self.orders.lock().await.push(OrderInfo {
                    order_id,
                    token_id: token_id.into(),
                    price,
                    original_size: size,
                    size_matched: Decimal::ZERO,
                    size_remaining: size,
                    side,
                    status: ClobOrderStatus::Open,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                });
                Ok(resp)
            }
        }
    }

    async fn cancel_order(&self, order_id: &str) -> Result<(), ClobError> {
        let mut orders = self.orders.lock().await;
        let before = orders.len();
        orders.retain(|o| o.order_id != order_id);
        if orders.len() == before {
            Err(ClobError::NotFound(order_id.into()))
        } else {
            Ok(())
        }
    }

    async fn cancel_all(&self) -> Result<u32, ClobError> {
        let mut orders = self.orders.lock().await;
        let count = u32::try_from(orders.len()).unwrap_or(u32::MAX);
        orders.clear();
        Ok(count)
    }

    async fn get_order(&self, order_id: &str) -> Result<OrderInfo, ClobError> {
        let orders = self.orders.lock().await;
        orders
            .iter()
            .find(|o| o.order_id == order_id)
            .cloned()
            .ok_or_else(|| ClobError::NotFound(order_id.into()))
    }

    async fn get_open_orders(&self) -> Result<Vec<OrderInfo>, ClobError> {
        let orders = self.orders.lock().await;
        Ok(orders
            .iter()
            .filter(|o| matches!(o.status, ClobOrderStatus::Open))
            .cloned()
            .collect())
    }

    async fn get_trades(&self, after: DateTime<Utc>) -> Result<Vec<TradeInfo>, ClobError> {
        Ok(self
            .trades
            .lock()
            .await
            .iter()
            .filter(|t| t.match_time >= after)
            .cloned()
            .collect())
    }

    async fn market_resolution(&self, condition_id: &str) -> Result<MarketResolution, ClobError> {
        Ok(self
            .resolutions
            .get(condition_id)
            .map_or_else(MarketResolution::open, |r| r.clone()))
    }

    async fn market_meta(
        &self,
        condition_id: &str,
    ) -> Result<Option<crate::types::ClobMarketMeta>, ClobError> {
        // Knows every market it has a resolution for, and nothing else. Tests
        // that want the copy engine's fallback to miss simply do not seed one.
        Ok(self
            .resolutions
            .get(condition_id)
            .map(|r| crate::types::ClobMarketMeta {
                condition_id: condition_id.to_owned(),
                end_date: None,
                game_start_time: None,
                closed: r.closed,
                tokens: Vec::new(),
            }))
    }

    async fn get_orderbook(&self, token_id: &str) -> Result<BookSnapshot, ClobError> {
        use rust_decimal_macros::dec;
        if self.delisted.lock().await.iter().any(|t| t == token_id) {
            return Err(ClobError::NotFound(format!("no book for {token_id}")));
        }
        // Return a synthetic snapshot for any other token_id.
        Ok(BookSnapshot {
            asset_id: token_id.into(),
            market: "0xmock_condition".into(),
            timestamp: Utc::now(),
            received_at: Utc::now(),
            bids: vec![PriceLevel {
                price: dec!(0.61),
                size: dec!(200),
            }],
            asks: vec![PriceLevel {
                price: dec!(0.62),
                size: dec!(500),
            }],
            hash: "mock_hash".into(),
        })
    }

    fn tick_size(&self, token_id: &str) -> Option<Decimal> {
        self.ticks.get(token_id).map(|v| *v.value())
    }

    fn min_order_size(&self, token_id: &str) -> Option<Decimal> {
        self.min_sizes.get(token_id).map(|v| *v.value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn the_mock_serves_the_tick_it_was_given() {
        // Executor tests need a fine grid without a network. Unknown tokens
        // decline, exactly as the real client does before its first book.
        let mock = MockClobClient::new();
        assert_eq!(mock.tick_size("0xno"), None);
        mock.set_tick("0xno", dec!(0.001));
        assert_eq!(mock.tick_size("0xno"), Some(dec!(0.001)));
        assert_eq!(mock.tick_size("0xother"), None);
    }

    #[tokio::test]
    async fn place_and_record() {
        let mock = MockClobClient::new();
        let resp = mock
            .place_limit_order(
                "0xtoken",
                Side::Buy,
                dec!(0.62),
                dec!(100),
                crate::types::OrderKind::Fak,
            )
            .await
            .unwrap();
        assert_eq!(resp.side, Side::Buy);
        assert_eq!(resp.price, dec!(0.62));
        let recorded = mock.received_orders().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].token_id, "0xtoken");
    }

    #[tokio::test]
    async fn enqueue_failure() {
        let mock = MockClobClient::new();
        mock.expect_failure(ClobError::Http {
            status: 503,
            message: "down".into(),
        })
        .await;
        let err = mock
            .place_limit_order(
                "0xt",
                Side::Buy,
                dec!(0.5),
                dec!(50),
                crate::types::OrderKind::Fak,
            )
            .await
            .unwrap_err();
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn cancel_all_clears_orders() {
        let mock = MockClobClient::new();
        mock.add_open_order("o1", "0xt1", dec!(0.62), dec!(100), Side::Buy)
            .await;
        mock.add_open_order("o2", "0xt2", dec!(0.63), dec!(50), Side::Buy)
            .await;
        mock.add_open_order("o3", "0xt3", dec!(0.61), dec!(75), Side::Buy)
            .await;
        let count = mock.cancel_all().await.unwrap();
        assert_eq!(count, 3);
        assert!(mock.get_open_orders().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancel_order_not_found() {
        let mock = MockClobClient::new();
        let err = mock.cancel_order("nonexistent").await.unwrap_err();
        assert!(matches!(err, ClobError::NotFound(_)));
    }

    #[tokio::test]
    async fn simulate_fill_updates_status() {
        let mock = MockClobClient::new();
        mock.add_open_order("o1", "0xt", dec!(0.62), dec!(100), Side::Buy)
            .await;
        mock.simulate_fill("o1", dec!(100)).await;
        let info = mock.get_order("o1").await.unwrap();
        assert_eq!(info.status, ClobOrderStatus::Matched);
        assert_eq!(info.size_matched, dec!(100));
    }

    #[tokio::test]
    async fn get_orderbook_returns_synthetic_snapshot() {
        let mock = MockClobClient::new();
        let snap = mock.get_orderbook("0xtoken").await.unwrap();
        assert_eq!(snap.asset_id, "0xtoken");
        assert!(!snap.asks.is_empty());
    }
}
