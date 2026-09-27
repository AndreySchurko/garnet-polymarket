//! [`GarnetBlockchainClient`] — real Polygon RPC implementation.

use std::sync::Arc;
use std::time::Duration;

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{info, warn};

use crate::settings::BlockchainSettings;
use garnet_types::market::OutcomeIndex;

use crate::contracts::{
    ContractAddresses, ICollateralOfframp, ICollateralOnramp, ICtfCollateralAdapter,
    INegRiskCtfCollateralAdapter, IERC1155, IERC20,
};
use crate::error::BcError;
use crate::failover::RpcFailover;
use crate::gas::{estimate_cost_usdc, GasOracle};
use crate::traits::BlockchainClient;
use crate::types::{
    decimal_to_u256_6dec, u256_18dec_to_decimal, u256_6dec_to_decimal, AllowanceStatus,
    BalanceSnapshot, GasPrices, TxHash, TxReceipt, B256,
};

/// Minimum allowance threshold: `U256::MAX / 2`.
///
/// If the current allowance ≥ this value, we consider it "effectively unlimited"
/// and skip the approve transaction.
fn min_allowance() -> U256 {
    U256::MAX >> 1
}

/// Convert Gwei (as `Decimal`) to Wei (`U256`).
fn gwei_to_wei(gwei: Decimal) -> U256 {
    use rust_decimal::prelude::ToPrimitive;
    let wei_per_gwei = Decimal::from(1_000_000_000u64);
    let wei = (gwei * wei_per_gwei).round();
    U256::from(wei.to_u128().unwrap_or(0))
}

/// Env var holding an optional SOCKS5 egress proxy for **Polygon RPC** traffic.
///
/// Independent of `POLYMARKET_PROXY_URL` (which only routes Polymarket/CLOB
/// traffic). When unset or blank the RPC client uses the host's default route
/// (the general/real IP) — no proxy.
const RPC_PROXY_ENV: &str = "POLYGON_RPC_PROXY_URL";

/// Read & validate the optional Polygon RPC proxy URL from [`RPC_PROXY_ENV`].
///
/// Returns `Ok(None)` when unset/blank (→ default route, the general IP).
///
/// # Errors
///
/// Returns [`BcError::Config`] if set but not a `socks5://`/`socks5h://` URL.
fn rpc_proxy_url() -> Result<Option<String>, BcError> {
    match std::env::var(RPC_PROXY_ENV) {
        Ok(raw) if !raw.trim().is_empty() => {
            let url = raw.trim().to_owned();
            if !(url.starts_with("socks5://") || url.starts_with("socks5h://")) {
                return Err(BcError::Config(format!(
                    "{RPC_PROXY_ENV} must be a SOCKS5 URL (socks5:// or socks5h://)"
                )));
            }
            Ok(Some(url))
        }
        _ => Ok(None),
    }
}

/// Build the workspace-reqwest (0.12) client used for raw JSON-RPC (receipt
/// polling). Applies the Polygon RPC SOCKS5 proxy iff configured.
///
/// # Errors
///
/// Returns [`BcError::Config`] on malformed proxy URL or client build failure.
fn build_rpc_http_client(cfg: &BlockchainSettings) -> Result<reqwest::Client, BcError> {
    let mut builder =
        reqwest::Client::builder().timeout(Duration::from_secs(cfg.rpc_request_timeout_sec));
    if let Some(url) = rpc_proxy_url()? {
        let proxy = reqwest::Proxy::all(&url)
            .map_err(|e| BcError::Config(format!("invalid {RPC_PROXY_ENV}: {e}")))?;
        builder = builder.proxy(proxy);
        info!("blockchain: raw Polygon RPC routed via SOCKS5 proxy");
    }
    builder
        .build()
        .map_err(|e| BcError::Config(format!("build RPC http client: {e}")))
}

