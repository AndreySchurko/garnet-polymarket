//! The only path to a real order: the signing CLOB client.
//!
//! Separated from `app.rs` because it is the only component whose absence must **degrade
//! rather than abort**: a shadow deployment has no keys and must start. But a half-alive
//! live path is worse than a disabled one: three L2 variables out of four is not "there are
//! no keys" but a typo in the deployment, and quietly slipping into shadow would show the
//! operator a green log where they expect live orders.
//!
//! Resolving the environment ([`resolve`]) is separated from connecting: connecting goes to
//! the network, while the live path goes wrong precisely when parsing the environment. That
//! keeps all of its logic under test.

use garnet_clob::types::ClobCredentials;
use garnet_clob::{Address, SignatureType};
use std::collections::HashMap;
use std::str::FromStr;

/// The environment variables, captured in one piece.
///
/// The parsing functions do not read `std::env` themselves: process variables are global
/// while the tests run in parallel in one process, and mutating the environment would make
/// the failures flaky.
pub struct LiveEnv {
    vars: HashMap<String, String>,
}

impl LiveEnv {
    /// A snapshot of the process environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self::from_pairs(std::env::vars())
    }

    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            vars: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// An empty variable counts as unset.
    ///
    /// systemd's `EnvironmentFile` yields an empty string where a value was left unfilled;
    /// such a key would pass a presence check and fail on signing.
    fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
    }
}

/// Everything needed to sign and submit an order.
pub struct LiveConfig {
    pub credentials: ClobCredentials,
    /// The signer's private key. It does not appear in `Debug`.
    pub private_key: String,
    pub signature_type: SignatureType,
    /// The address that **holds** the collateral: a proxy or a Safe. An EOA has none.
    pub funder: Option<Address>,
    pub chain_id: u64,
}

impl std::fmt::Debug for LiveConfig {
    /// The config is written to the log in full at startup, so the private key does not go
    /// in here at all: a secret in `Debug` is a secret in journald.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveConfig")
            .field("credentials", &self.credentials)
            .field("private_key", &"***REDACTED***")
            .field("signature_type", &self.signature_type)
            .field("funder", &self.funder)
            .field("chain_id", &self.chain_id)
            .finish()
    }
}

/// The variables, any one of which signals an intent to trade live.
const LIVE_VARS: [&str; 5] = [
    "POLY_ADDRESS",
    "POLY_API_KEY",
    "POLY_API_SECRET",
    "POLY_API_PASSPHRASE",
    "PRIVATE_KEY",
];

/// Parse the environment of the live path.
///
/// `Ok(None)` — the live path is disabled: none of [`LIVE_VARS`] is set.
/// `Err` — an intent to trade is declared, but the environment is incomplete or wrong.
///
/// # Errors
///
/// It returns an error naming the specific variable: a message saying "live did not come up"
/// without the variable's name would cost half an hour on the host.
pub fn resolve(env: &LiveEnv) -> anyhow::Result<Option<LiveConfig>> {
    if LIVE_VARS.iter().all(|v| env.get(v).is_none()) {
        return Ok(None);
    }

    let require = |var: &str| -> anyhow::Result<String> {
        env.get(var).map(ToOwned::to_owned).ok_or_else(|| {
            anyhow::anyhow!("{var} is not set, yet the live path is declared by other variables")
        })
    };

    let credentials = ClobCredentials {
        address: require("POLY_ADDRESS")?,
        api_key: require("POLY_API_KEY")?,
        api_secret: require("POLY_API_SECRET")?,
        passphrase: require("POLY_API_PASSPHRASE")?,
        builder_code: env.get("BUILDER_CODE").map(ToOwned::to_owned),
    };

    // The L2 keys sign the REST requests; the order itself is signed with the private key.
    // Without it the client would build and fail on every order in production.
    let private_key = require("PRIVATE_KEY")?;

    let signature_type = signature_type(env.get("POLY_SIGNATURE_TYPE"))?;

    // The EOA flow has no proxy: the signer is the holder. A funder passed in would tell the
    // SDK to build an order for a wallet that does not exist.
    let funder = if signature_type == SignatureType::Eoa {
        None
    } else {
        let raw = require("POLY_PROXY_ADDRESS")?;
        Some(
            Address::from_str(raw.trim())
                .map_err(|e| anyhow::anyhow!("POLY_PROXY_ADDRESS is not an address: {e}"))?,
        )
    };

    // The wrong network means an EIP-712 signature with a foreign domain: the exchange will
    // reject everything, and it will look like a problem with the keys.
    let chain_id = match env.get("POLYGON_CHAIN_ID") {
        None => 137,
        Some(raw) => raw
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("POLYGON_CHAIN_ID is not a number: {e}"))?,
    };

    Ok(Some(LiveConfig {
        credentials,
        private_key,
        signature_type,
        funder,
        chain_id,
    }))
}

/// Parsing `POLY_SIGNATURE_TYPE`.
///
/// In the predecessor the value was hardcoded as `PolyProxy`, and an EOA operator signed every
/// order through a proxy that does not exist. A typo has to be an error, not a silent fallback
/// to a default.
fn signature_type(raw: Option<&str>) -> anyhow::Result<SignatureType> {
    match raw.unwrap_or("POLY_PROXY").trim().to_uppercase().as_str() {
        "EOA" => Ok(SignatureType::Eoa),
        "POLY_PROXY" => Ok(SignatureType::PolyProxy),
        "POLY_GNOSIS_SAFE" => Ok(SignatureType::PolyGnosisSafe),
        other => Err(anyhow::anyhow!(
            "POLY_SIGNATURE_TYPE must be EOA|POLY_PROXY|POLY_GNOSIS_SAFE, got `{other}`"
        )),
    }
}

/// Connect the signing client.
///
/// A thin layer over the carried-over `garnet-clob`: all the logic the live path goes wrong
/// in has already been handled in [`resolve`]. What remains here is only the network call,
/// verified by a live $1 order rather than by a test.
///
/// # Errors
///
/// An SDK error during authentication: the keys exist, but the exchange did not accept them.
pub async fn connect(
    cfg: LiveConfig,
    settings: garnet_clob::ClobSettings,
) -> anyhow::Result<garnet_clob::GarnetClobClient> {
    Ok(garnet_clob::GarnetClobClient::new_with_sdk(
        settings,
        cfg.credentials,
        cfg.private_key,
        cfg.signature_type,
        cfg.funder,
        cfg.chain_id,
    )
    .await?)
}
