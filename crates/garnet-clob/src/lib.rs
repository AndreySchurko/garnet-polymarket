//! Polymarket CLOB v2 client for Garnet.
//!
//! Wraps the CLOB v2 REST API and WebSocket feed behind the [`ClobClient`]
//! trait, enabling dependency injection in tests via [`MockClobClient`].
//!
//! # CLOB v2 invariants enforced by this crate
//!
//! - `feeRateBps`, `nonce`, and `taker` are **never** set in order payloads;
//!   the exchange populates them automatically.
//! - Contract addresses are imported from the SDK constants, never hard-coded.
//! - Builder code is attached to orders only when `BUILDER_CODE` env var is
//!   non-empty.
//! - Order placement is EIP-712-signed by the injected
//!   [`OrderSigner`](signer::OrderSigner); read-only clients built via
//!   [`GarnetClobClient::new`] return [`ClobError::Auth`] for
//!   `place_limit_order`.
//! - Heartbeat must run every 15 s; the exchange cancels open orders if it stops.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │  garnet-trading-engine  (Phase 7)                    │
//! │      Arc<dyn ClobClient>                             │
//! └──────────┬───────────────────────────────────────────┘
//!            │  trait ClobClient
//!      ┌─────┴──────────────────────────────┐
//!      │ GarnetClobClient  │  MockClobClient │
//!      │  (REST + auth)    │  (tests / TEST) │
//!      └─────────────────────────────────────┘
//! ```
//!
//! See also [`OrderbookWs`] for the live WebSocket feed and
//! [`HeartbeatManager`] for the mandatory keep-alive loop.

#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::doc_markdown)]

pub mod backoff;
/// Egress proxy for Polymarket-bound traffic.
///
/// Public so `garnet-feed` dials RTDS through the same SOCKS5 path as the CLOB
/// WebSocket instead of growing a second, divergent implementation.
pub mod book;
pub mod client;
pub mod cloudflare;
pub mod error;
pub mod proxy;
pub mod retry;
/// Official-SDK order build/sign/post path (internal).
mod sdk;
pub mod settings;
pub mod signer;
pub(crate) mod time;
pub mod traits;
pub mod types;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

/// Re-exported because `GarnetClobClient::new_with_sdk` takes one: a caller
/// must not have to depend on `alloy` to name a type this crate's own public
/// signature requires.
pub use alloy::primitives::Address;
pub use book::{BookSnapshot, PriceLevel};
pub use client::GarnetClobClient;
pub use error::ClobError;
pub use settings::ClobSettings;
pub use signer::{
    Exchange, InMemoryOrderSigner, OrderSigner, SignatureType, SignedOrder, UnsignedOrder,
};
pub use traits::ClobClient;
pub use types::{ClobCredentials, ClobOrderStatus, OrderInfo, OrderKind, OrderResponse, Side};

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(env!("CARGO_PKG_NAME"), "garnet-clob");
    }
}
