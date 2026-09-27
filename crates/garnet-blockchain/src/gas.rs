//! Gas station polling and MATIC price oracle with in-memory cache.

use std::time::{Duration, Instant};

use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Number;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::error::BcError;
use crate::types::{GasPrices, GasSource};

// ---------------------------------------------------------------------------
// Gas station JSON shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GasStationResponse {
    #[serde(rename = "safeLow")]
    safe_low: GasTier,
    standard: GasTier,
    fast: GasTier,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GasTier {
    max_fee: Number,
}

// ---------------------------------------------------------------------------
// CoinGecko shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CoinGeckoResponse {
    #[serde(rename = "matic-network")]
    matic_network: MaticPrices,
}

#[derive(Debug, Deserialize)]
struct MaticPrices {
    usd: Number,
}

// ---------------------------------------------------------------------------
// Cache entries
// ---------------------------------------------------------------------------

pub struct GasCacheEntry {
    pub prices: GasPrices,
    pub expires_at: Instant,
}

pub struct MaticCacheEntry {
    pub price_usdc: Decimal,
    pub expires_at: Instant,
}

// ---------------------------------------------------------------------------
// GasOracle
// ---------------------------------------------------------------------------

/// HTTP-based gas and MATIC price oracle with in-memory TTL caches.
pub struct GasOracle {
    http: reqwest::Client,
    gas_station_url: String,
    matic_price_url: String,
    gas_ttl: Duration,
    matic_ttl: Duration,
    gas_cache: Mutex<Option<GasCacheEntry>>,
    matic_cache: Mutex<Option<MaticCacheEntry>>,
}

impl GasOracle {
    /// Construct a new oracle.
    #[must_use]
    pub fn new(
        http: reqwest::Client,
        gas_station_url: String,
        matic_price_url: String,
        gas_ttl_secs: u64,
        matic_ttl_secs: u64,
    ) -> Self {
        Self {
            http,
            gas_station_url,
            matic_price_url,
            gas_ttl: Duration::from_secs(gas_ttl_secs),
            matic_ttl: Duration::from_secs(matic_ttl_secs),
            gas_cache: Mutex::new(None),
            matic_cache: Mutex::new(None),
        }
    }

    /// Fetch EIP-1559 gas tiers.  Returns from cache when still valid.
    ///
    /// Falls back to an RPC-derived estimate if the gas station is unreachable.
    ///
    /// # Errors
    ///
    /// [`BcError::Http`] if the gas station and RPC fallback both fail.
    pub async fn fetch_gas_prices(
        &self,
        rpc_fallback_gwei: Option<Decimal>,
    ) -> Result<GasPrices, BcError> {
        {
            let cache = self.gas_cache.lock().await;
            if let Some(entry) = cache.as_ref() {
                if entry.expires_at > Instant::now() {
                    debug!("gas prices served from cache");
                    return Ok(entry.prices.clone());
                }
            }
        }

        match self.fetch_gas_station().await {
            Ok(prices) => {
                let entry = GasCacheEntry {
                    prices: prices.clone(),
                    expires_at: Instant::now() + self.gas_ttl,
                };
                *self.gas_cache.lock().await = Some(entry);
                Ok(prices)
            }
            Err(e) => {
                warn!("gas station unavailable ({e}); using RPC fallback");
                // Conservative 50 Gwei default when no fallback provided.
                let base = rpc_fallback_gwei.unwrap_or_else(|| rust_decimal_macros::dec!(50));
                let prices = GasPrices {
                    safe_low_gwei: base,
                    standard_gwei: base * rust_decimal_macros::dec!(1.1),
                    fast_gwei: base * rust_decimal_macros::dec!(1.2),
                    source: GasSource::RpcFallback,
                    fetched_at: chrono::Utc::now(),
                };
                Ok(prices)
            }
        }
    }