/// Build the alloy-compatible reqwest (0.13) client backing the RPC provider.
/// Applies the Polygon RPC SOCKS5 proxy iff configured; otherwise the host's
/// default route (general IP) is used.
///
/// # Errors
///
/// Returns [`BcError::Config`] on malformed proxy URL or client build failure.
fn build_alloy_rpc_client(cfg: &BlockchainSettings) -> Result<reqwest013::Client, BcError> {
    let mut builder =
        reqwest013::Client::builder().timeout(Duration::from_secs(cfg.rpc_request_timeout_sec));
    if let Some(url) = rpc_proxy_url()? {
        let proxy = reqwest013::Proxy::all(&url)
            .map_err(|e| BcError::Config(format!("invalid {RPC_PROXY_ENV}: {e}")))?;
        builder = builder.proxy(proxy);
        info!("blockchain: Polygon RPC provider routed via SOCKS5 proxy");
    } else {
        info!("blockchain: Polygon RPC provider using default route (no proxy)");
    }
    builder
        .build()
        .map_err(|e| BcError::Config(format!("build alloy RPC client: {e}")))
}

// ---------------------------------------------------------------------------
// Client struct
// ---------------------------------------------------------------------------

/// Polygon blockchain client backed by alloy with RPC failover.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use garnet_blockchain::client::GarnetBlockchainClient;
/// use garnet_blockchain::traits::BlockchainClient;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let client = Arc::new(GarnetBlockchainClient::from_env()?);
/// let usdc = client.balance_usdc().await?;
/// println!("USDC balance: {usdc}");
/// # Ok(())
/// # }
/// ```
pub struct GarnetBlockchainClient {
    failover: Arc<RpcFailover>,
    signer: PrivateKeySigner,
    contracts: ContractAddresses,
    gas_oracle: Arc<GasOracle>,
    cfg: BlockchainSettings,
    is_live: bool,
    /// Address whose balances (USDC / pUSD / MATIC) are reported for display.
    ///
    /// On Polymarket the collateral actually lives on the **proxy / Safe**
    /// (maker) address, not the signing EOA. We read `POLY_PROXY_ADDRESS` for
    /// this; it falls back to the signer's EOA only when the proxy var is
    /// unset (correct for a pure-EOA trading flow).
    balance_address: Address,
    /// Shared reqwest client for raw JSON-RPC calls (e.g., receipt polling).
    /// Workspace reqwest (0.12); proxied via `POLYGON_RPC_PROXY_URL` if set.
    http: reqwest::Client,
    /// reqwest 0.13 client backing the alloy RPC providers. Separate crate
    /// version (alloy bundles 0.13); proxied via `POLYGON_RPC_PROXY_URL` if set.
    alloy_http: reqwest013::Client,
}

impl GarnetBlockchainClient {
    /// Construct from environment variables.
    ///
    /// Required env vars:
    /// - `POLYGON_RPC_URLS` — comma-separated list of RPC endpoints.
    /// - `PRIVATE_KEY` — hex-encoded 32-byte private key (with or without 0x prefix).
    /// - `TRADING_MODE` — `LIVE` or `TEST`.
    /// - Contract address vars (see [`ContractAddresses::from_env`]).
    ///
    /// # Errors
    ///
    /// Returns [`BcError::EnvMissing`] or [`BcError::Config`] if any required
    /// variable is absent or malformed.
    pub fn from_env() -> Result<Self, BcError> {
        let rpc_urls = std::env::var("POLYGON_RPC_URLS")
            .map_err(|_| BcError::EnvMissing("POLYGON_RPC_URLS".into()))?;

        let private_key =
            std::env::var("PRIVATE_KEY").map_err(|_| BcError::EnvMissing("PRIVATE_KEY".into()))?;

        let is_live = std::env::var("TRADING_MODE")
            .unwrap_or_else(|_| "TEST".into())
            .eq_ignore_ascii_case("LIVE");

        let failover = Arc::new(RpcFailover::from_csv(&rpc_urls)?);

        let signer: PrivateKeySigner = private_key
            .trim()
            .parse()
            .map_err(|e| BcError::Signer(format!("parse PRIVATE_KEY: {e}")))?;

        let contracts = ContractAddresses::from_env()?;
        let cfg = BlockchainSettings::default();

        // Balance display address: prefer the Polymarket proxy / Safe (maker)
        // that actually holds the collateral; fall back to the signer EOA.
        let balance_address = match std::env::var("POLY_PROXY_ADDRESS") {
            Ok(s) if !s.trim().is_empty() => s
                .trim()
                .parse::<Address>()
                .map_err(|e| BcError::Config(format!("parse POLY_PROXY_ADDRESS: {e}")))?,
            _ => signer.address(),
        };
        info!(balance_address = %balance_address, "blockchain: balance display address");

        let http = build_rpc_http_client(&cfg)?;
        let alloy_http = build_alloy_rpc_client(&cfg)?;

        let gas_oracle = Arc::new(GasOracle::new(
            http.clone(),
            cfg.gas_station_url.clone(),
            cfg.matic_price_url.clone(),
            cfg.gas_cache_ttl_sec,
            cfg.matic_price_ttl_sec,
        ));

        Ok(Self {
            failover,
            signer,
            contracts,
            gas_oracle,
            cfg,
            is_live,
            balance_address,
            http,
            alloy_http,
        })
    }

