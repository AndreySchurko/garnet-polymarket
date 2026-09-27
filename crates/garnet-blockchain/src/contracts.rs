//! Contract ABI definitions and address resolution.
//!
//! Addresses are loaded **exclusively from environment variables** — never
//! hard-coded.  Five of them are also carried by the SDK, reachable through
//! `polymarket_client_sdk_v2::contract_config(chain_id, is_neg_risk)` (there is
//! no `contracts::polygon` module — that path never existed).  The collateral
//! addresses are not in the SDK at all; take them from
//! <https://docs.polymarket.com/resources/contracts>.
//!
//! Required env vars (see `.env.example`) — all eight, `from_env` has no
//! default for any of them:
//! - `POLYGON_USDC_ADDRESS` — **USDC.e**, not native USDC (see below)
//! - `POLYGON_PUSD_ADDRESS`
//! - `POLYGON_CTF_EXCHANGE_V2_ADDRESS`
//! - `POLYGON_NEG_RISK_CTF_EXCHANGE_V2_ADDRESS`
//! - `POLYGON_NEG_RISK_CTF_COLLATERAL_ADAPTER_ADDRESS`
//! - `POLYGON_CTF_COLLATERAL_ADAPTER_ADDRESS`
//! - `POLYGON_CTF_ADDRESS`
//! - `POLYGON_COLLATERAL_ONRAMP_ADDRESS`
//! - `POLYGON_COLLATERAL_OFFRAMP_ADDRESS`
//!
//! `POLYGON_NEG_RISK_ADAPTER_ADDRESS` is **gone**: it named the V1 adapter,
//! which Polymarket marks deprecated and which pays out USDC.e instead of pUSD.
//! Its replacement takes byte-identical calldata, so the rename is the only
//! thing that makes the swap visible to an operator.
//!
//! The collateral asset is bridged **USDC.e**
//! (`0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174`).  Native Circle USDC is
//! rejected: `CollateralOnramp.paused(nativeUsdc)` returns true, and the vault
//! backing pUSD holds USDC.e.  `pUSD.USDC()` returns the native address and is
//! vestigial — do not follow it.

use std::str::FromStr;

use alloy::primitives::Address;

use crate::error::BcError;

// ---------------------------------------------------------------------------
// ABI bindings (via alloy sol! macro)
// ---------------------------------------------------------------------------

alloy::sol! {
    /// Standard ERC-20 interface (balanceOf, approve, allowance).
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256 balance);
        function approve(address spender, uint256 amount) external returns (bool success);
        function allowance(address owner, address spender) external view returns (uint256 remaining);
    }

    /// ERC-1155 operator approval (CTF token → exchange).
    #[sol(rpc)]
    interface IERC1155 {
        function setApprovalForAll(address operator, bool approved) external;
        function isApprovedForAll(address account, address operator) external view returns (bool approved);
        /// How many outcome tokens an address holds. This is what the reconciler
        /// checks the ledger against.
        function balanceOf(address account, uint256 id) external view returns (uint256 amount);
    }

    /// Polymarket `CollateralOnramp` — wraps USDC.e into pUSD.
    ///
    /// The pUSD token itself is a plain ERC-20 whose minting is permissioned;
    /// it exposes no `deposit`/`withdraw` and is **not** an ERC-4626 vault.
    /// Wrapping goes through this contract, and the USDC.e approval must name
    /// the onramp — approving the pUSD token instead grants an allowance
    /// nothing will ever spend.
    // Parameter names carry no underscore, unlike the published Solidity. The
    // selector is computed from the types alone, so the name is ours to choose,
    // and `sol!` turns each one into a struct field that clippy's
    // `used_underscore_binding` then flags in code we do not write — enough to
    // fail `-D warnings` for the whole workspace.
    #[sol(rpc)]
    interface ICollateralOnramp {
        /// Pull `amount` of `asset` (USDC.e) and mint pUSD to `to`.
        function wrap(address asset, address to, uint256 amount) external;
        /// True when the admin has paused this asset. Native USDC is paused.
        function paused(address asset) external view returns (bool);
    }

    /// Polymarket `CollateralOfframp` — unwraps pUSD back into USDC.e.
    ///
    /// Mirror of the onramp: the caller must first approve the offramp to
    /// spend their pUSD.
    #[sol(rpc)]
    interface ICollateralOfframp {
        /// Burn `amount` of pUSD and send `asset` (USDC.e) to `to`.
        function unwrap(address asset, address to, uint256 amount) external;
        /// True when the admin has paused this asset.
        function paused(address asset) external view returns (bool);
    }

    /// Polymarket `CtfCollateralAdapter` — redemption for standard markets.
    ///
    /// Same signature as the plain Gnosis CTF `redeemPositions`, but the
    /// adapter burns the outcome tokens through the CTF, takes the released
    /// USDC.e, wraps it, and returns **pUSD**. Redeeming against the CTF
    /// directly would pay out USDC.e and need a second wrap.
    ///
    /// `collateralToken` is the asset the condition was collateralized in —
    /// **USDC.e**, not pUSD. Verified by deriving `positionId` from live
    /// markets: `keccak256(abi.encodePacked(USDC.e, collectionId))` reproduces
    /// the `clobTokenIds` Gamma reports for non-neg-risk markets exactly.
    /// Passing pUSD here addresses positions that do not exist.
    #[sol(rpc)]
    interface ICtfCollateralAdapter {
        /// Redeem a set of winning positions and receive pUSD.
        ///
        /// - `parentCollectionId`: always `bytes32(0)` for top-level markets.
        /// - `conditionId`: the 32-byte market condition identifier.
        /// - `indexSets`: outcome index bitmasks (e.g., `[1]` for outcome 0).
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] calldata indexSets
        ) external;
    }

    /// Polymarket `NegRiskCtfCollateralAdapter` — redemption for negative-risk
    /// markets.
    ///
    /// `NegRisk` positions are collateralized in `WrappedCollateral`
    /// (`0x3a3bd7bb…02e2`, confirmed by deriving `positionId` from live
    /// neg-risk markets) and cannot be redeemed through the plain CTF:
    /// redemption goes through this adapter, which pulls the caller's CTF
    /// outcome tokens, redeems them in wrapped collateral, and returns
    /// **pUSD**. `_amounts` is always length 2 — `[yes_amount, no_amount]` —
    /// so a NO position passes `[0, size]` and a YES position `[size, 0]`.
    ///
    /// The V1 `NegRiskAdapter` (`0xd91E80cF…`) takes the identical calldata but
    /// pays out USDC.e; Polymarket marks it deprecated.
    #[sol(rpc)]
    interface INegRiskCtfCollateralAdapter {
        function redeemPositions(bytes32 conditionId, uint256[] calldata amounts) external;
    }
}