    async fn fetch_gas_station(&self) -> Result<GasPrices, BcError> {
        let resp: GasStationResponse = self
            .http
            .get(&self.gas_station_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let to_dec = |n: &Number| -> Decimal {
            std::str::FromStr::from_str(&n.to_string()).unwrap_or(rust_decimal_macros::dec!(50))
        };

        Ok(GasPrices {
            safe_low_gwei: to_dec(&resp.safe_low.max_fee),
            standard_gwei: to_dec(&resp.standard.max_fee),
            fast_gwei: to_dec(&resp.fast.max_fee),
            source: GasSource::GasStation,
            fetched_at: chrono::Utc::now(),
        })
    }

    /// Fetch the MATIC/USD price.  Returns from cache when still valid.
    ///
    /// # Errors
    ///
    /// [`BcError::Http`] if the `CoinGecko` API is unreachable.
    pub async fn matic_price_usdc(&self) -> Result<Decimal, BcError> {
        {
            let cache = self.matic_cache.lock().await;
            if let Some(entry) = cache.as_ref() {
                if entry.expires_at > Instant::now() {
                    debug!("MATIC price served from cache: {}", entry.price_usdc);
                    return Ok(entry.price_usdc);
                }
            }
        }

        let url = format!(
            "{}?ids=matic-network&vs_currencies=usd",
            self.matic_price_url
        );
        let resp: CoinGeckoResponse = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let price: Decimal = std::str::FromStr::from_str(&resp.matic_network.usd.to_string())
            .map_err(|e| BcError::Decimal(format!("MATIC price parse: {e}")))?;

        let entry = MaticCacheEntry {
            price_usdc: price,
            expires_at: Instant::now() + self.matic_ttl,
        };
        *self.matic_cache.lock().await = Some(entry);

        Ok(price)
    }

    /// Invalidate both caches (used in tests).
    pub async fn clear_cache(&self) {
        *self.gas_cache.lock().await = None;
        *self.matic_cache.lock().await = None;
    }
}

// ---------------------------------------------------------------------------
// Standalone cost estimator
// ---------------------------------------------------------------------------

/// Calculate estimated transaction cost in USDC from gas parameters.
///
/// `max_fee_per_gas_wei` is in wei per gas unit (EIP-1559 `maxFeePerGas`).
/// `gas_units` is the estimated gas limit.
/// `matic_price_usdc` is the MATIC/USD price as a `Decimal`.
///
/// Returns the estimated cost in USDC.
#[must_use]
pub fn estimate_cost_usdc(
    gas_units: u64,
    max_fee_per_gas_wei: alloy::primitives::U256,
    matic_price_usdc: Decimal,
) -> Decimal {
    use crate::types::u256_18dec_to_decimal;
    let total_wei = max_fee_per_gas_wei * alloy::primitives::U256::from(gas_units);
    let matic_cost = u256_18dec_to_decimal(total_wei);
    matic_cost * matic_price_usdc
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;
    use rust_decimal_macros::dec;

    #[test]
    fn cost_estimate_sanity() {
        // 21_000 gas × 30 Gwei × $0.80/MATIC ≈ $0.000504
        let max_fee_wei = U256::from(30_000_000_000u64); // 30 Gwei in wei
        let cost = estimate_cost_usdc(21_000, max_fee_wei, dec!(0.80));
        assert!(cost > dec!(0));
        assert!(cost < dec!(1.0), "cost {cost} should be < $1");
    }

    #[test]
    fn zero_gas_units_gives_zero_cost() {
        let cost = estimate_cost_usdc(0, U256::from(30_000_000_000u64), dec!(0.80));
        assert_eq!(cost, dec!(0));
    }

    #[tokio::test]
    async fn gas_station_parse_fallback() {
        // Simulate unreachable gas station → fallback prices returned.
        let oracle = GasOracle::new(
            reqwest::Client::new(),
            "http://127.0.0.1:19999/gas-unreachable".into(),
            "http://127.0.0.1:19999/coingecko-unreachable".into(),
            30,
            300,
        );
        // Should not panic; returns fallback prices.
        let prices = oracle.fetch_gas_prices(Some(dec!(35))).await.unwrap();
        assert!(matches!(prices.source, GasSource::RpcFallback));
        assert_eq!(prices.safe_low_gwei, dec!(35));
    }
}
