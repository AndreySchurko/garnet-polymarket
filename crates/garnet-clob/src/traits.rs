//! [`ClobClient`] trait — the single abstraction over the CLOB v2 REST API.
//!
//! [`GarnetClobClient`](crate::client::GarnetClobClient) provides the real
//! implementation; [`MockClobClient`](crate::mock::MockClobClient) is used in
//! tests and `TEST` trading mode.  All consumers accept `Arc<dyn ClobClient>`.

use crate::book::BookSnapshot;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::error::ClobError;
use crate::types::{OrderInfo, OrderResponse, Side, TradeInfo};

/// Abstraction over all CLOB v2 order-management operations.
///
/// Both [`GarnetClobClient`](crate::client::GarnetClobClient) and
/// [`MockClobClient`](crate::mock::MockClobClient) implement this trait,
/// allowing dependency injection at the trading-engine level.
///
/// # Thread safety
///
/// All implementors must be `Send + Sync`; they are typically held behind an
/// `Arc<dyn ClobClient>`.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use rust_decimal_macros::dec;
/// use garnet_clob::traits::ClobClient;
/// use garnet_clob::types::{OrderKind, Side};
///
/// async fn example(client: Arc<dyn ClobClient>) {
///     let resp = client
///         .place_limit_order("0xno_token", Side::Buy, dec!(0.62), dec!(100), OrderKind::Gtc)
///         .await
///         .expect("order placed");
///     println!("order id: {}", resp.order_id);
/// }
/// ```
#[async_trait]
pub trait ClobClient: Send + Sync {
    /// Place a GTC limit order on the CLOB.
    ///
    /// `token_id` is the ERC-1155 NO-token ID.  `price` and `size` use
    /// [`rust_decimal::Decimal`]; the CLOB converts them to 6-decimal integer
    /// amounts internally.
    ///
    /// In [`GarnetClobClient`](crate::client::GarnetClobClient) the order is
    /// EIP-712-signed by the injected
    /// [`OrderSigner`](crate::signer::OrderSigner) and then POSTed to
    /// `/order`.  Use [`MockClobClient`](crate::mock::MockClobClient) in
    /// tests; a client constructed via
    /// [`GarnetClobClient::new`](crate::client::GarnetClobClient::new)
    /// without `new_with_signer` returns [`ClobError::Auth`] here.
    ///
    /// # Errors
    ///
    /// - [`ClobError::Auth`] — no signer was attached (read-only client).
    /// - [`ClobError::Http`] — CLOB returned a non-2xx status.
    /// - [`ClobError::RateLimited`] — 429 from CLOB.
    async fn place_limit_order(
        &self,
        token_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        kind: crate::types::OrderKind,
    ) -> Result<OrderResponse, ClobError>;

    /// Cancel a single open order by ID.
    ///
    /// # Errors
    ///
    /// - [`ClobError::NotFound`] — order does not exist.
    /// - [`ClobError::Http`] — CLOB returned a non-2xx status.
    async fn cancel_order(&self, order_id: &str) -> Result<(), ClobError>;

    /// Cancel **all** open orders owned by this account.
    ///
    /// Returns the number of orders that were cancelled.
    ///
    /// # Errors
    ///
    /// - [`ClobError::Http`] — CLOB returned a non-2xx status.
    async fn cancel_all(&self) -> Result<u32, ClobError>;

    /// Fetch current state of a single order.
    ///
    /// # Errors
    ///
    /// - [`ClobError::NotFound`] — order does not exist.
    /// - [`ClobError::Http`] — non-2xx CLOB response.
    async fn get_order(&self, order_id: &str) -> Result<OrderInfo, ClobError>;

    /// Fetch all orders currently open for this account.
    ///
    /// # Errors
    ///
    /// - [`ClobError::Http`] — non-2xx CLOB response.
    async fn get_open_orders(&self) -> Result<Vec<OrderInfo>, ClobError>;

    /// Fetch our own trades matched at or after `after`.
    ///
    /// The one view of the exchange that shows a fill we never recorded: an
    /// order that landed after our POST timed out and matched before the open-
    /// order reconciliation could see it resting (X-2) never appears in
    /// [`get_open_orders`](Self::get_open_orders) at all.
    ///
    /// # Errors
    ///
    /// - [`ClobError::Http`] — non-2xx CLOB response.
    async fn get_trades(&self, after: DateTime<Utc>) -> Result<Vec<TradeInfo>, ClobError>;

    /// Fetch the current orderbook snapshot for `token_id` via REST.
    ///
    /// Used by [`OrderbookWs`](crate::orderbook_ws::OrderbookWs) as a fallback
    /// when the WS hash does not match.
    ///
    /// # Errors
    ///
    /// - [`ClobError::Http`] — non-2xx CLOB response.
    async fn get_orderbook(&self, token_id: &str) -> Result<BookSnapshot, ClobError>;

    /// Whether `condition_id` has settled, and on which outcome.
    ///
    /// The exit path needs this **before** it touches a book, not after: a
    /// resolved market is delisted, so anything that fetches an orderbook first
    /// and skips the position when there is none leaves a settled — possibly
    /// winning — position open for ever. That is not hypothetical; it is what
    /// the shadow monitor did until 2026-08-13, while holding $35.87 of
    /// unclaimed winnings.
    ///
    /// Defaulted rather than required so the test doubles that have no notion
    /// of resolution keep compiling. The default reports ignorance as an error,
    /// which every caller must treat as "leave the position alone" — never as
    /// "still trading".
    ///
    /// # Errors
    ///
    /// - [`ClobError::NotFound`] — no such market, or the implementation does
    ///   not answer this question.
    /// - [`ClobError::Http`] — non-2xx CLOB response.
    /// Metadata for one market, or `None` when the CLOB does not know it.
    ///
    /// The copy engine's fallback when its own table has no row yet. See
    /// [`crate::types::ClobMarketMeta`] for why this exists and what it leaves
    /// out.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError`] if the request fails.
    async fn market_meta(
        &self,
        condition_id: &str,
    ) -> Result<Option<crate::types::ClobMarketMeta>, ClobError>;

    async fn market_resolution(
        &self,
        condition_id: &str,
    ) -> Result<crate::types::MarketResolution, ClobError> {
        Err(ClobError::NotFound(format!(
            "market_resolution not implemented for {condition_id}"
        )))
    }

    /// Last tick size the exchange reported for `token_id` **on this client**.
    ///
    /// Filled as a side effect of [`Self::get_orderbook`]: `/book` carries the
    /// market's tick on every response, and both pricing paths fetch a book
    /// immediately before they price. `None` means no book has been fetched for
    /// this token yet, or the response carried no usable tick; callers price
    /// without snapping in both cases.
    ///
    /// Not `async` and not fallible: it reads a local map, never the network.
    fn tick_size(&self, _token_id: &str) -> Option<Decimal> {
        None
    }

    /// Smallest order the exchange will accept on `token_id`, in shares.
    ///
    /// Learned from the same `/book` response as [`Self::tick_size`]. `None`
    /// means we have not been told, and the caller sends what it sized: the
    /// exchange rejecting an order is better than us inventing a floor.
    fn min_order_size(&self, _token_id: &str) -> Option<Decimal> {
        None
    }
}
