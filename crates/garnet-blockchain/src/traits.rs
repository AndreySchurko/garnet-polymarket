//! [`BlockchainClient`] trait — single abstraction over all on-chain operations.
//!
//! [`GarnetBlockchainClient`](crate::client::GarnetBlockchainClient) is the
//! real implementation; [`MockBlockchainClient`](crate::mock::MockBlockchainClient)
//! is used in tests and `TEST` trading mode.  All consumers accept
//! `Arc<dyn BlockchainClient>`.

use std::time::Duration;

use async_trait::async_trait;
use rust_decimal::Decimal;

use garnet_types::market::OutcomeIndex;

use crate::error::BcError;
use crate::types::{AllowanceStatus, BalanceSnapshot, GasPrices, TxHash, TxReceipt, B256, U256};

/// Abstraction over all Polygon/EVM wallet operations.
///
/// # Thread safety
///
/// All implementors must be `Send + Sync`; they are typically held behind
/// `Arc<dyn BlockchainClient>`.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use rust_decimal_macros::dec;
/// use garnet_blockchain::traits::BlockchainClient;
///
/// async fn show_balances(client: Arc<dyn BlockchainClient>) {
///     let snap = client.balances_all().await.expect("balances");
///     println!("USDC={} pUSD={} MATIC={}", snap.usdc_balance, snap.pusd_balance, snap.matic_balance);
/// }
/// ```
#[async_trait]
pub trait BlockchainClient: Send + Sync {
    // -----------------------------------------------------------------------
    // Balances
    // -----------------------------------------------------------------------

    /// Read the USDC (unwrapped) balance of the trading wallet.
    ///
    /// # Errors
    ///
    /// Returns [`BcError::Provider`] on RPC failure or [`BcError::AllRpcsUnhealthy`]
    /// when no endpoint is reachable.
    async fn balance_usdc(&self) -> Result<Decimal, BcError>;

    /// Read the pUSD (Polymarket wrapped collateral) balance.
    ///
    /// # Errors
    ///
    /// Same as [`balance_usdc`](Self::balance_usdc).
    async fn balance_pusd(&self) -> Result<Decimal, BcError>;

    /// Read the MATIC (native gas token) balance.
    ///
    /// # Errors
    ///
    /// Same as [`balance_usdc`](Self::balance_usdc).
    async fn balance_matic(&self) -> Result<Decimal, BcError>;

    /// Fetch all three balances in a single snapshot.
    ///
    /// # Errors
    ///
    /// Returns the first error encountered.
    async fn balances_all(&self) -> Result<BalanceSnapshot, BcError>;

    /// The balance of outcome tokens on the address holding the collateral.
    ///
    /// The second picture of the world for reconciliation: the position row in our
    /// database against what the chain sees. `token_id` is a 256-bit decimal string.
    ///
    /// # Errors
    ///
    /// An unreadable `token_id` or an RPC failure. A zero instead of an error would
    /// mean "there is no position" and would produce false divergences.
    async fn balance_of_outcome(&self, token_id: &str) -> Result<Decimal, BcError>;

    /// Read the USDC balance of the **signing EOA wallet** (e.g. the `MetaMask`
    /// account), as opposed to the proxy/Safe used by [`balance_usdc`](Self::balance_usdc).
    /// Shown as an informational line on the dashboard. Defaults to
    /// [`balance_usdc`](Self::balance_usdc) for implementations without a distinct EOA.
    ///
    /// # Errors
    ///
    /// Same as [`balance_usdc`](Self::balance_usdc).
    async fn balance_usdc_wallet(&self) -> Result<Decimal, BcError> {
        self.balance_usdc().await
    }

    // -----------------------------------------------------------------------
    // Collateral wrapping
    // -----------------------------------------------------------------------

    /// Wrap `amount` USDC into pUSD by calling the pUSD deposit function.
    ///
    /// Requires the wallet to hold at least `amount` USDC and to have given
    /// the pUSD contract an ERC-20 approval.
    ///
    /// # Errors
    ///
    /// - [`BcError::InsufficientBalance`] if USDC balance is too low.
    /// - [`BcError::Contract`] if the on-chain call reverts.
    /// - [`BcError::TestModeRestriction`] when called in TEST mode.
    async fn wrap_usdc(&self, amount: Decimal) -> Result<TxHash, BcError>;

    /// Unwrap `amount` pUSD back to USDC.
    ///
    /// # Errors
    ///
    /// - [`BcError::InsufficientBalance`] if pUSD balance is too low.
    /// - [`BcError::Contract`] if the on-chain call reverts.
    /// - [`BcError::TestModeRestriction`] when called in TEST mode.
    async fn unwrap_pusd(&self, amount: Decimal) -> Result<TxHash, BcError>;

