//! Public data types shared across the blockchain crate.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub use alloy::primitives::{TxHash, B256, U256};

/// Minimal transaction receipt returned by [`crate::traits::BlockchainClient::wait_for_receipt`].
///
/// Uses a small custom type instead of `alloy::rpc::types::TransactionReceipt`
/// to avoid enabling the `rpc-types` alloy feature, which triggers a known
/// serde compilation bug in `alloy-consensus 0.12.x`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxReceipt {
    /// Transaction hash.
    pub transaction_hash: TxHash,
    /// Block number in which the transaction was included.
    pub block_number: Option<u64>,
    /// Actual gas units consumed.
    pub gas_used: u64,
    /// `true` = transaction succeeded (EVM status code 1).
    pub success: bool,
}

// ---------------------------------------------------------------------------
// Balance snapshot
// ---------------------------------------------------------------------------

/// Point-in-time snapshot of the trading wallet's on-chain balances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceSnapshot {
    /// Unwrapped USDC balance (6 decimals).
    pub usdc_balance: Decimal,
    /// Polymarket wrapped USD (pUSD) balance (6 decimals).
    pub pusd_balance: Decimal,
    /// MATIC (native gas token) balance (18 decimals).
    pub matic_balance: Decimal,
    /// UTC timestamp at which these values were read.
    pub captured_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Gas prices
// ---------------------------------------------------------------------------

/// EIP-1559 gas price tiers fetched from the Polygon gas oracle.
///
/// All fee values are in Gwei.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GasPrices {
    /// `SafeLow` `maxFeePerGas` in Gwei.
    pub safe_low_gwei: Decimal,
    /// Standard `maxFeePerGas` in Gwei.
    pub standard_gwei: Decimal,
    /// Fast `maxFeePerGas` in Gwei.
    pub fast_gwei: Decimal,
    /// Data origin.
    pub source: GasSource,
    /// UTC timestamp of this fetch.
    pub fetched_at: DateTime<Utc>,
}

/// Origin of gas price data.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum GasSource {
    /// Polygon gas station API.
    GasStation,
    /// Fallback `eth_gasPrice` RPC call + 20% premium.
    RpcFallback,
}

// ---------------------------------------------------------------------------
// Allowance status
// ---------------------------------------------------------------------------

/// Current ERC-20 / ERC-1155 allowance state for CLOB v2 contracts.
///
/// Four boolean flags — one per required CLOB v2 approval.  The struct
/// structure mirrors the four on-chain calls needed and is intentional.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowanceStatus {
    /// pUSD → CTF Exchange V2 (ERC-20 approve).
    pub pusd_to_ctf_exchange: bool,
    /// pUSD → Neg Risk CTF Exchange V2 (ERC-20 approve).
    pub pusd_to_neg_risk_exchange: bool,
    /// CTF tokens → CTF Exchange V2 (ERC-1155 setApprovalForAll).
    pub ctf_to_ctf_exchange: bool,
    /// CTF tokens → Neg Risk CTF Exchange V2 (ERC-1155 setApprovalForAll).
    pub ctf_to_neg_risk_exchange: bool,
    /// CTF tokens → `NegRiskCtfCollateralAdapter` (ERC-1155 setApprovalForAll).
    /// Required so the adapter can pull outcome tokens during neg-risk
    /// redemption.
    pub ctf_to_neg_risk_adapter: bool,
    /// CTF tokens → `CtfCollateralAdapter` (ERC-1155 setApprovalForAll).
    /// Standard-market redemption goes through this adapter so the payout
    /// arrives as pUSD; without the approval it cannot pull the outcome tokens.
    pub ctf_to_collateral_adapter: bool,
}

impl AllowanceStatus {
    /// Returns `true` when every required allowance is above the threshold.
    #[must_use]
    pub fn all_set(&self) -> bool {
        self.pusd_to_ctf_exchange
            && self.pusd_to_neg_risk_exchange
            && self.ctf_to_ctf_exchange
            && self.ctf_to_neg_risk_exchange
            && self.ctf_to_neg_risk_adapter
            && self.ctf_to_collateral_adapter
    }
}

// ---------------------------------------------------------------------------
// Helpers: Decimal ↔ U256 conversions
// ---------------------------------------------------------------------------

/// Convert a 6-decimal `Decimal` amount to a raw ERC-20 `U256` integer.
///
/// # Errors
///
/// Returns a string description if `amount` overflows `u128`.
pub fn decimal_to_u256_6dec(amount: Decimal) -> Result<U256, String> {
    use rust_decimal::prelude::ToPrimitive;
    let scaled = (amount * Decimal::from(1_000_000u64)).round();
    let raw = scaled
        .to_u128()
        .ok_or_else(|| format!("value {amount} overflows u128 at 6 decimals"))?;
    Ok(U256::from(raw))
}

/// Convert a raw ERC-20 6-decimal `U256` integer to a human-readable `Decimal`.
#[must_use]
pub fn u256_6dec_to_decimal(raw: U256) -> Decimal {
    let raw_u128: u128 = raw.try_into().unwrap_or(u128::MAX);
    Decimal::from(raw_u128) / Decimal::from(1_000_000u64)
}

/// Convert an 18-decimal native-token `U256` (MATIC/ETH) to `Decimal`.
#[must_use]
pub fn u256_18dec_to_decimal(raw: U256) -> Decimal {
    let divisor = U256::from(1_000_000_000_000_000_000u128); // 10^18
    let whole = raw / divisor;
    let frac = raw % divisor;
    let whole_d = Decimal::from(u128::try_from(whole).unwrap_or(u128::MAX));
    let frac_d = Decimal::from(u128::try_from(frac).unwrap_or(0))
        / Decimal::from(1_000_000_000_000_000_000u128);
    whole_d + frac_d
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn round_trip_6dec_one_dollar() {
        let amount = dec!(1.0);
        let raw = decimal_to_u256_6dec(amount).unwrap();
        assert_eq!(raw, U256::from(1_000_000u64));
        assert_eq!(u256_6dec_to_decimal(raw), dec!(1.0));
    }

    #[test]
    fn round_trip_6dec_fractional() {
        let amount = dec!(0.625);
        let raw = decimal_to_u256_6dec(amount).unwrap();
        assert_eq!(raw, U256::from(625_000u64));
        assert_eq!(u256_6dec_to_decimal(raw), dec!(0.625));
    }

    #[test]
    fn u256_18dec_one_matic() {
        let raw = U256::from(1_000_000_000_000_000_000u128);
        let d = u256_18dec_to_decimal(raw);
        assert_eq!(d, dec!(1.0));
    }

    #[test]
    fn allowance_all_set() {
        let status = AllowanceStatus {
            pusd_to_ctf_exchange: true,
            pusd_to_neg_risk_exchange: true,
            ctf_to_ctf_exchange: true,
            ctf_to_neg_risk_exchange: true,
            ctf_to_neg_risk_adapter: true,
            ctf_to_collateral_adapter: true,
        };
        assert!(status.all_set());
    }

    #[test]
    fn allowance_partial_not_set() {
        let status = AllowanceStatus {
            pusd_to_ctf_exchange: true,
            pusd_to_neg_risk_exchange: false,
            ctf_to_ctf_exchange: true,
            ctf_to_neg_risk_exchange: true,
            ctf_to_neg_risk_adapter: true,
            ctf_to_collateral_adapter: true,
        };
        assert!(!status.all_set());
    }
}
