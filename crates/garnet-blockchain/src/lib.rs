//! Blockchain client for Garnet — Polygon mainnet (chain 137).
//!
//! Uses `alloy` (not web3).  Key responsibilities:
//! - [`traits::BlockchainClient`] — trait abstraction for dependency injection.
//! - [`client::GarnetBlockchainClient`] — real alloy/RPC implementation.
//! - [`mock::MockBlockchainClient`] — in-memory mock for tests and TEST mode.
//! - `ensure_v2_allowances()` — approve CTF Exchange V2 and Neg Risk Exchange V2.
//! - Balance checks: pUSD (collateral), USDC (unwrapped), MATIC (gas).
//! - Gas price oracle with per-transaction cost guard (`max_gas_cost_usdc`).
//! - RPC failover across `POLYGON_RPC_URLS`.
//!
//! Automatic redemption of WON positions lives in
//! `garnet_trading_engine::redeem_scheduler` (it needs DB access) and drives
//! [`traits::BlockchainClient::redeem_position`] defined here.

pub mod client;
pub mod contracts;
pub mod error;
pub mod failover;
pub mod gas;
pub mod order_signer;
pub mod settings;
pub mod traits;
pub mod types;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

pub use client::GarnetBlockchainClient;
pub use error::BcError;
pub use traits::BlockchainClient;
pub use types::{AllowanceStatus, BalanceSnapshot, GasPrices, TxHash, TxReceipt, B256, U256};
