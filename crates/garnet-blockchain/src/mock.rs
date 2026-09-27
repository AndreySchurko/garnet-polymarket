//! [`MockBlockchainClient`] for tests and `TEST` trading mode.
//!
//! All balances are virtual and held in `Arc<Mutex<...>>`.  Transactions
//! return a synthetic `TxHash`; no real RPC calls are made.
//!
//! # Examples
//!
//! ```rust
//! use std::sync::Arc;
//! use rust_decimal_macros::dec;
//! use garnet_blockchain::mock::MockBlockchainClient;
//! use garnet_blockchain::traits::BlockchainClient;
//!
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! let mock = Arc::new(MockBlockchainClient::new());
//! mock.set_usdc(dec!(500)).await;
//! assert_eq!(mock.balance_usdc().await.unwrap(), dec!(500));
//! # });
//! ```

#![cfg(any(test, feature = "mock"))]

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{FixedBytes, U256};
use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::Mutex;
use tracing::info;

use garnet_types::market::OutcomeIndex;

use crate::error::BcError;
use crate::traits::BlockchainClient;
use crate::types::{
    AllowanceStatus, BalanceSnapshot, GasPrices, GasSource, TxHash, TxReceipt, B256,
};

// ---------------------------------------------------------------------------
// Recorded call types
// ---------------------------------------------------------------------------

/// A recorded transaction call made through the mock.
#[derive(Debug, Clone)]
pub enum MockCall {
    /// `wrap_usdc(amount)`.
    WrapUsdc(Decimal),
    /// `unwrap_pusd(amount)`.
    UnwrapPusd(Decimal),
    /// `ensure_v2_allowances()`.
    EnsureAllowances,
    /// `redeem_position(condition_id, neg_risk, outcome, size)`.
    RedeemPosition(B256, bool, OutcomeIndex, Decimal),
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct MockState {
    /// Outcome-token balances by `token_id`.
    outcomes: std::collections::HashMap<String, Decimal>,
    usdc: Decimal,
    pusd: Decimal,
    matic: Decimal,
    allowances: AllowanceStatus,
    calls: Vec<MockCall>,
    tx_counter: u64,
}

impl MockState {
    fn new_tx(&mut self) -> TxHash {
        self.tx_counter += 1;
        let mut bytes = [0u8; 32];
        let count_bytes = self.tx_counter.to_le_bytes();
        bytes[..8].copy_from_slice(&count_bytes);
        FixedBytes::from(bytes)
    }
}

// ---------------------------------------------------------------------------
// MockBlockchainClient
// ---------------------------------------------------------------------------

/// In-memory blockchain client for unit tests and TEST trading mode.
pub struct MockBlockchainClient {
    state: Arc<Mutex<MockState>>,
}

impl MockBlockchainClient {
    /// Create a mock with zero balances and no allowances set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MockState {
                outcomes: std::collections::HashMap::new(),
                usdc: Decimal::ZERO,
                pusd: Decimal::ZERO,
                matic: dec!(1.0),
                allowances: AllowanceStatus {
                    pusd_to_ctf_exchange: false,
                    pusd_to_neg_risk_exchange: false,
                    ctf_to_ctf_exchange: false,
                    ctf_to_neg_risk_exchange: false,
                    ctf_to_neg_risk_adapter: false,
                    ctf_to_collateral_adapter: false,
                },
                calls: Vec::new(),
                tx_counter: 0,
            })),
        }
    }

    /// Set the virtual USDC balance.
    pub async fn set_usdc(&self, amount: Decimal) {
        self.state.lock().await.usdc = amount;
    }

    /// Set the balance of outcome tokens.
    pub async fn set_outcome_balance(&self, token_id: &str, size: Decimal) {
        self.state
            .lock()
            .await
            .outcomes
            .insert(token_id.to_string(), size);
    }

    /// Set the virtual pUSD balance.
    pub async fn set_pusd(&self, amount: Decimal) {
        self.state.lock().await.pusd = amount;
    }

    /// Set the virtual MATIC balance.
    pub async fn set_matic(&self, amount: Decimal) {
        self.state.lock().await.matic = amount;
    }

    /// Pre-configure all allowances as already set.
    pub async fn set_all_allowances(&self) {
        let mut s = self.state.lock().await;
        s.allowances = AllowanceStatus {
            pusd_to_ctf_exchange: true,
            pusd_to_neg_risk_exchange: true,
            ctf_to_ctf_exchange: true,
            ctf_to_neg_risk_exchange: true,
            ctf_to_neg_risk_adapter: true,
            ctf_to_collateral_adapter: true,
        };
    }

    /// Return all calls recorded so far.
    pub async fn calls(&self) -> Vec<MockCall> {
        self.state.lock().await.calls.clone()
    }

    /// Reset all state.
    pub async fn reset(&self) {
        let mut s = self.state.lock().await;
        s.outcomes.clear();
        s.usdc = Decimal::ZERO;
        s.pusd = Decimal::ZERO;
        s.matic = dec!(1.0);
        s.tx_counter = 0;
        s.calls.clear();
        s.allowances = AllowanceStatus {
            pusd_to_ctf_exchange: false,
            pusd_to_neg_risk_exchange: false,
            ctf_to_ctf_exchange: false,
            ctf_to_neg_risk_exchange: false,
            ctf_to_neg_risk_adapter: false,
            ctf_to_collateral_adapter: false,
        };
    }
}