    // -----------------------------------------------------------------------
    // Allowances
    // -----------------------------------------------------------------------

    /// Ensure all five CLOB v2 allowances are set to `U256::MAX / 2`.
    ///
    /// Checks each allowance first; only submits approve/setApprovalForAll
    /// transactions for those that are missing.  Returns the list of tx hashes
    /// sent (empty when everything was already approved).
    ///
    /// Allowances required:
    /// 1. pUSD → CTF Exchange V2 (ERC-20 approve)
    /// 2. pUSD → Neg Risk CTF Exchange V2 (ERC-20 approve)
    /// 3. CTF tokens → CTF Exchange V2 (ERC-1155 setApprovalForAll)
    /// 4. CTF tokens → Neg Risk CTF Exchange V2 (ERC-1155 setApprovalForAll)
    /// 5. CTF tokens → Neg Risk Adapter (ERC-1155 setApprovalForAll)
    ///
    /// # Errors
    ///
    /// [`BcError::Contract`] if any approval transaction reverts.
    async fn ensure_v2_allowances(&self) -> Result<Vec<TxHash>, BcError>;

    /// Read the current allowance state without modifying it.
    ///
    /// # Errors
    ///
    /// [`BcError::Provider`] on RPC failure.
    async fn check_v2_allowances(&self) -> Result<AllowanceStatus, BcError>;

    // -----------------------------------------------------------------------
    // Position redemption
    // -----------------------------------------------------------------------

    /// Redeem winning CTF positions for pUSD collateral.
    ///
    /// Redeem a resolved winning position for collateral (USDC).
    ///
    /// Routes by market type and token side:
    /// - **neg-risk** markets (all Polymarket weather markets) redeem through the
    ///   `NegRiskAdapter.redeemPositions(conditionId, amounts)`, where `amounts`
    ///   is `[yes, no]` in 6-decimal token units — `[0, size]` for a NO position,
    ///   `[size, 0]` for a YES position.
    /// - **standard** CTF markets call `ConditionalTokens.redeemPositions` with
    ///   `parentCollectionId = B256::ZERO` and the outcome `indexSet` (`1` for
    ///   YES / outcome 0, `2` for NO / outcome 1).
    ///
    /// `condition_id` is the per-bucket CTF conditionId. `size` is the number of
    /// outcome shares held (used only on the neg-risk path; the standard CTF path
    /// redeems the full held balance).
    ///
    /// # Errors
    ///
    /// - [`BcError::Contract`] if the call reverts (e.g., market not resolved).
    /// - [`BcError::GasTooExpensive`] if estimated gas exceeds `max_gas_cost_usdc`.
    async fn redeem_position(
        &self,
        condition_id: B256,
        neg_risk: bool,
        outcome: OutcomeIndex,
        size: Decimal,
    ) -> Result<TxHash, BcError>;

    // -----------------------------------------------------------------------
    // Gas & pricing
    // -----------------------------------------------------------------------

    /// Fetch current EIP-1559 gas price tiers.
    ///
    /// Tries the Polygon gas station first; falls back to `eth_gasPrice` + 20%.
    /// Results are cached for `gas_cache_ttl_sec` seconds.
    ///
    /// # Errors
    ///
    /// [`BcError::Http`] if both gas station and RPC fallback fail.
    async fn fetch_gas_prices(&self) -> Result<GasPrices, BcError>;

    /// Estimate the USD cost of a transaction given gas units and max fee.
    ///
    /// Uses the cached MATIC price; falls back to a conservative default if the
    /// price oracle is unavailable.  Always returns a value (no `Result`).
    ///
    /// `max_fee` is in wei per gas unit (EIP-1559 `maxFeePerGas`).
    async fn estimate_tx_cost_usdc(&self, gas_units: u64, max_fee: U256) -> Decimal;

    /// Fetch the current MATIC price in USD.
    ///
    /// Cached for `matic_price_ttl_sec` seconds.
    ///
    /// # Errors
    ///
    /// [`BcError::Http`] if the `CoinGecko` API is unreachable.
    async fn matic_price_usdc(&self) -> Result<Decimal, BcError>;

    // -----------------------------------------------------------------------
    // Transaction utilities
    // -----------------------------------------------------------------------

    /// Wait for a transaction to be included in a block, up to `timeout`.
    ///
    /// Polls `eth_getTransactionReceipt` via raw JSON-RPC every 2 seconds.
    ///
    /// # Errors
    ///
    /// - [`BcError::TxTimeout`] if the tx is not confirmed within `timeout`.
    /// - [`BcError::Provider`] on RPC failure.
    async fn wait_for_receipt(&self, tx: TxHash, timeout: Duration) -> Result<TxReceipt, BcError>;
}