    /// Build a read-only provider bound to a specific RPC URL.
    ///
    /// Uses the shared (optionally SOCKS5-proxied) reqwest client so all Polygon
    /// RPC egress honours `POLYGON_RPC_PROXY_URL`.
    fn provider_at(&self, url: reqwest::Url) -> impl Provider + Clone {
        ProviderBuilder::new().connect_reqwest(self.alloy_http.clone(), url)
    }

    /// Run an idempotent read operation against the current healthy RPC,
    /// rotating to the next endpoint (and marking the failed one unhealthy via
    /// [`RpcFailover::mark_unhealthy`]) on any error, up to one attempt per
    /// configured endpoint.
    ///
    /// This is what actually drives [`RpcFailover`]: without marking on error,
    /// `current_url` would always return the first URL and failover would never
    /// rotate. Only used for read/view calls (balances, allowances), where any
    /// error is an endpoint problem rather than a contract revert — so marking
    /// the endpoint unhealthy is always the right response. Write calls are not
    /// retried here, to avoid the risk of re-broadcasting a transaction.
    ///
    /// # Errors
    ///
    /// Returns the last error seen, or [`BcError::AllRpcsUnhealthy`] when every
    /// endpoint is already in its cool-down window.
    async fn with_read_failover<T, F, Fut>(&self, op: F) -> Result<T, BcError>
    where
        F: Fn(reqwest::Url) -> Fut,
        Fut: std::future::Future<Output = Result<T, BcError>>,
    {
        let attempts = self.failover.len().await.max(1);
        let mut last_err = BcError::AllRpcsUnhealthy;
        for _ in 0..attempts {
            let url = self.failover.current_url().await?;
            match op(url.clone()).await {
                Ok(value) => return Ok(value),
                Err(e) => {
                    warn!(%url, error = %e, "RPC read failed; marking endpoint unhealthy and rotating");
                    self.failover.mark_unhealthy(&url).await;
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// Build a wallet-equipped provider for sending transactions.
    async fn write_provider(&self) -> Result<impl Provider + Clone, BcError> {
        let url = self.failover.current_url().await?;
        let wallet = EthereumWallet::from(self.signer.clone());
        // In alloy 2.0 recommended fillers (nonce, gas, chain-id) are added by default.
        Ok(ProviderBuilder::new()
            .wallet(wallet)
            .connect_reqwest(self.alloy_http.clone(), url))
    }

    /// Guard that rejects write operations in TEST mode.
    fn require_live(&self, op: &str) -> Result<(), BcError> {
        if !self.is_live {
            return Err(BcError::TestModeRestriction(format!(
                "{op} is not allowed in TEST mode"
            )));
        }
        Ok(())
    }

    /// Wallet address derived from the private key.
    fn wallet_address(&self) -> Address {
        self.signer.address()
    }

    /// Borrow the wallet's [`PrivateKeySigner`] — used by the
    /// [`garnet_clob::OrderSigner`] impl in
    /// [`crate::order_signer`] for EIP-712 order signing.
    #[must_use]
    pub fn signer_ref(&self) -> &PrivateKeySigner {
        &self.signer
    }

    /// Public EOA address controlled by this client.  Equivalent to
    /// [`PrivateKeySigner::address`].
    #[must_use]
    pub fn eoa_address(&self) -> Address {
        self.signer.address()
    }

    /// Check estimated gas cost and warn / error if it exceeds the configured limit.
    async fn guard_gas_cost(&self, gas_units: u64, max_fee: U256) -> Result<(), BcError> {
        let matic = self
            .gas_oracle
            .matic_price_usdc()
            .await
            .unwrap_or(dec!(0.80));
        let cost = estimate_cost_usdc(gas_units, max_fee, matic);
        if cost > self.cfg.max_gas_cost_usdc {
            return Err(BcError::GasTooExpensive {
                cost_usdc: cost.to_string(),
                limit_usdc: self.cfg.max_gas_cost_usdc.to_string(),
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl BlockchainClient for GarnetBlockchainClient {
    async fn balance_usdc(&self) -> Result<Decimal, BcError> {
        self.with_read_failover(|url| async move {
            let provider = self.provider_at(url);
            let token = IERC20::new(self.contracts.usdc, &provider);
            let res = token
                .balanceOf(self.balance_address)
                .call()
                .await
                .map_err(|e| BcError::Contract(e.to_string()))?;
            Ok(u256_6dec_to_decimal(res))
        })
        .await
    }

    async fn balance_pusd(&self) -> Result<Decimal, BcError> {
        self.with_read_failover(|url| async move {
            let provider = self.provider_at(url);
            let token = IERC20::new(self.contracts.pusd, &provider);
            let res = token
                .balanceOf(self.balance_address)
                .call()
                .await
                .map_err(|e| BcError::Contract(e.to_string()))?;
            Ok(u256_6dec_to_decimal(res))
        })
        .await
    }

    async fn balance_of_outcome(&self, token_id: &str) -> Result<Decimal, BcError> {
        let id = U256::from_str_radix(token_id.trim(), 10)
            .map_err(|e| BcError::Config(format!("token_id {token_id} is not a number: {e}")))?;
        self.with_read_failover(|url| async move {
            let provider = self.provider_at(url);
            let ctf = IERC1155::new(self.contracts.ctf, &provider);
            let raw: U256 = ctf
                .balanceOf(self.balance_address, id)
                .call()
                .await
                .map_err(|e| BcError::Contract(e.to_string()))?;
            Ok(u256_6dec_to_decimal(raw))
        })
        .await
    }

    async fn balance_usdc_wallet(&self) -> Result<Decimal, BcError> {
        self.with_read_failover(|url| async move {
            let provider = self.provider_at(url);
            let token = IERC20::new(self.contracts.usdc, &provider);
            // The signing EOA (MetaMask account), distinct from the proxy/Safe
            // used for the displayed trading balance.
            let res = token
                .balanceOf(self.signer.address())
                .call()
                .await
                .map_err(|e| BcError::Contract(e.to_string()))?;
            Ok(u256_6dec_to_decimal(res))
        })
        .await
    }

    async fn balance_matic(&self) -> Result<Decimal, BcError> {
        self.with_read_failover(|url| async move {
            let provider = self.provider_at(url);
            let raw = provider
                .get_balance(self.balance_address)
                .latest()
                .await
                .map_err(|e| BcError::Provider(e.to_string()))?;
            Ok(u256_18dec_to_decimal(raw))
        })
        .await
    }

    async fn balances_all(&self) -> Result<BalanceSnapshot, BcError> {
        let (usdc, pusd, matic) = tokio::try_join!(
            self.balance_usdc(),
            self.balance_pusd(),
            self.balance_matic(),
        )?;
        Ok(BalanceSnapshot {
            usdc_balance: usdc,
            pusd_balance: pusd,
            matic_balance: matic,
            captured_at: Utc::now(),
        })
    }

    async fn wrap_usdc(&self, amount: Decimal) -> Result<TxHash, BcError> {
        self.require_live("wrap_usdc")?;

        let usdc_bal = self.balance_usdc().await?;
        if usdc_bal < amount {
            return Err(BcError::InsufficientBalance {
                have: usdc_bal.to_string(),
                need: amount.to_string(),
            });
        }

        let raw_amount = decimal_to_u256_6dec(amount).map_err(BcError::Decimal)?;

        let provider = self.write_provider().await?;
        let wallet_addr = self.wallet_address();

        // Step 1: approve the ONRAMP to spend USDC.e. Approving the pUSD token
        // instead — which is what this did before — grants an allowance that
        // nothing can spend, because pUSD has no transferFrom path of its own
        // for wrapping.
        let usdc_contract = IERC20::new(self.contracts.usdc, &provider);
        usdc_contract
            .approve(self.contracts.collateral_onramp, raw_amount)
            .send()
            .await
            .map_err(|e| BcError::Contract(format!("USDC.e approve: {e}")))?
            .get_receipt()
            .await
            .map_err(|e| BcError::Contract(format!("USDC.e approve receipt: {e}")))?;

        // Step 2: wrap USDC.e → pUSD. `asset` is the asset being handed over.
        let onramp = ICollateralOnramp::new(self.contracts.collateral_onramp, &provider);
        let pending = onramp
            .wrap(self.contracts.usdc, wallet_addr, raw_amount)
            .send()
            .await
            .map_err(|e| BcError::Contract(format!("collateral onramp wrap: {e}")))?;

        let receipt = pending
            .get_receipt()
            .await
            .map_err(|e| BcError::Contract(format!("collateral onramp wrap receipt: {e}")))?;

        let tx_hash = receipt.transaction_hash;
        info!(amount = %amount, tx = %tx_hash, "wrapped USDC.e → pUSD");
        Ok(tx_hash)
    }

    async fn unwrap_pusd(&self, amount: Decimal) -> Result<TxHash, BcError> {
        self.require_live("unwrap_pusd")?;

        let pusd_bal = self.balance_pusd().await?;
        if pusd_bal < amount {
            return Err(BcError::InsufficientBalance {
                have: pusd_bal.to_string(),
                need: amount.to_string(),
            });
        }

        let raw_amount = decimal_to_u256_6dec(amount).map_err(BcError::Decimal)?;

        let provider = self.write_provider().await?;
        let wallet_addr = self.wallet_address();

        // Mirror of the wrap path: the offramp burns our pUSD, so it needs an
        // allowance on the pUSD token first.
        let pusd_contract = IERC20::new(self.contracts.pusd, &provider);
        pusd_contract
            .approve(self.contracts.collateral_offramp, raw_amount)
            .send()
            .await
            .map_err(|e| BcError::Contract(format!("pUSD approve: {e}")))?
            .get_receipt()
            .await
            .map_err(|e| BcError::Contract(format!("pUSD approve receipt: {e}")))?;

        // `asset` here is the asset we want back, not the one we hand over.
        let offramp = ICollateralOfframp::new(self.contracts.collateral_offramp, &provider);
        let pending = offramp
            .unwrap(self.contracts.usdc, wallet_addr, raw_amount)
            .send()
            .await
            .map_err(|e| BcError::Contract(format!("collateral offramp unwrap: {e}")))?;

        let receipt = pending
            .get_receipt()
            .await
            .map_err(|e| BcError::Contract(format!("collateral offramp unwrap receipt: {e}")))?;

        let tx_hash = receipt.transaction_hash;
        info!(amount = %amount, tx = %tx_hash, "unwrapped pUSD → USDC.e");
        Ok(tx_hash)
    }

    async fn check_v2_allowances(&self) -> Result<AllowanceStatus, BcError> {
        let owner = self.wallet_address();
        let threshold = min_allowance();

        self.with_read_failover(|url| async move {
            let provider = self.provider_at(url);

            // pUSD ERC-20 allowances.
            let pusd = IERC20::new(self.contracts.pusd, &provider);
            let pusd_ctf: U256 = pusd
                .allowance(owner, self.contracts.ctf_exchange_v2)
                .call()
                .await
                .map_err(|e| BcError::Contract(e.to_string()))?;
            let pusd_neg: U256 = pusd
                .allowance(owner, self.contracts.neg_risk_ctf_exchange_v2)
                .call()
                .await
                .map_err(|e| BcError::Contract(e.to_string()))?;

            // CTF ERC-1155 approvals.
            let ctf = IERC1155::new(self.contracts.ctf, &provider);
            let ctf_ctf: bool = ctf
                .isApprovedForAll(owner, self.contracts.ctf_exchange_v2)
                .call()
                .await
                .map_err(|e| BcError::Contract(e.to_string()))?;
            let ctf_neg: bool = ctf
                .isApprovedForAll(owner, self.contracts.neg_risk_ctf_exchange_v2)
                .call()
                .await
                .map_err(|e| BcError::Contract(e.to_string()))?;
            // Required for redemption: both adapters pull outcome tokens.
            let ctf_neg_adapter: bool = ctf
                .isApprovedForAll(owner, self.contracts.neg_risk_collateral_adapter)
                .call()
                .await
                .map_err(|e| BcError::Contract(e.to_string()))?;
            let ctf_collateral_adapter: bool = ctf
                .isApprovedForAll(owner, self.contracts.ctf_collateral_adapter)
                .call()
                .await
                .map_err(|e| BcError::Contract(e.to_string()))?;

            Ok(AllowanceStatus {
                pusd_to_ctf_exchange: pusd_ctf >= threshold,
                pusd_to_neg_risk_exchange: pusd_neg >= threshold,
                ctf_to_ctf_exchange: ctf_ctf,
                ctf_to_neg_risk_exchange: ctf_neg,
                ctf_to_neg_risk_adapter: ctf_neg_adapter,
                ctf_to_collateral_adapter: ctf_collateral_adapter,
            })
        })
        .await
    }

    async fn ensure_v2_allowances(&self) -> Result<Vec<TxHash>, BcError> {
        let status = self.check_v2_allowances().await?;
        let mut hashes: Vec<TxHash> = Vec::new();

        if status.all_set() {
            info!("all V2 allowances already set — no transactions needed");
            return Ok(hashes);
        }

        let provider = self.write_provider().await?;
        let max = U256::MAX;

        // pUSD → CTF Exchange V2.
        if !status.pusd_to_ctf_exchange {
            let pusd = IERC20::new(self.contracts.pusd, &provider);
            let receipt = pusd
                .approve(self.contracts.ctf_exchange_v2, max)
                .send()
                .await
                .map_err(|e| BcError::Contract(format!("approve pUSD→CTFv2: {e}")))?
                .get_receipt()
                .await
                .map_err(|e| BcError::Contract(format!("receipt pUSD→CTFv2: {e}")))?;
            info!(tx = %receipt.transaction_hash, "approved pUSD → CTF Exchange V2");
            hashes.push(receipt.transaction_hash);
        }

        // pUSD → Neg Risk Exchange V2.
        if !status.pusd_to_neg_risk_exchange {
            let pusd = IERC20::new(self.contracts.pusd, &provider);
            let receipt = pusd
                .approve(self.contracts.neg_risk_ctf_exchange_v2, max)
                .send()
                .await
                .map_err(|e| BcError::Contract(format!("approve pUSD→NegRisk: {e}")))?
                .get_receipt()
                .await
                .map_err(|e| BcError::Contract(format!("receipt pUSD→NegRisk: {e}")))?;
            info!(tx = %receipt.transaction_hash, "approved pUSD → Neg Risk Exchange V2");
            hashes.push(receipt.transaction_hash);
        }

        // CTF → CTF Exchange V2.
        if !status.ctf_to_ctf_exchange {
            let ctf = IERC1155::new(self.contracts.ctf, &provider);
            let receipt = ctf
                .setApprovalForAll(self.contracts.ctf_exchange_v2, true)
                .send()
                .await
                .map_err(|e| BcError::Contract(format!("setApprovalForAll CTF→CTFv2: {e}")))?
                .get_receipt()
                .await
                .map_err(|e| BcError::Contract(format!("receipt CTF→CTFv2: {e}")))?;
            info!(tx = %receipt.transaction_hash, "approved CTF tokens → CTF Exchange V2");
            hashes.push(receipt.transaction_hash);
        }

        // CTF → Neg Risk Exchange V2.
        if !status.ctf_to_neg_risk_exchange {
            let ctf = IERC1155::new(self.contracts.ctf, &provider);
            let receipt = ctf
                .setApprovalForAll(self.contracts.neg_risk_ctf_exchange_v2, true)
                .send()
                .await
                .map_err(|e| BcError::Contract(format!("setApprovalForAll CTF→NegRisk: {e}")))?
                .get_receipt()
                .await
                .map_err(|e| BcError::Contract(format!("receipt CTF→NegRisk: {e}")))?;
            info!(tx = %receipt.transaction_hash, "approved CTF tokens → Neg Risk Exchange V2");
            hashes.push(receipt.transaction_hash);
        }

        // CTF → NegRiskCtfCollateralAdapter (so it can pull tokens on redeem).
        if !status.ctf_to_neg_risk_adapter {
            let ctf = IERC1155::new(self.contracts.ctf, &provider);
            let receipt = ctf
                .setApprovalForAll(self.contracts.neg_risk_collateral_adapter, true)
                .send()
                .await
                .map_err(|e| {
                    BcError::Contract(format!("setApprovalForAll CTF→NegRiskAdapter: {e}"))
                })?
                .get_receipt()
                .await
                .map_err(|e| BcError::Contract(format!("receipt CTF→NegRiskAdapter: {e}")))?;
            info!(tx = %receipt.transaction_hash, "approved CTF tokens → Neg Risk Collateral Adapter");
            hashes.push(receipt.transaction_hash);
        }

        // CTF → CtfCollateralAdapter (same, for standard markets).
        if !status.ctf_to_collateral_adapter {
            let ctf = IERC1155::new(self.contracts.ctf, &provider);
            let receipt = ctf
                .setApprovalForAll(self.contracts.ctf_collateral_adapter, true)
                .send()
                .await
                .map_err(|e| {
                    BcError::Contract(format!("setApprovalForAll CTF→CtfCollateralAdapter: {e}"))
                })?
                .get_receipt()
                .await
                .map_err(|e| BcError::Contract(format!("receipt CTF→CtfCollateralAdapter: {e}")))?;
            info!(tx = %receipt.transaction_hash, "approved CTF tokens → CTF Collateral Adapter");
            hashes.push(receipt.transaction_hash);
        }

        Ok(hashes)
    }

    async fn redeem_position(
        &self,
        condition_id: B256,
        neg_risk: bool,
        outcome: OutcomeIndex,
        size: Decimal,
    ) -> Result<TxHash, BcError> {
        use garnet_types::market::OutcomeIndex;

        // Guard: reject if estimated gas cost exceeds the configured limit (§7.4.5).
        let prices = self
            .gas_oracle
            .fetch_gas_prices(None)
            .await
            .unwrap_or_else(|_| {
                use crate::types::GasPrices;
                GasPrices {
                    safe_low_gwei: dec!(50),
                    standard_gwei: dec!(60),
                    fast_gwei: dec!(70),
                    source: crate::types::GasSource::RpcFallback,
                    fetched_at: chrono::Utc::now(),
                }
            });
        self.guard_gas_cost(200_000, gwei_to_wei(prices.fast_gwei))
            .await?;

        let provider = self.write_provider().await?;

        let receipt = if neg_risk {
            // Neg-risk redemption goes through the adapter, which pulls the CTF
            // outcome tokens, redeems them in wrapped collateral, and returns
            // pUSD. `amounts` is [yes, no] in 6-decimal token units.
            let amount = crate::types::decimal_to_u256_6dec(size)
                .map_err(|e| BcError::Config(format!("redeem size {size}: {e}")))?;
            let amounts = match outcome {
                OutcomeIndex::First => vec![amount, U256::ZERO],
                OutcomeIndex::Second => vec![U256::ZERO, amount],
            };
            let adapter = INegRiskCtfCollateralAdapter::new(
                self.contracts.neg_risk_collateral_adapter,
                &provider,
            );
            let pending = adapter
                .redeemPositions(condition_id, amounts)
                .send()
                .await
                .map_err(|e| BcError::Contract(format!("negRisk redeemPositions: {e}")))?;
            pending
                .get_receipt()
                .await
                .map_err(|e| BcError::Contract(format!("negRisk redeemPositions receipt: {e}")))?
        } else {
            // Standard CTF: index_set 1 = YES (outcome 0), 2 = NO (outcome 1).
            // The outcome index, not "yes/no": 1 is the condition's first outcome, 2 the second.
            let index_set = U256::from(outcome.index_set());
            // Through the adapter, not the CTF directly, so the payout arrives
            // as pUSD. `collateralToken` is USDC.e — the asset the condition
            // was collateralized in. This argument used to be pUSD, which
            // addresses positions that do not exist: deriving
            // keccak256(abi.encodePacked(token, collectionId)) from live
            // markets reproduces Gamma's clobTokenIds only for USDC.e.
            let adapter =
                ICtfCollateralAdapter::new(self.contracts.ctf_collateral_adapter, &provider);
            let pending = adapter
                .redeemPositions(
                    self.contracts.usdc,
                    B256::ZERO,
                    condition_id,
                    vec![index_set],
                )
                .send()
                .await
                .map_err(|e| BcError::Contract(format!("redeemPositions: {e}")))?;
            pending
                .get_receipt()
                .await
                .map_err(|e| BcError::Contract(format!("redeemPositions receipt: {e}")))?
        };

        info!(condition = %condition_id, neg_risk, ?outcome, tx = %receipt.transaction_hash, "redeemed position");
        Ok(receipt.transaction_hash)
    }

    async fn fetch_gas_prices(&self) -> Result<GasPrices, BcError> {
        self.gas_oracle.fetch_gas_prices(None).await
    }

    async fn estimate_tx_cost_usdc(&self, gas_units: u64, max_fee: U256) -> Decimal {
        let matic = self
            .gas_oracle
            .matic_price_usdc()
            .await
            .unwrap_or_else(|e| {
                warn!("MATIC price unavailable ({e}); using $0.80 fallback");
                dec!(0.80)
            });
        estimate_cost_usdc(gas_units, max_fee, matic)
    }

    async fn matic_price_usdc(&self) -> Result<Decimal, BcError> {
        self.gas_oracle.matic_price_usdc().await
    }

    async fn wait_for_receipt(&self, tx: TxHash, timeout: Duration) -> Result<TxReceipt, BcError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let url = self.failover.current_url().await?;

        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(BcError::TxTimeout(format!("{tx}")));
            }

            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_getTransactionReceipt",
                "params": [format!("{tx:?}")]
            });

            let resp: serde_json::Value = self
                .http
                .post(url.clone())
                .json(&body)
                .send()
                .await
                .map_err(|e| BcError::Provider(format!("http: {e}")))?
                .json()
                .await
                .map_err(|e| BcError::Provider(format!("json parse: {e}")))?;

            let result = &resp["result"];
            if result.is_null() {
                // Not yet mined.
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            let block_number = result["blockNumber"]
                .as_str()
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok());
            let gas_used = result["gasUsed"]
                .as_str()
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
            let status_hex = result["status"].as_str().unwrap_or("0x0");
            let success = status_hex == "0x1" || status_hex == "0x01";

            return Ok(TxReceipt {
                transaction_hash: tx,
                block_number,
                gas_used,
                success,
            });
        }
    }
}