impl Default for MockBlockchainClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl BlockchainClient for MockBlockchainClient {
    async fn balance_of_outcome(&self, token_id: &str) -> Result<Decimal, BcError> {
        // An unreadable identifier is an error here too: a stub that forgives what
        // production fails on would hide the caller's defect. The check is the same as
        // in the client: real `token_id` values are 256-bit and do not fit in a u128.
        if alloy::primitives::U256::from_str_radix(token_id.trim(), 10).is_err() {
            return Err(BcError::Config(format!(
                "token_id {token_id} is not a number"
            )));
        }
        Ok(self
            .state
            .lock()
            .await
            .outcomes
            .get(token_id)
            .copied()
            .unwrap_or(Decimal::ZERO))
    }

    async fn balance_usdc(&self) -> Result<Decimal, BcError> {
        Ok(self.state.lock().await.usdc)
    }

    async fn balance_pusd(&self) -> Result<Decimal, BcError> {
        Ok(self.state.lock().await.pusd)
    }

    async fn balance_matic(&self) -> Result<Decimal, BcError> {
        Ok(self.state.lock().await.matic)
    }

    async fn balances_all(&self) -> Result<BalanceSnapshot, BcError> {
        let s = self.state.lock().await;
        Ok(BalanceSnapshot {
            usdc_balance: s.usdc,
            pusd_balance: s.pusd,
            matic_balance: s.matic,
            captured_at: Utc::now(),
        })
    }

    async fn wrap_usdc(&self, amount: Decimal) -> Result<TxHash, BcError> {
        let mut s = self.state.lock().await;
        if s.usdc < amount {
            return Err(BcError::InsufficientBalance {
                have: s.usdc.to_string(),
                need: amount.to_string(),
            });
        }
        s.usdc -= amount;
        s.pusd += amount;
        s.calls.push(MockCall::WrapUsdc(amount));
        let tx = s.new_tx();
        info!(amount = %amount, tx = %tx, "[mock] wrapped USDC → pUSD");
        Ok(tx)
    }

    async fn unwrap_pusd(&self, amount: Decimal) -> Result<TxHash, BcError> {
        let mut s = self.state.lock().await;
        if s.pusd < amount {
            return Err(BcError::InsufficientBalance {
                have: s.pusd.to_string(),
                need: amount.to_string(),
            });
        }
        s.pusd -= amount;
        s.usdc += amount;
        s.calls.push(MockCall::UnwrapPusd(amount));
        let tx = s.new_tx();
        info!(amount = %amount, tx = %tx, "[mock] unwrapped pUSD → USDC");
        Ok(tx)
    }

    async fn check_v2_allowances(&self) -> Result<AllowanceStatus, BcError> {
        Ok(self.state.lock().await.allowances.clone())
    }

    async fn ensure_v2_allowances(&self) -> Result<Vec<TxHash>, BcError> {
        let mut s = self.state.lock().await;
        s.calls.push(MockCall::EnsureAllowances);

        if s.allowances.all_set() {
            return Ok(Vec::new());
        }

        let mut hashes = Vec::new();
        if !s.allowances.pusd_to_ctf_exchange {
            s.allowances.pusd_to_ctf_exchange = true;
            hashes.push(s.new_tx());
        }
        if !s.allowances.pusd_to_neg_risk_exchange {
            s.allowances.pusd_to_neg_risk_exchange = true;
            hashes.push(s.new_tx());
        }
        if !s.allowances.ctf_to_ctf_exchange {
            s.allowances.ctf_to_ctf_exchange = true;
            hashes.push(s.new_tx());
        }
        if !s.allowances.ctf_to_neg_risk_exchange {
            s.allowances.ctf_to_neg_risk_exchange = true;
            hashes.push(s.new_tx());
        }
        if !s.allowances.ctf_to_neg_risk_adapter {
            s.allowances.ctf_to_neg_risk_adapter = true;
            hashes.push(s.new_tx());
        }
        if !s.allowances.ctf_to_collateral_adapter {
            s.allowances.ctf_to_collateral_adapter = true;
            hashes.push(s.new_tx());
        }

        info!("[mock] set {} allowance tx(es)", hashes.len());
        Ok(hashes)
    }