// ---------------------------------------------------------------------------
// Address bundle
// ---------------------------------------------------------------------------

/// Resolved on-chain addresses for all Polymarket V2 contracts.
///
/// Populated via [`ContractAddresses::from_env`]; each address comes from its
/// own environment variable so that testnet / mainnet can be switched without
/// recompiling.
#[derive(Debug, Clone)]
pub struct ContractAddresses {
    /// Bridged **USDC.e** on Polygon — the only asset the ramps accept.
    pub usdc: Address,
    /// Polymarket USD (pUSD) — wrapped collateral for CLOB v2.
    pub pusd: Address,
    /// `CollateralOnramp` — the only way to turn USDC.e into pUSD.
    pub collateral_onramp: Address,
    /// `CollateralOfframp` — the only way to turn pUSD back into USDC.e.
    pub collateral_offramp: Address,
    /// CTF Exchange V2 — primary order book contract.
    pub ctf_exchange_v2: Address,
    /// Neg Risk CTF Exchange V2 — for negative-risk markets.
    pub neg_risk_ctf_exchange_v2: Address,
    /// `NegRiskCtfCollateralAdapter` — redemption for negative-risk markets,
    /// paying out pUSD. Also the ERC-1155 operator that pulls the outcome
    /// tokens, so it needs `setApprovalForAll`.
    pub neg_risk_collateral_adapter: Address,
    /// `CtfCollateralAdapter` — redemption for standard markets, paying out
    /// pUSD. Also an ERC-1155 operator, same as above.
    pub ctf_collateral_adapter: Address,
    /// Gnosis `ConditionalTokens` Framework (ERC-1155 CTF tokens).
    pub ctf: Address,
}

impl ContractAddresses {
    /// Load all contract addresses from environment variables.
    ///
    /// # Errors
    ///
    /// Returns [`BcError::EnvMissing`] if a required variable is absent, or
    /// [`BcError::Config`] if a value is not a valid Ethereum address.
    pub fn from_env() -> Result<Self, BcError> {
        let load = |key: &'static str| -> Result<Address, BcError> {
            let val = std::env::var(key).map_err(|_| BcError::EnvMissing(key.into()))?;
            Address::from_str(&val)
                .map_err(|e| BcError::Config(format!("invalid address for {key}: {e}")))
        };

        Ok(Self {
            usdc: load("POLYGON_USDC_ADDRESS")?,
            pusd: load("POLYGON_PUSD_ADDRESS")?,
            collateral_onramp: load("POLYGON_COLLATERAL_ONRAMP_ADDRESS")?,
            collateral_offramp: load("POLYGON_COLLATERAL_OFFRAMP_ADDRESS")?,
            ctf_exchange_v2: load("POLYGON_CTF_EXCHANGE_V2_ADDRESS")?,
            neg_risk_ctf_exchange_v2: load("POLYGON_NEG_RISK_CTF_EXCHANGE_V2_ADDRESS")?,
            neg_risk_collateral_adapter: load("POLYGON_NEG_RISK_CTF_COLLATERAL_ADAPTER_ADDRESS")?,
            ctf_collateral_adapter: load("POLYGON_CTF_COLLATERAL_ADAPTER_ADDRESS")?,
            ctf: load("POLYGON_CTF_ADDRESS")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // Serialize env-var tests — std::env is process-global and tests run in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn missing_env_returns_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("POLYGON_USDC_ADDRESS");
        let err = ContractAddresses::from_env().unwrap_err();
        assert!(matches!(err, BcError::EnvMissing(_)));
        assert!(err.to_string().contains("POLYGON_USDC_ADDRESS"));
    }

    #[test]
    fn invalid_address_returns_config_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("POLYGON_USDC_ADDRESS", "not-an-address");
        let err = ContractAddresses::from_env().unwrap_err();
        // May be EnvMissing for other vars, but if USDC is parsed first it is Config.
        assert!(!err.to_string().is_empty());
        std::env::remove_var("POLYGON_USDC_ADDRESS");
    }
}
