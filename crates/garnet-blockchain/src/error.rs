//! Error types for `garnet-blockchain`.

use thiserror::Error;

/// All errors produced by the blockchain layer.
#[derive(Debug, Error)]
pub enum BcError {
    /// RPC / transport failure.
    #[error("provider error: {0}")]
    Provider(String),

    /// All configured RPC endpoints are currently unhealthy.
    #[error("all RPC endpoints are unhealthy")]
    AllRpcsUnhealthy,

    /// A submitted transaction was not confirmed within the timeout.
    #[error("transaction timed out: {0}")]
    TxTimeout(String),

    /// On-chain contract call failed.
    #[error("contract error: {0}")]
    Contract(String),

    /// Configuration or environment problem.
    #[error("config error: {0}")]
    Config(String),

    /// HTTP request to an off-chain API (gas station, price oracle) failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON parse failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Decimal conversion overflow or invalid value.
    #[error("decimal error: {0}")]
    Decimal(String),

    /// On-chain balance is too low to proceed.
    #[error("insufficient balance: have {have}, need {need}")]
    InsufficientBalance { have: String, need: String },

    /// Estimated gas cost exceeds `max_gas_cost_usdc`.
    #[error("gas cost {cost_usdc} USDC exceeds limit {limit_usdc} USDC")]
    GasTooExpensive {
        cost_usdc: String,
        limit_usdc: String,
    },

    /// Private key parse or signing error.
    #[error("signer error: {0}")]
    Signer(String),

    /// Required environment variable is absent.
    #[error("env var `{0}` not set")]
    EnvMissing(String),

    /// Operation is not permitted in TEST mode.
    #[error("TEST mode restriction: {0}")]
    TestModeRestriction(String),
}

impl BcError {
    /// Returns `true` if retrying the same operation might succeed.
    ///
    /// Provider and all-unhealthy errors are retryable; config, signer, and
    /// env errors are permanent.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Provider(_) | Self::AllRpcsUnhealthy | Self::Http(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_is_retryable() {
        assert!(BcError::Provider("timeout".into()).is_retryable());
    }

    #[test]
    fn config_not_retryable() {
        assert!(!BcError::Config("bad url".into()).is_retryable());
    }

    #[test]
    fn signer_not_retryable() {
        assert!(!BcError::Signer("bad key".into()).is_retryable());
    }

    #[test]
    fn display_not_empty() {
        let e = BcError::EnvMissing("PRIVATE_KEY".into());
        assert!(!e.to_string().is_empty());
    }
}