    async fn redeem_position(
        &self,
        condition_id: B256,
        neg_risk: bool,
        outcome: OutcomeIndex,
        size: Decimal,
    ) -> Result<TxHash, BcError> {
        let mut s = self.state.lock().await;
        s.calls.push(MockCall::RedeemPosition(
            condition_id,
            neg_risk,
            outcome,
            size,
        ));
        let tx = s.new_tx();
        info!(condition = %condition_id, neg_risk, ?outcome, tx = %tx, "[mock] redeemed position");
        Ok(tx)
    }

    async fn fetch_gas_prices(&self) -> Result<GasPrices, BcError> {
        Ok(GasPrices {
            safe_low_gwei: dec!(30),
            standard_gwei: dec!(35),
            fast_gwei: dec!(40),
            source: GasSource::RpcFallback,
            fetched_at: Utc::now(),
        })
    }

    async fn estimate_tx_cost_usdc(&self, gas_units: u64, max_fee: U256) -> Decimal {
        crate::gas::estimate_cost_usdc(gas_units, max_fee, dec!(0.80))
    }

    async fn matic_price_usdc(&self) -> Result<Decimal, BcError> {
        Ok(dec!(0.80))
    }

    async fn wait_for_receipt(&self, tx: TxHash, _timeout: Duration) -> Result<TxReceipt, BcError> {
        Ok(TxReceipt {
            transaction_hash: tx,
            block_number: Some(1),
            gas_used: 21_000,
            success: true,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn wrap_moves_usdc_to_pusd() {
        let mock = MockBlockchainClient::new();
        mock.set_usdc(dec!(100)).await;
        let tx = mock.wrap_usdc(dec!(50)).await.unwrap();
        assert!(!tx.is_zero());
        assert_eq!(mock.balance_usdc().await.unwrap(), dec!(50));
        assert_eq!(mock.balance_pusd().await.unwrap(), dec!(50));
    }

    #[tokio::test]
    async fn wrap_insufficient_balance() {
        let mock = MockBlockchainClient::new();
        mock.set_usdc(dec!(10)).await;
        let err = mock.wrap_usdc(dec!(20)).await.unwrap_err();
        assert!(matches!(err, BcError::InsufficientBalance { .. }));
    }

    #[tokio::test]
    async fn unwrap_moves_pusd_to_usdc() {
        let mock = MockBlockchainClient::new();
        mock.set_pusd(dec!(200)).await;
        mock.unwrap_pusd(dec!(100)).await.unwrap();
        assert_eq!(mock.balance_pusd().await.unwrap(), dec!(100));
        assert_eq!(mock.balance_usdc().await.unwrap(), dec!(100));
    }

    #[tokio::test]
    async fn ensure_allowances_all_missing_sends_six_txs() {
        let mock = MockBlockchainClient::new();
        let hashes = mock.ensure_v2_allowances().await.unwrap();
        // Two ERC-20 approves of pUSD to the exchanges, plus four ERC-1155
        // setApprovalForAll on the CTF: both exchanges and both redemption
        // adapters. The adapters were added when redemption moved off the V1
        // path; without their approval they cannot pull the outcome tokens.
        assert_eq!(hashes.len(), 6);
        assert!(mock.check_v2_allowances().await.unwrap().all_set());
    }

    #[tokio::test]
    async fn ensure_allowances_idempotent_when_all_set() {
        let mock = MockBlockchainClient::new();
        mock.set_all_allowances().await;
        let hashes = mock.ensure_v2_allowances().await.unwrap();
        assert!(hashes.is_empty());
    }

    #[tokio::test]
    async fn balances_all_snapshot() {
        let mock = MockBlockchainClient::new();
        mock.set_usdc(dec!(100)).await;
        mock.set_pusd(dec!(200)).await;
        mock.set_matic(dec!(5)).await;
        let snap = mock.balances_all().await.unwrap();
        assert_eq!(snap.usdc_balance, dec!(100));
        assert_eq!(snap.pusd_balance, dec!(200));
        assert_eq!(snap.matic_balance, dec!(5));
    }

    #[tokio::test]
    async fn redeem_position_recorded() {
        use garnet_types::market::OutcomeIndex;
        let mock = MockBlockchainClient::new();
        let cid = B256::ZERO;
        let tx = mock
            .redeem_position(cid, true, OutcomeIndex::Second, dec!(1))
            .await
            .unwrap();
        assert!(!tx.is_zero());
        let calls = mock.calls().await;
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            calls[0],
            MockCall::RedeemPosition(_, true, OutcomeIndex::Second, _)
        ));
    }

    #[tokio::test]
    async fn gas_price_estimate_nonzero() {
        let mock = MockBlockchainClient::new();
        let prices = mock.fetch_gas_prices().await.unwrap();
        assert!(prices.standard_gwei > dec!(0));
    }
}
