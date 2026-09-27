//! [`GarnetClobClient`] — REST wrapper around the Polymarket CLOB v2 API.
//!
//! Authentication uses HMAC-SHA256 API key headers.  Order *placement*
//! additionally requires **EIP-712 signing**, performed by an
//! [`OrderSigner`](crate::signer::OrderSigner) trait object injected via
//! [`GarnetClobClient::new_with_signer`].  Without a signer the client can
//! still query open orders, the orderbook, and cancel orders — useful for
//! the `--smoke-clob` flow which only needs API-key authentication.
//!
//! # CLOB v2 critical rules enforced here
//!
//! - The signed payload uses the **CTF Exchange V2** `Order` struct (domain
//!   version `"2"`). The V1 fields `taker`, `nonce`, `feeRateBps`, and signed
//!   `expiration` do not exist in V2; `timestamp`/`metadata`/`builder` do.
//! - Contract addresses are **never** hard-coded; the
//!   [`ClobSigningContext`] is built from environment variables in
//!   [`ClobSigningContext::from_env`].
//! - Builder code is appended to every order when `BUILDER_CODE` env var is set.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::book::{BookSnapshot, PriceLevel};
use crate::settings::ClobSettings;
use alloy::primitives::{Address, B256, U256};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use reqwest::{header::HeaderMap, Client};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::{
    error::ClobError,
    signer::{Exchange, OrderSigner, SignatureType, SignedOrder, UnsignedOrder},
    traits::ClobClient,
    types::{ClobCredentials, ClobOrderStatus, OrderInfo, OrderResponse, Side, TradeInfo},
};

// ---------------------------------------------------------------------------
// Signing context
// ---------------------------------------------------------------------------

/// All static data needed to build an [`UnsignedOrder`] for the CTF Exchange V2.
///
/// The values are wallet-and-network constants and so are loaded once at
/// startup (typically via [`ClobSigningContext::from_env`]).
#[derive(Debug, Clone)]
pub struct ClobSigningContext {
    /// Maker — proxy wallet address in `PolyProxy` mode, EOA in `Eoa` mode.
    pub maker: Address,
    /// Signing EOA address — recovered from the EIP-712 signature on-chain.
    pub signer: Address,
    /// CTF Exchange V2 contract on Polygon — EIP-712 `verifyingContract`.
    pub verifying_contract: Address,
    /// EIP-712 signature variant (EOA = 0, `PolyProxy` = 1, `PolyGnosisSafe` = 2).
    pub signature_type: SignatureType,
    /// Polygon chain ID (mainnet = `137`).
    pub chain_id: u64,
}

impl ClobSigningContext {
    /// Construct from environment variables — matches `garnet-blockchain`
    /// conventions:
    ///
    /// - `POLY_PROXY_ADDRESS` — the proxy / maker address.
    /// - `POLY_SIGNER_ADDRESS` — the signing EOA; defaults to
    ///   `POLY_PROXY_ADDRESS` when unset.
    /// - `POLY_SIGNATURE_TYPE` — `EOA`, `POLY_PROXY` (default), or
    ///   `POLY_GNOSIS_SAFE`.
    /// - `POLYGON_CTF_EXCHANGE_V2_ADDRESS` — verifying contract.
    /// - `POLYGON_CHAIN_ID` — defaults to `137`.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Config`] if a required variable is missing or
    /// malformed.
    pub fn from_env() -> Result<Self, ClobError> {
        let parse_addr = |key: &str, val: String| {
            Address::from_str(val.trim()).map_err(|e| {
                ClobError::Config(format!("env `{key}` is not a valid 0x address: {e}"))
            })
        };

        let maker_raw = std::env::var("POLY_PROXY_ADDRESS")
            .map_err(|_| ClobError::Config("POLY_PROXY_ADDRESS is not set".into()))?;
        let maker = parse_addr("POLY_PROXY_ADDRESS", maker_raw.clone())?;

        let signer = match std::env::var("POLY_SIGNER_ADDRESS") {
            Ok(v) if !v.is_empty() => parse_addr("POLY_SIGNER_ADDRESS", v)?,
            _ => maker,
        };

        let signature_type = match std::env::var("POLY_SIGNATURE_TYPE")
            .unwrap_or_else(|_| "POLY_PROXY".into())
            .to_uppercase()
            .as_str()
        {
            "EOA" => SignatureType::Eoa,
            "POLY_PROXY" => SignatureType::PolyProxy,
            "POLY_GNOSIS_SAFE" => SignatureType::PolyGnosisSafe,
            other => {
                return Err(ClobError::Config(format!(
                    "POLY_SIGNATURE_TYPE must be EOA|POLY_PROXY|POLY_GNOSIS_SAFE (got `{other}`)"
                )));
            }
        };

        let verifying_raw = std::env::var("POLYGON_CTF_EXCHANGE_V2_ADDRESS")
            .map_err(|_| ClobError::Config("POLYGON_CTF_EXCHANGE_V2_ADDRESS is not set".into()))?;
        let verifying_contract = parse_addr("POLYGON_CTF_EXCHANGE_V2_ADDRESS", verifying_raw)?;

        let chain_id = std::env::var("POLYGON_CHAIN_ID")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(137);

        Ok(Self {
            maker,
            signer,
            verifying_contract,
            signature_type,
            chain_id,
        })
    }
}

// ---------------------------------------------------------------------------
// REST request / response shapes for order placement
// ---------------------------------------------------------------------------

/// Sub-object embedded in the `POST /order` request body (CTF Exchange **V2**).
///
/// Field names and value encodings match the official SDK's
/// `OrderV2WithSignature` serializer (`rs-clob-client-v2/src/clob/types/mod.rs`)
/// exactly: `salt` is a JSON **number** (u64), amounts/`tokenId`/`timestamp`/
/// `expiration` are decimal strings, `side` is the string `"BUY"`/`"SELL"`,
/// `metadata`/`builder` are 0x-prefixed bytes32, and `signature` is 0x-hex.
/// The V1 fields `taker`, `nonce`, and `feeRateBps` do **not** appear in V2.
#[derive(Debug, Serialize)]
struct WireOrder {
    salt: u64,
    maker: String,
    signer: String,
    #[serde(rename = "tokenId")]
    token_id: String,
    #[serde(rename = "makerAmount")]
    maker_amount: String,
    #[serde(rename = "takerAmount")]
    taker_amount: String,
    side: Side,
    expiration: String,
    #[serde(rename = "signatureType")]
    signature_type: u8,
    timestamp: String,
    metadata: String,
    builder: String,
    signature: String,
}

/// Body of `POST /order` as accepted by the CLOB v2 REST endpoint.
///
/// `owner` is the L2 **API key** (not the wallet address), matching the SDK's
/// `SignedOrder.owner: ApiKey`.
#[derive(Debug, Serialize)]
struct PlaceOrderBody {
    order: WireOrder,
    #[serde(rename = "orderType")]
    order_type: &'static str,
    owner: String,
}

// `order_type` stopped being a constant: copying uses FAK, and that is the only
// reason the crate could not be carried over unchanged.

impl PlaceOrderBody {
    fn new(signed: &SignedOrder, owner_api_key: &str, kind: crate::types::OrderKind) -> Self {
        let order = &signed.order;
        Self {
            order: WireOrder {
                salt: u64::try_from(order.salt).unwrap_or(0),
                maker: format!("{:#x}", order.maker),
                signer: format!("{:#x}", order.signer),
                token_id: order.token_id.to_string(),
                maker_amount: order.maker_amount.to_string(),
                taker_amount: order.taker_amount.to_string(),
                side: order.side,
                expiration: order.expiration.to_string(),
                signature_type: order.signature_type.as_u8(),
                timestamp: order.timestamp.to_string(),
                metadata: format!("{:#x}", order.metadata),
                builder: format!("{:#x}", order.builder),
                signature: signed.signature_hex(),
            },
            order_type: kind.as_wire(),
            owner: owner_api_key.to_string(),
        }
    }
}

/// Successful response from `POST /order`.
#[derive(Debug, Deserialize)]
struct ClobOrderResponse {
    #[serde(rename = "orderID", alias = "orderId", alias = "id")]
    order_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "transactionHash", alias = "hash")]
    transaction_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// Internal REST response shapes
// ---------------------------------------------------------------------------

/// Page wrapper for V2 paginated data endpoints (e.g. `GET /data/orders`).
///
/// Only `data` is consumed; `next_cursor` / `limit` / `count` are ignored — our
/// health-check and reconciliation reads only need the first page.
#[derive(Debug, Deserialize)]
struct RestPage<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

/// Shape of a V2 order item from `GET /data/orders` and `GET /data/order/{id}`.
///
/// V2 differs from V1: there is **no** `remaining_size` field (derive it from
/// `original_size − size_matched`) and `created_at` arrives as Unix **seconds**
/// (integer), not an ISO-8601 string. There is no `updated_at`.
#[derive(Debug, Deserialize)]
struct RestOrder {
    id: String,
    asset_id: String,
    price: String,
    original_size: String,
    size_matched: String,
    side: String,
    status: String,
    #[serde(default)]
    created_at: Option<i64>,
}

/// Shape of a maker order inside a `GET /data/trades` item.
#[derive(Debug, Deserialize)]
struct RestMakerOrder {
    order_id: String,
}

/// Shape of a V2 trade item from `GET /data/trades`.
///
/// `match_time` arrives as Unix **seconds** in a string, like the other V2
/// timestamps. `maker_orders` is absent on some responses, so it defaults.
#[derive(Debug, Deserialize)]
struct RestTrade {
    id: String,
    taker_order_id: String,
    #[serde(default)]
    maker_orders: Vec<RestMakerOrder>,
    market: String,
    asset_id: String,
    side: String,
    size: String,
    price: String,
    match_time: String,
}

/// Shape of a single price level in `GET /book?token_id=...`.
#[derive(Debug, Deserialize)]
struct RestLevel {
    price: String,
    size: String,
}

/// One outcome leg of `GET /markets/{condition_id}`.
#[derive(Debug, Deserialize)]
struct RestMarketToken {
    /// ERC-1155 id of this leg. The one field that lets a trade naming only a
    /// token be mapped back onto an outcome.
    #[serde(default)]
    token_id: String,
    #[serde(default)]
    outcome: String,
    /// Present and true on exactly one leg once the market has settled.
    #[serde(default)]
    winner: bool,
}

/// Shape of `GET /markets/{condition_id}` — the resolution view.
///
/// Deliberately a handful of fields out of the ~40 the endpoint returns: this
/// exists to answer "has it settled and who won", and every field named here is
/// one more thing a wire change can break.
#[derive(Debug, Deserialize)]
struct RestMarket {
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    tokens: Vec<RestMarketToken>,
    /// Echoed back by the endpoint. Absent means the CLOB has nothing for this
    /// id, which must read as "unknown" rather than as a market whose fields
    /// all happen to be empty.
    #[serde(default)]
    condition_id: Option<String>,
    /// Every date is optional and every one is `#[serde(default)]`: a wire
    /// change must cost the field, not the whole response, exactly as the
    /// resolution view above intends.
    #[serde(default)]
    end_date_iso: Option<DateTime<Utc>>,
    #[serde(default)]
    game_start_time: Option<DateTime<Utc>>,
}

/// Shape of `GET /book?token_id=...` response.
#[derive(Debug, Deserialize)]
struct RestBook {
    asset_id: String,
    market: String,
    hash: String,
    timestamp: String,
    /// Minimum price increment for this market, as the exchange reports it.
    ///
    /// `Option` even though the exchange sends it on every book today, and read
    /// through [`de_scalar_string`] rather than as a plain `String`: a wire
    /// change must cost us the tick, not the whole snapshot.
    #[serde(default, deserialize_with = "de_scalar_string")]
    tick_size: Option<String>,
    /// Smallest order this market accepts, in shares. Same lenient reader and
    /// the same reason: the SDK types it as a `Decimal`, so the wire may carry
    /// a number, and a wire change must not cost us the book.
    #[serde(default, deserialize_with = "de_scalar_string")]
    min_order_size: Option<String>,
    #[serde(default)]
    bids: Vec<RestLevel>,
    #[serde(default)]
    asks: Vec<RestLevel>,
}

/// Read a JSON scalar as its string form, tolerating `"0.001"` and `0.001`
/// alike, and yielding `None` for anything else.
///
/// The exchange sends the tick as a string on the WS `book` frame, which is the
/// only shape we have captured; the official SDK reads the REST field as
/// `TryFromInto<Decimal>`, which accepts a number too. Deserializing straight
/// into `String` would therefore let a numeric tick fail the **entire** book
/// response — turning a bonus field into an outage on every price decision.
/// Never returns an error for that reason.
fn de_scalar_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<serde_json::Value>::deserialize(de)? {
        Some(serde_json::Value::String(s)) => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// The usable tick from a `/book` response, or `None` with a reason logged.
///
/// `None` is not an error. It means the caller prices this order the way every
/// order was priced before any tick was read — unsnapped. Substituting a
/// constant would be worse: on a 0.001 market a hardcoded 0.01 coarsens a
/// perfectly valid price by up to a cent.
#[must_use]
fn parse_tick(raw: Option<&str>, token_id: &str) -> Option<Decimal> {
    let Some(raw) = raw else {
        warn!(
            token_id,
            "book carries no tick_size; price will not be snapped"
        );
        return None;
    };
    match Decimal::from_str(raw) {
        Ok(tick) if tick > Decimal::ZERO => Some(tick),
        Ok(tick) => {
            warn!(token_id, %tick, "book tick_size is not positive; price will not be snapped");
            None
        }
        Err(e) => {
            warn!(token_id, raw, error = %e, "book tick_size is unparseable; price will not be snapped");
            None
        }
    }
}

/// The usable minimum order size from a `/book` response.
///
/// `None` when the field is absent or unusable: the caller then sends what it
/// sized, which is what happened before this was read at all. Only a positive
/// number is a floor — zero would reject every order.
#[must_use]
fn parse_min_size(raw: Option<&str>, token_id: &str) -> Option<Decimal> {
    let raw = raw?;
    match Decimal::from_str(raw) {
        Ok(size) if size > Decimal::ZERO => Some(size),
        Ok(size) => {
            warn!(token_id, %size, "book min_order_size is not positive; ignoring it");
            None
        }
        Err(e) => {
            warn!(token_id, raw, error = %e, "book min_order_size is unparseable; ignoring it");
            None
        }
    }
}

/// Shape of `DELETE /cancel-all` (cancel all) response.
#[derive(Debug, Deserialize)]
struct RestCancelAllResponse {
    #[serde(default)]
    canceled: Vec<String>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Polymarket CLOB v2 REST client.
///
/// # Examples
///
/// ```no_run
/// use garnet_clob::client::GarnetClobClient;
/// use garnet_clob::types::ClobCredentials;
/// use garnet_clob::settings::ClobSettings;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let cfg   = ClobSettings::default();
/// let creds = ClobCredentials::from_env()?;
/// let client = GarnetClobClient::new(cfg, creds)?;
/// # Ok(())
/// # }
/// ```
/// Simple async token-bucket for proactively throttling order submissions to
/// the CLOB `clob_rate_limit_orders_per_min` budget (complements the reactive
/// 429 handling). Capacity = the per-minute limit; refills continuously.
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    /// Tokens added per second.
    refill_per_sec: f64,
    last_refill: std::time::Instant,
}

impl TokenBucket {
    fn per_minute(limit: u32) -> Self {
        let cap = f64::from(limit.max(1));
        Self {
            tokens: cap,
            capacity: cap,
            refill_per_sec: cap / 60.0,
            last_refill: std::time::Instant::now(),
        }
    }

    /// Block until a token is available, then consume one.
    async fn acquire(&mut self) {
        loop {
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(self.last_refill).as_secs_f64();
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last_refill = now;
            if self.tokens >= 1.0 {
                self.tokens -= 1.0;
                return;
            }
            let wait = (1.0 - self.tokens) / self.refill_per_sec;
            tokio::time::sleep(Duration::from_secs_f64(wait.max(0.01))).await;
        }
    }
}

pub struct GarnetClobClient {
    http: Client,
    base_url: String,
    creds: ClobCredentials,
    /// Proactive order-submission throttle (orders/min).
    order_limiter: Arc<tokio::sync::Mutex<TokenBucket>>,
    /// Shared cross-process budget for `clob.polymarket.com`, read from
    /// `EGRESS_REDIS_URL`. `None` inside means no limiter is configured or it
    /// was unreachable — that is fail-open, not an error.
    ///
    /// Lazily built: two of the three constructors are synchronous, and the
    /// connection must not become the reason a client cannot be constructed.
    egress_bucket: tokio::sync::OnceCell<Option<garnet_redis::EgressBucket>>,
    /// Requests/second this process should assume for the shared budget.
    egress_clob_rps: f64,
    /// Host the shared budget belongs to, derived from `base_url`.
    ///
    /// Derived rather than hard-coded so a staging CLOB host does not silently
    /// spend the production key's tokens.
    egress_host: String,
    /// EIP-712 order signer.  `None` disables [`Self::place_limit_order`] —
    /// useful for smoke tests that exercise only `GET` / `DELETE` paths.
    signer: Option<Arc<dyn OrderSigner>>,
    /// Static signing parameters (maker, verifying contract, chain ID, …).
    /// Must be present whenever `signer` is.
    sign_ctx: Option<ClobSigningContext>,
    /// Official-SDK order placer. When present, [`Self::place_limit_order`]
    /// routes build/sign/post through `polymarket_client_sdk_v2` instead of the
    /// hand-rolled signer path.
    sdk: Option<crate::sdk::SdkOrder>,
    /// Last tick the exchange reported per token, learned from `/book`.
    ///
    /// Deliberately not a field on [`BookSnapshot`]: that type is mirrored by
    /// Zod in `packages/shared-types` and pinned by contract tests.
    ticks: Arc<DashMap<String, Decimal>>,
    /// Smallest accepted order per token, learned from the same `/book` reply.
    ///
    /// Sizing knows our own `min_order_usdc` floor; it does not know the
    /// market's, and an order under it is rejected by the exchange after we have
    /// already spent the round trip and the rate-limit budget on it.
    min_sizes: Arc<DashMap<String, Decimal>>,
}

impl std::fmt::Debug for GarnetClobClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GarnetClobClient")
            .field("http", &self.http)
            .field("base_url", &self.base_url)
            .field("creds", &self.creds)
            .field("has_signer", &self.signer.is_some())
            .field("sign_ctx", &self.sign_ctx)
            .field("sdk", &self.sdk)
            .finish_non_exhaustive()
    }
}

impl GarnetClobClient {
    /// Construct a new client **without** an order signer.
    ///
    /// All read paths (`get_*`, `get_orderbook`) and cancel paths work, but
    /// [`place_limit_order`](ClobClient::place_limit_order) returns
    /// [`ClobError::Auth`].  Use [`new_with_signer`](Self::new_with_signer)
    /// for production trading.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Config`] if the HTTP client cannot be built.
    pub fn new(cfg: ClobSettings, creds: ClobCredentials) -> Result<Self, ClobError> {
        let http = build_http_client()?;
        let order_limiter = Arc::new(tokio::sync::Mutex::new(TokenBucket::per_minute(
            cfg.clob_rate_limit_orders_per_min,
        )));
        let egress_clob_rps = cfg.egress_clob_rps;
        let egress_host = host_of(&cfg.clob_host).unwrap_or_else(|| DEFAULT_CLOB_HOST.to_owned());
        Ok(Self {
            http,
            base_url: cfg.clob_host,
            creds,
            order_limiter,
            egress_bucket: tokio::sync::OnceCell::new(),
            egress_clob_rps,
            egress_host,
            signer: None,
            sign_ctx: None,
            sdk: None,
            ticks: Arc::new(DashMap::new()),
            min_sizes: Arc::new(DashMap::new()),
        })
    }

    /// Construct a **read-only** client for public market data (`get_orderbook`,
    /// other `GET`s) with no credentials and no order signer.
    ///
    /// `place_limit_order` returns [`ClobError::Auth`] (no signer); credentialed
    /// endpoints (`cancel_*`, `get_open_orders`) will be rejected by the server
    /// (empty HMAC creds). Intended for TEST mode, where order books are read
    /// from the live public CLOB but fills are simulated locally — so no API
    /// keys, private key, or funds are ever required.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Config`] if the HTTP client cannot be built.
    pub fn new_read_only(cfg: ClobSettings) -> Result<Self, ClobError> {
        let empty_creds = ClobCredentials {
            address: String::new(),
            api_key: String::new(),
            api_secret: String::new(),
            passphrase: String::new(),
            builder_code: None,
        };
        Self::new(cfg, empty_creds)
    }

    /// Construct a new client with an attached EIP-712
    /// [`OrderSigner`] and the static signing parameters.
    ///
    /// This is the constructor used in LIVE trading;
    /// `place_limit_order` will sign and POST orders to `/order`.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Config`] if the HTTP client cannot be built.
    pub fn new_with_signer(
        cfg: ClobSettings,
        creds: ClobCredentials,
        signer: Arc<dyn OrderSigner>,
        sign_ctx: ClobSigningContext,
    ) -> Result<Self, ClobError> {
        let http = build_http_client()?;
        let order_limiter = Arc::new(tokio::sync::Mutex::new(TokenBucket::per_minute(
            cfg.clob_rate_limit_orders_per_min,
        )));
        let egress_clob_rps = cfg.egress_clob_rps;
        let egress_host = host_of(&cfg.clob_host).unwrap_or_else(|| DEFAULT_CLOB_HOST.to_owned());
        Ok(Self {
            http,
            base_url: cfg.clob_host,
            creds,
            order_limiter,
            egress_bucket: tokio::sync::OnceCell::new(),
            egress_clob_rps,
            egress_host,
            signer: Some(signer),
            sign_ctx: Some(sign_ctx),
            sdk: None,
            ticks: Arc::new(DashMap::new()),
            min_sizes: Arc::new(DashMap::new()),
        })
    }

    /// Construct a client whose order **build/sign/post** path is driven by the
    /// official `polymarket_client_sdk_v2`.
    ///
    /// The SDK handles EIP-712 signing, server protocol-version resolution
    /// (V1/V2/V3), and the EIP-1271 deposit-wallet flow. Cancellation, reads,
    /// and the heartbeat continue to use this crate's HMAC REST path, so the
    /// existing L2 `creds` are still required.
    ///
    /// `funder` is the maker/funder address (the Polymarket proxy, Safe, or
    /// deposit wallet); pass `None` for the `EOA` flow. `signature_type` must be
    /// consistent with `funder` per the CLOB rules.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError`] if the HTTP client cannot be built or the SDK
    /// client cannot authenticate with the supplied credentials.
    pub async fn new_with_sdk(
        cfg: ClobSettings,
        creds: ClobCredentials,
        private_key: String,
        signature_type: SignatureType,
        funder: Option<Address>,
        chain_id: u64,
    ) -> Result<Self, ClobError> {
        let http = build_http_client()?;
        let sdk = crate::sdk::SdkOrder::connect(
            &cfg.clob_host,
            &creds,
            polymarket_client_sdk_v2::auth::SecretString::from(private_key),
            signature_type,
            funder,
            chain_id,
        )
        .await?;
        let order_limiter = Arc::new(tokio::sync::Mutex::new(TokenBucket::per_minute(
            cfg.clob_rate_limit_orders_per_min,
        )));
        let egress_clob_rps = cfg.egress_clob_rps;
        let egress_host = host_of(&cfg.clob_host).unwrap_or_else(|| DEFAULT_CLOB_HOST.to_owned());
        Ok(Self {
            http,
            base_url: cfg.clob_host,
            creds,
            order_limiter,
            egress_bucket: tokio::sync::OnceCell::new(),
            egress_clob_rps,
            egress_host,
            signer: None,
            sign_ctx: None,
            sdk: Some(sdk),
            ticks: Arc::new(DashMap::new()),
            min_sizes: Arc::new(DashMap::new()),
        })
    }

    /// Build a signed [`PlaceOrderBody`] for the supplied parameters.
    ///
    /// Public so that integration tests can exercise the signing + JSON
    /// serialisation path without spinning up a mock HTTP server.
    ///
    /// # Errors
    ///
    /// - [`ClobError::Auth`] when no signer is configured.
    /// - [`ClobError::Config`] when amounts overflow or `token_id` is not a
    ///   valid 256-bit integer.
    #[allow(clippy::similar_names)] // public CLOB API uses `side`/`size` everywhere
    async fn build_signed_body(
        &self,
        token_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        kind: crate::types::OrderKind,
    ) -> Result<PlaceOrderBody, ClobError> {
        let order_signer = self.signer.as_ref().ok_or_else(|| {
            ClobError::Auth(
                "EIP-712 order signing requires an OrderSigner. Build the client \
                 with `GarnetClobClient::new_with_signer(...)`."
                    .into(),
            )
        })?;
        let ctx = self.sign_ctx.as_ref().expect("sign_ctx set with signer");

        let token_id_u256 = U256::from_str(token_id)
            .or_else(|_| U256::from_str_radix(token_id.trim_start_matches("0x"), 16))
            .map_err(|e| ClobError::Config(format!("invalid token_id `{token_id}`: {e}")))?;

        let (maker_amount, taker_amount) =
            UnsignedOrder::amounts_from_price_size(side, price, size)?;

        // V2 salt is a u64 (the SDK serializes it as a JSON number via
        // `ser_salt`), not a full 256-bit value.
        let salt = U256::from(rand::random::<u64>());

        // V2 signed field: order creation time in milliseconds.
        let timestamp = U256::from(u64::try_from(Utc::now().timestamp_millis()).unwrap_or(0));

        // Builder-code attribution: a 32-byte value. Parse `BUILDER_CODE` as a
        // 0x-prefixed bytes32; anything else (incl. empty) means no attribution.
        let builder = self
            .creds
            .builder_code
            .as_deref()
            .and_then(|s| s.parse::<B256>().ok())
            .unwrap_or(B256::ZERO);

        let unsigned = UnsignedOrder {
            salt,
            maker: ctx.maker,
            signer: ctx.signer,
            token_id: token_id_u256,
            maker_amount,
            taker_amount,
            expiration: U256::ZERO, // GTC; wire-only, not signed in V2
            side,
            signature_type: ctx.signature_type,
            timestamp,
            metadata: B256::ZERO,
            builder,
            // Weather markets are non-negative-risk; if Garnet ever trades
            // neg-risk markets the trait method should be extended to carry
            // the flag through.
            exchange: Exchange::CtfExchange,
            verifying_contract: ctx.verifying_contract,
            chain_id: ctx.chain_id,
        };

        let signed_order = order_signer.sign_order(&unsigned).await?;

        Ok(PlaceOrderBody::new(
            &signed_order,
            &self.creds.api_key,
            kind,
        ))
    }

    /// Returns `true` if [`Self::place_limit_order`] can sign and submit
    /// orders.  False means the client was built without an
    /// [`OrderSigner`] (read-only configuration).
    #[must_use]
    pub fn has_signer(&self) -> bool {
        self.signer.is_some() || self.sdk.is_some()
    }

    /// Build authenticated headers for a CLOB v2 API-key request.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Auth`] if HMAC signing fails.
    fn auth_headers(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<HeaderMap, ClobError> {
        let timestamp = Utc::now().timestamp().to_string();
        let signature = self.creds.sign(&timestamp, method, path, body)?;

        let mut headers = HeaderMap::new();
        let insert = |h: &mut HeaderMap, k: &'static str, v: &str| {
            if let Ok(val) = v.parse() {
                h.insert(k, val);
            }
        };
        insert(&mut headers, "POLY_ADDRESS", &self.creds.address);
        insert(&mut headers, "POLY_API_KEY", &self.creds.api_key);
        insert(&mut headers, "POLY_SIGNATURE", &signature);
        insert(&mut headers, "POLY_TIMESTAMP", &timestamp);
        insert(&mut headers, "POLY_PASSPHRASE", &self.creds.passphrase);
        Ok(headers)
    }

    /// The shared per-host budget, connected on first use.
    ///
    /// `None` means no limiter is configured or it was unreachable at startup —
    /// fail-open, the caller simply does not wait.
    async fn egress_budget(&self) -> Option<&garnet_redis::EgressBucket> {
        self.egress_bucket
            .get_or_init(|| {
                garnet_redis::EgressBucket::from_env(&self.egress_host, self.egress_clob_rps)
            })
            .await
            .as_ref()
    }

    /// Wait for a token from the shared per-host budget before leaving the box.
    ///
    /// Every request to `egress_host` goes through here — reads, cancels and
    /// orders alike. A budget that only counted the reads would describe a
    /// request rate we do not have: Cloudflare scores what actually leaves the
    /// IP, and on a busy market the writes are the burst.
    async fn egress_acquire(&self) {
        if let Some(bucket) = self.egress_budget().await {
            bucket.acquire().await;
        }
    }

    /// Max attempts for an idempotent REST call before giving up.
    const REST_ATTEMPTS: u32 = 4;

    /// Send an **idempotent** request (GET/DELETE), honouring `Retry-After` and
    /// backing off with full jitter when the server sends no hint.
    ///
    /// Deliberately not used for `POST /order`: a signed order body is not
    /// idempotent, and replaying it can double-submit. Order pacing stays with
    /// `order_limiter`.
    ///
    /// The final attempt returns the throttled response as-is so
    /// [`Self::check_response`] can classify it — a 503 carrying `cf-ray` is a
    /// Cloudflare block, not a rate limit, and must reach `EgressBlocked`.
    async fn send_idempotent(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ClobError> {
        self.send_idempotent_counted(req)
            .await
            .map(|(resp, _)| resp)
    }

    /// [`Self::send_idempotent`], also reporting how many attempts it took.
    ///
    /// Only the cancel path needs the number, and it needs it to read a `404`
    /// correctly: on the first attempt that means "no such order", but after a
    /// retry it may equally mean "the attempt before this one landed and we
    /// never saw the reply". See [`Self::cancel_order`].
    async fn send_idempotent_counted(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<(reqwest::Response, u32), ClobError> {
        // One shared budget per host across every Garnet service and both bots:
        // Cloudflare scores the aggregate per IP, so a per-process bucket is
        // guesswork. Absent or unreachable limiter → no wait (fail-open).
        //
        // Keyed strictly by the request's own host. `check_geoblock` goes to
        // polymarket.com, a different host with its own Cloudflare budget, and
        // must not spend the CLOB's tokens under the CLOB's key.
        if request_host(&req).as_deref() == Some(self.egress_host.as_str()) {
            self.egress_acquire().await;
        }

        let mut pending: Option<ClobError> = None;
        for attempt in 0..Self::REST_ATTEMPTS {
            if attempt > 0 {
                let wait = match &pending {
                    Some(ClobError::RateLimited {
                        retry_after_secs: Some(secs),
                    }) => Duration::from_secs_f64(*secs),
                    _ => crate::backoff::full_jitter(
                        attempt - 1,
                        Duration::from_millis(500),
                        Duration::from_secs(30),
                    ),
                };
                tokio::time::sleep(wait).await;
            }

            let Some(attempt_req) = req.try_clone() else {
                // A streaming body cannot be replayed; send it once.
                return req
                    .send()
                    .await
                    .map(|resp| (resp, attempt + 1))
                    .map_err(ClobError::Network);
            };

            match attempt_req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let throttled = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE;
                    if !throttled || attempt + 1 == Self::REST_ATTEMPTS {
                        return Ok((resp, attempt + 1));
                    }
                    let hint = crate::retry::retry_after_secs(resp.headers());
                    warn!(
                        status = status.as_u16(),
                        retry_after = hint.unwrap_or(-1.0),
                        attempt,
                        "CLOB throttled; backing off"
                    );
                    pending = Some(ClobError::RateLimited {
                        retry_after_secs: hint,
                    });
                }
                Err(e) => {
                    if attempt + 1 == Self::REST_ATTEMPTS {
                        return Err(ClobError::Network(e));
                    }
                    warn!(error = %e, attempt, "CLOB transport error; retrying");
                    pending = None;
                }
            }
        }
        Err(pending.unwrap_or_else(|| ClobError::Config("REST retry loop exhausted".into())))
    }

    /// Extract and parse a response body, returning a CLOB error for non-2xx.
    async fn check_response<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, ClobError> {
        let status = resp.status();
        let headers = resp.headers().clone();
        if status == reqwest::StatusCode::NOT_FOUND {
            let msg = resp.text().await.unwrap_or_default();
            return Err(ClobError::NotFound(msg));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ClobError::RateLimited {
                retry_after_secs: crate::retry::retry_after_secs(&headers),
            });
        }
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            // A Cloudflare block (403/503 + cf-ray + non-JSON) is not an API
            // error — it means we are banned at the edge. Surface it distinctly
            // so the risk layer fires KS_EGRESS_BLOCKED, not KS_CLOB_LOST.
            if crate::cloudflare::is_cloudflare_block(status, &headers, &msg) {
                return Err(ClobError::EgressBlocked(format!(
                    "Cloudflare {} on CLOB REST",
                    status.as_u16()
                )));
            }
            return Err(ClobError::Http {
                status: status.as_u16(),
                message: msg,
            });
        }
        resp.json::<T>().await.map_err(ClobError::Network)
    }

    /// Parse a [`RestOrder`] into the public [`OrderInfo`] type.
    ///
    /// Returns [`ClobError::Parse`] when a decimal field arrives malformed —
    /// silently coercing to zero would let bad fills slip into downstream
    /// risk and reconciliation logic.
    fn parse_order(o: RestOrder) -> Result<OrderInfo, ClobError> {
        let parse_dec = |field: &'static str, s: &str| -> Result<Decimal, ClobError> {
            Decimal::from_str(s)
                .map_err(|e| ClobError::Parse(format!("order field `{field}` value `{s}`: {e}")))
        };
        let side = if o.side.eq_ignore_ascii_case("BUY") {
            Side::Buy
        } else {
            Side::Sell
        };
        let status = parse_status(&o.status);
        let original_size = parse_dec("original_size", &o.original_size)?;
        let size_matched = parse_dec("size_matched", &o.size_matched)?;
        // V2 omits `remaining_size`; derive it. Clamp at zero so a transient
        // over-fill report can never produce a negative remaining quantity.
        let size_remaining = (original_size - size_matched).max(Decimal::ZERO);
        // V2 sends `created_at` as Unix seconds and has no `updated_at`.
        let created_at = o
            .created_at
            .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
            .unwrap_or_else(Utc::now);
        Ok(OrderInfo {
            order_id: o.id,
            token_id: o.asset_id,
            price: parse_dec("price", &o.price)?,
            original_size,
            size_matched,
            size_remaining,
            side,
            status,
            created_at,
            updated_at: created_at,
        })
    }

    /// Parse one `GET /data/trades` item.
    ///
    /// A trade whose numbers or timestamp do not parse is an error rather than
    /// a silently dropped row: this feed exists to notice fills we do not know
    /// about, and a swallowed one defeats the whole point.
    fn parse_trade(t: RestTrade) -> Result<TradeInfo, ClobError> {
        let parse_dec = |field: &'static str, s: &str| -> Result<Decimal, ClobError> {
            Decimal::from_str(s)
                .map_err(|e| ClobError::Parse(format!("trade field `{field}` value `{s}`: {e}")))
        };
        let side = if t.side.eq_ignore_ascii_case("BUY") {
            Side::Buy
        } else {
            Side::Sell
        };
        let match_time = t
            .match_time
            .parse::<i64>()
            .ok()
            .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
            .ok_or_else(|| {
                ClobError::Parse(format!(
                    "trade match_time `{}` is not epoch seconds",
                    t.match_time
                ))
            })?;
        Ok(TradeInfo {
            trade_id: t.id,
            taker_order_id: t.taker_order_id,
            maker_order_ids: t.maker_orders.into_iter().map(|m| m.order_id).collect(),
            market: t.market,
            token_id: t.asset_id,
            side,
            size: parse_dec("size", &t.size)?,
            price: parse_dec("price", &t.price)?,
            match_time,
        })
    }

    /// Verify connectivity to the CLOB by calling `GET /ok`.
    ///
    /// Returns `Ok(())` if the server responds with HTTP 200.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Network`] or [`ClobError::Http`] on failure.
    pub async fn health_check(&self) -> Result<(), ClobError> {
        let url = format!("{}/ok", self.base_url);
        let resp = self.send_idempotent(self.http.get(&url)).await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let msg = resp.text().await.unwrap_or_default();
            return Err(ClobError::Http {
                status,
                message: msg,
            });
        }
        Ok(())
    }

    /// Probe `GET https://polymarket.com/api/geoblock` to learn the egress the
    /// exchange actually sees `{ blocked, ip, country, region }`.
    ///
    /// Goes through this client's proxy-aware HTTP stack, so it measures the
    /// same egress the order path uses. Returns [`ClobError::EgressBlocked`]
    /// when the response is a Cloudflare block rather than the JSON payload.
    ///
    /// # Errors
    ///
    /// [`ClobError::EgressBlocked`] on a Cloudflare block, [`ClobError::Http`]
    /// on any other non-2xx, [`ClobError::Network`] on transport failure.
    pub async fn check_geoblock(&self) -> Result<GeoblockStatus, ClobError> {
        // The geoblock endpoint lives on polymarket.com, a third host distinct
        // from clob_host / gamma_host.
        let url = "https://polymarket.com/api/geoblock";
        let resp = self.send_idempotent(self.http.get(url)).await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            if crate::cloudflare::is_cloudflare_block(status, &headers, &msg) {
                return Err(ClobError::EgressBlocked(format!(
                    "Cloudflare {} on geoblock endpoint",
                    status.as_u16()
                )));
            }
            return Err(ClobError::Http {
                status: status.as_u16(),
                message: msg,
            });
        }
        resp.json::<GeoblockStatus>()
            .await
            .map_err(ClobError::Network)
    }
}

impl GarnetClobClient {
    /// The shared HTTP client, so sibling components (the heartbeat) reuse this
    /// connection pool instead of opening their own.
    ///
    /// More independent `reqwest` pools means more parallel TLS handshakes,
    /// which is exactly the bursty shape Bot Management scores badly
    /// (spec §A5). Callers needing a different timeout set it per request.
    #[must_use]
    pub fn http(&self) -> Client {
        self.http.clone()
    }
}

/// Observed egress as reported by `polymarket.com/api/geoblock`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GeoblockStatus {
    /// Whether the FRONTEND blocks this location (do NOT gate the API on this).
    #[serde(default)]
    pub blocked: bool,
    /// Detected egress IP.
    #[serde(default)]
    pub ip: String,
    /// ISO 3166-1 alpha-2 country code.
    #[serde(default)]
    pub country: String,
    /// Region / sub-national code.
    #[serde(default)]
    pub region: String,
}

/// Host used for the shared budget when `clob_host` cannot be parsed.
pub(crate) const DEFAULT_CLOB_HOST: &str = "clob.polymarket.com";

/// Host component of a URL.
///
/// `None` for anything unparseable, which the caller turns into the default —
/// a malformed host must not become a bucket key of its own.
pub(crate) fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(std::borrow::ToOwned::to_owned)
}

/// Host a built request would go to.
///
/// Building costs one clone; the alternative is threading the host through
/// every call site, where it would drift from the URL actually used.
fn request_host(req: &reqwest::RequestBuilder) -> Option<String> {
    req.try_clone()?
        .build()
        .ok()?
        .url()
        .host_str()
        .map(std::borrow::ToOwned::to_owned)
}

/// Construct the shared `reqwest::Client` used by every variant of
/// [`GarnetClobClient`].
fn build_http_client() -> Result<Client, ClobError> {
    let builder = Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("garnet-clob/0.1 (Polymarket CLOB v2)");
    // Route CLOB REST through POLYMARKET_PROXY_URL when set (geo egress).
    crate::proxy::apply_reqwest_proxy(builder)?
        .build()
        .map_err(|e| ClobError::Config(format!("reqwest client: {e}")))
}

/// Map the textual `status` field of a CLOB response to [`ClobOrderStatus`].
fn parse_status(s: &str) -> ClobOrderStatus {
    match s {
        "MATCHED" => ClobOrderStatus::Matched,
        "DELAYED" => ClobOrderStatus::Delayed,
        "CANCELLED" => ClobOrderStatus::Cancelled,
        "EXPIRED" => ClobOrderStatus::Expired,
        _ => ClobOrderStatus::Open,
    }
}

#[async_trait]
impl ClobClient for GarnetClobClient {
    /// Place a GTC limit order on the CLOB.
    ///
    /// Builds an [`UnsignedOrder`] from the request, signs it via the
    /// injected [`OrderSigner`], and POSTs `{order, orderType, owner}`
    /// to `/order` with the standard HMAC-SHA256 API-key headers (the HMAC
    /// covers the serialized body).
    ///
    /// Returns [`ClobError::Auth`] if the client was constructed without a
    /// signer (read-only mode).
    #[allow(clippy::similar_names)] // public CLOB API uses `side`/`size` everywhere
    async fn place_limit_order(
        &self,
        token_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        kind: crate::types::OrderKind,
    ) -> Result<OrderResponse, ClobError> {
        // Proactive rate-limit: stay within clob_rate_limit_orders_per_min so we
        // do not rely solely on reactive 429 retries.
        self.order_limiter.lock().await.acquire().await;

        // Then the shared per-IP budget, which the order limiter knows nothing
        // about: it counts our orders, not the reads the scanner and the second
        // bot are making through the same address at the same moment.
        //
        // Both paths below go to `base_url`, whose host *is* `egress_host`, so
        // there is nothing to gate on here the way `send_idempotent` has to.
        // The SDK's own `GET /trades` polling inside `post_order` still escapes
        // the count — it does not come through this client.
        self.egress_acquire().await;

        // Preferred path: official SDK handles build → sign → post.
        if let Some(sdk) = &self.sdk {
            let placed = sdk
                .place_limit_order(token_id, side, price, size, kind)
                .await?;
            info!(
                order_id = %placed.order_id,
                tx_hash = placed.tx_hash.as_deref().unwrap_or("-"),
                status = %placed.status,
                "CLOB v2 order accepted (SDK)"
            );
            return Ok(OrderResponse {
                order_id: placed.order_id,
                status: parse_status(&placed.status),
                token_id: token_id.into(),
                price,
                size,
                side,
                created_at: Utc::now(),
            });
        }

        // Fallback: hand-rolled signer + REST POST.
        let body = self
            .build_signed_body(token_id, side, price, size, kind)
            .await?;

        // Serialize once: the exact string we sign must be the exact bytes we
        // send, or the server-side L2 HMAC check (which covers the body) fails.
        let body_str = serde_json::to_string(&body)
            .map_err(|e| ClobError::Config(format!("serialize order body: {e}")))?;

        let path = "/order";
        let url = format!("{}{}", self.base_url, path);
        let headers = self.auth_headers("POST", path, Some(&body_str))?;

        debug!(
            token_id,
            ?side,
            price = %price,
            size = %size,
            "submitting signed CLOB v2 order"
        );

        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body_str)
            .send()
            .await?;

        let parsed: ClobOrderResponse = Self::check_response(resp).await?;

        let status = parsed
            .status
            .as_deref()
            .map_or(ClobOrderStatus::Open, parse_status);

        info!(
            order_id = %parsed.order_id,
            tx_hash = parsed.transaction_hash.as_deref().unwrap_or("-"),
            ?status,
            "CLOB v2 order accepted"
        );

        Ok(OrderResponse {
            order_id: parsed.order_id,
            status,
            token_id: token_id.into(),
            price,
            size,
            side,
            created_at: Utc::now(),
        })
    }

    async fn cancel_order(&self, order_id: &str) -> Result<(), ClobError> {
        // V2: `DELETE /order` takes the id in a JSON body (`{"orderID": ...}`),
        // not in the path. The HMAC signature covers the exact body bytes, so
        // serialize once and reuse that string for both signing and sending.
        let path = "/order";
        let url = format!("{}{}", self.base_url, path);
        let body_str = serde_json::to_string(&serde_json::json!({ "orderID": order_id }))
            .map_err(|e| ClobError::Config(format!("serialize cancel body: {e}")))?;
        let headers = self.auth_headers("DELETE", path, Some(&body_str))?;

        // Retried like every other DELETE, `cancel_all` included (G-16). A
        // cancel is idempotent — the id names one order and asking twice for it
        // to stop resting cannot cost anything — so the single shot this used to
        // be bought nothing and gave a transient 429 or dropped connection the
        // power to leave an order live that the caller has been told is gone.
        // That is G-7's two-orders-on-the-book case reached from the other side.
        //
        // `send_idempotent` also spends the shared egress budget for us; the
        // explicit `egress_acquire` this used to need went with it.
        debug!(order_id, "cancelling order");
        let (resp, attempts) = self
            .send_idempotent_counted(
                self.http
                    .delete(&url)
                    .headers(headers)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body_str),
            )
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // A 404 on the first attempt is what it says: no such order. A 404
            // after a retry is ambiguous — an earlier attempt may have cancelled
            // it and lost the reply — and reporting failure there would tell the
            // caller an order still rests when we just took it off the book.
            // Callers act on "is it gone?", and after a retry the answer is yes
            // either way.
            if attempts > 1 {
                debug!(
                    order_id,
                    attempts, "order already gone on a retried cancel — treating as cancelled"
                );
                return Ok(());
            }
            return Err(ClobError::NotFound(order_id.into()));
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let msg = resp.text().await.unwrap_or_default();
            return Err(ClobError::Http {
                status,
                message: msg,
            });
        }
        Ok(())
    }

    async fn cancel_all(&self) -> Result<u32, ClobError> {
        // V2: dedicated `DELETE /cancel-all` endpoint, no body. The old
        // `DELETE /orders` (no payload) returned HTTP 400 "Invalid order
        // payload" because that route is the batch cancel-by-ids endpoint.
        let path = "/cancel-all";
        let url = format!("{}{}", self.base_url, path);
        let headers = self.auth_headers("DELETE", path, None)?;

        debug!("cancelling all orders");
        let resp = self
            .send_idempotent(self.http.delete(&url).headers(headers))
            .await?;
        let body: RestCancelAllResponse = Self::check_response(resp).await?;
        let count = u32::try_from(body.canceled.len()).unwrap_or(u32::MAX);
        tracing::info!(count, "cancelled all orders");
        Ok(count)
    }

    async fn get_order(&self, order_id: &str) -> Result<OrderInfo, ClobError> {
        // V2: single order lives under the `data/` namespace.
        let path = format!("/data/order/{order_id}");
        let url = format!("{}{}", self.base_url, path);
        let headers = self.auth_headers("GET", &path, None)?;

        let resp = self
            .send_idempotent(self.http.get(&url).headers(headers))
            .await?;
        let raw: RestOrder = Self::check_response(resp).await?;
        Self::parse_order(raw)
    }

    async fn get_open_orders(&self) -> Result<Vec<OrderInfo>, ClobError> {
        // V2: `GET /data/orders` returns the maker's live orders as a paginated
        // `Page` wrapper. There is no `?status=OPEN` filter (the V1 query that
        // returned HTTP 405 here). The signature covers the path only — no
        // query string — so none is appended.
        let path = "/data/orders";
        let url = format!("{}{}", self.base_url, path);
        let headers = self.auth_headers("GET", path, None)?;

        let resp = self
            .send_idempotent(self.http.get(&url).headers(headers))
            .await?;
        let page: RestPage<RestOrder> = Self::check_response(resp).await?;
        page.data.into_iter().map(Self::parse_order).collect()
    }

    async fn get_trades(&self, after: DateTime<Utc>) -> Result<Vec<TradeInfo>, ClobError> {
        // V2: `GET /data/trades` returns our own trades, newest first, in the
        // same paginated `Page` wrapper. `after` is Unix seconds. The signature
        // covers the path only (see `auth_headers`), so the query string is
        // appended to the URL and left out of the signed message.
        let path = "/data/trades";
        let url = format!("{}{}?after={}", self.base_url, path, after.timestamp());
        let headers = self.auth_headers("GET", path, None)?;

        let resp = self
            .send_idempotent(self.http.get(&url).headers(headers))
            .await?;
        let page: RestPage<RestTrade> = Self::check_response(resp).await?;
        page.data.into_iter().map(Self::parse_trade).collect()
    }

    async fn market_resolution(
        &self,
        condition_id: &str,
    ) -> Result<crate::types::MarketResolution, ClobError> {
        // Public, like `/book`. Verified live on 2026-08-13 against
        // 0xed5203d2… (Cleveland Guardians vs. Detroit Tigers), which answered
        // `closed: true` with `winner: true` on the Tigers leg while Gamma
        // still reported the market as open with an `endDate` a week away.
        let url = format!("{}/markets/{}", self.base_url, condition_id);

        let resp = self.send_idempotent(self.http.get(&url)).await?;
        let raw: RestMarket = Self::check_response(resp).await?;

        // The winner is only meaningful once `closed` is set. A settled market
        // has exactly one winning leg; more than one is a wire contract we do
        // not understand, and guessing at it would settle a position at the
        // wrong end of a dollar.
        let winners: Vec<&RestMarketToken> = raw.tokens.iter().filter(|t| t.winner).collect();
        let winner = if raw.closed {
            match winners.as_slice() {
                [only] => Some(only.outcome.clone()),
                [] => None,
                many => {
                    tracing::error!(
                        condition_id,
                        count = many.len(),
                        "settled market reports several winning outcomes — refusing to pick one"
                    );
                    None
                }
            }
        } else {
            // A market still trading has no winner to report, whatever a stray
            // flag on a leg might say.
            None
        };

        Ok(crate::types::MarketResolution {
            closed: raw.closed,
            winner,
        })
    }

    async fn market_meta(
        &self,
        condition_id: &str,
    ) -> Result<Option<crate::types::ClobMarketMeta>, ClobError> {
        // Same public endpoint as `market_resolution`, read for a different
        // question. Kept as its own method rather than widening that one: the
        // resolution view is called on a settled market and this one on a
        // market that is very much live, and folding them would make each
        // caller carry the other's fields.
        let url = format!("{}/markets/{}", self.base_url, condition_id);
        let resp = self.send_idempotent(self.http.get(&url)).await?;
        let raw: RestMarket = Self::check_response(resp).await?;

        // No id echoed back is the CLOB saying it does not know this market.
        let Some(id) = raw.condition_id.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(crate::types::ClobMarketMeta {
            condition_id: id,
            end_date: raw.end_date_iso,
            game_start_time: raw.game_start_time,
            closed: raw.closed,
            tokens: raw
                .tokens
                .into_iter()
                .map(|t| (t.token_id, t.outcome))
                .collect(),
        }))
    }

    async fn get_orderbook(&self, token_id: &str) -> Result<BookSnapshot, ClobError> {
        // Orderbook endpoint is public; no auth required.
        let url = format!("{}/book?token_id={}", self.base_url, token_id);

        let resp = self.send_idempotent(self.http.get(&url)).await?;
        let raw: RestBook = Self::check_response(resp).await?;

        // Keyed by the token we asked for, not by `raw.asset_id`: the caller
        // will look it up with the same string it passed in here.
        if let Some(min) = parse_min_size(raw.min_order_size.as_deref(), token_id) {
            self.min_sizes.insert(token_id.to_owned(), min);
        }
        if let Some(tick) = parse_tick(raw.tick_size.as_deref(), token_id) {
            self.ticks.insert(token_id.to_owned(), tick);
            // Pushed from the one place that learns the number, so our map and
            // the SDK's cache cannot drift apart.
            if let Some(sdk) = &self.sdk {
                sdk.set_tick(token_id, tick);
            }
        }

        let parse_level = |side: &'static str, l: RestLevel| -> Result<PriceLevel, ClobError> {
            let price = Decimal::from_str(&l.price).map_err(|e| {
                ClobError::Parse(format!("orderbook {side} price `{}`: {e}", l.price))
            })?;
            let size = Decimal::from_str(&l.size).map_err(|e| {
                ClobError::Parse(format!("orderbook {side} size `{}`: {e}", l.size))
            })?;
            Ok(PriceLevel { price, size })
        };

        let timestamp = crate::time::parse_ws_timestamp(&raw.timestamp).map_err(|e| {
            ClobError::Parse(format!(
                "orderbook timestamp `{}` for token {}: {}",
                raw.timestamp, token_id, e
            ))
        })?;

        let mut bids: Vec<PriceLevel> = raw
            .bids
            .into_iter()
            .map(|l| parse_level("bid", l))
            .collect::<Result<_, _>>()?;
        let mut asks: Vec<PriceLevel> = raw
            .asks
            .into_iter()
            .map(|l| parse_level("ask", l))
            .collect::<Result<_, _>>()?;

        // The REST snapshot trusts the API ordering; enforce the documented
        // invariants so downstream depth-walking is correct: bids sorted by
        // descending price, asks by ascending price.
        bids.sort_by_key(|b| std::cmp::Reverse(b.price));
        asks.sort_by_key(|a| a.price);

        Ok(BookSnapshot {
            asset_id: raw.asset_id,
            market: raw.market,
            timestamp,
            received_at: Utc::now(),
            bids,
            asks,
            hash: raw.hash,
        })
    }

    fn tick_size(&self, token_id: &str) -> Option<Decimal> {
        self.ticks.get(token_id).map(|v| *v.value())
    }

    fn min_order_size(&self, token_id: &str) -> Option<Decimal> {
        self.min_sizes.get(token_id).map(|v| *v.value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_extracts_the_bucket_key() {
        assert_eq!(
            host_of("https://clob.polymarket.com").as_deref(),
            Some("clob.polymarket.com")
        );
        assert_eq!(
            host_of("https://polymarket.com/api/geoblock").as_deref(),
            Some("polymarket.com")
        );
        // Unparseable input must not become a bucket key of its own.
        assert_eq!(host_of("not a url"), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn request_host_matches_the_url_actually_used() {
        let http = Client::new();
        assert_eq!(
            request_host(&http.get("https://clob.polymarket.com/book")).as_deref(),
            Some("clob.polymarket.com")
        );
        // The geoblock probe is a DIFFERENT host and must not spend the CLOB
        // budget under the CLOB key.
        assert_eq!(
            request_host(&http.get("https://polymarket.com/api/geoblock")).as_deref(),
            Some("polymarket.com")
        );
        assert_ne!(
            request_host(&http.get("https://polymarket.com/api/geoblock")),
            request_host(&http.get("https://clob.polymarket.com/book"))
        );
    }

    #[test]
    fn a_trade_carries_every_order_id_it_could_be_ours_through() {
        let body = r#"{
            "id":"trade_1","taker_order_id":"ord_taker","market":"0xcond",
            "asset_id":"0xno","side":"SELL","size":"25","price":"0.54",
            "match_time":"1785316074",
            "maker_orders":[{"order_id":"ord_maker_a"},{"order_id":"ord_maker_b"}]
        }"#;
        let raw: RestTrade = serde_json::from_str(body).expect("trade parses");
        let trade = GarnetClobClient::parse_trade(raw).expect("valid trade parses");
        assert_eq!(trade.trade_id, "trade_1");
        assert_eq!(trade.side, Side::Sell);
        assert_eq!(trade.size, Decimal::from(25));
        assert_eq!(trade.match_time.timestamp(), 1_785_316_074);
        // Reconciliation asks about all three: we could be the taker or either
        // maker, and only "none of them is ours" means nobody booked the fill.
        assert_eq!(
            trade.order_ids().collect::<Vec<_>>(),
            vec!["ord_taker", "ord_maker_a", "ord_maker_b"]
        );
    }

    #[test]
    fn a_trade_without_maker_orders_still_parses() {
        let body = r#"{
            "id":"trade_2","taker_order_id":"ord_taker","market":"0xcond",
            "asset_id":"0xno","side":"BUY","size":"5","price":"0.61",
            "match_time":"1785316074"
        }"#;
        let raw: RestTrade = serde_json::from_str(body).expect("trade parses");
        let trade = GarnetClobClient::parse_trade(raw).expect("valid trade parses");
        assert_eq!(trade.order_ids().collect::<Vec<_>>(), vec!["ord_taker"]);
    }

    #[test]
    fn a_trade_with_an_unreadable_match_time_is_an_error() {
        // Not dropped: this feed exists to notice fills we do not know about,
        // and a silently skipped row is exactly the fill we would miss.
        let body = r#"{
            "id":"trade_3","taker_order_id":"ord","market":"0xcond",
            "asset_id":"0xno","side":"BUY","size":"5","price":"0.61",
            "match_time":"not-a-timestamp"
        }"#;
        let raw: RestTrade = serde_json::from_str(body).expect("trade parses");
        assert!(GarnetClobClient::parse_trade(raw).is_err());
    }

    #[test]
    fn parse_order_buy_open() {
        let raw = RestOrder {
            id: "ord_1".into(),
            asset_id: "0xno".into(),
            price: "0.62".into(),
            original_size: "100".into(),
            size_matched: "40".into(),
            side: "BUY".into(),
            status: "OPEN".into(),
            created_at: Some(1_700_000_000),
        };
        let info = GarnetClobClient::parse_order(raw).expect("valid order parses");
        assert_eq!(info.order_id, "ord_1");
        assert_eq!(info.side, Side::Buy);
        assert_eq!(info.status, ClobOrderStatus::Open);
        // size_remaining is derived: original − matched = 60.
        assert_eq!(info.size_remaining, Decimal::from(60));
        assert_eq!(info.size_matched + info.size_remaining, info.original_size,);
    }

    #[test]
    fn parse_order_rejects_invalid_decimal() {
        let raw = RestOrder {
            id: "ord_2".into(),
            asset_id: "0xno".into(),
            price: "not-a-number".into(),
            original_size: "100".into(),
            size_matched: "0".into(),
            side: "BUY".into(),
            status: "OPEN".into(),
            created_at: None,
        };
        let err = GarnetClobClient::parse_order(raw).expect_err("invalid price must reject");
        match err {
            ClobError::Parse(msg) => {
                assert!(msg.contains("price"), "msg should name the field: {msg}");
                assert!(msg.contains("not-a-number"), "msg should echo value: {msg}");
            }
            other => panic!("expected ClobError::Parse, got {other:?}"),
        }
    }

    #[test]
    fn parse_order_rejects_invalid_size() {
        let raw = RestOrder {
            id: "ord_3".into(),
            asset_id: "0xno".into(),
            price: "0.62".into(),
            original_size: "garbage".into(),
            size_matched: "0".into(),
            side: "BUY".into(),
            status: "OPEN".into(),
            created_at: None,
        };
        let err = GarnetClobClient::parse_order(raw).expect_err("invalid size must reject");
        assert!(matches!(err, ClobError::Parse(_)));
    }

    #[test]
    fn place_limit_order_without_signer_returns_auth_error() {
        let creds = test_creds();
        let client = GarnetClobClient::new(ClobSettings::default(), creds).unwrap();
        assert!(!client.has_signer());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(client.place_limit_order(
                "0xtoken",
                Side::Buy,
                "0.62".parse().unwrap(),
                "100".parse().unwrap(),
                crate::types::OrderKind::Fak,
            ))
            .unwrap_err();
        assert!(matches!(err, ClobError::Auth(_)));
        assert!(!err.is_retryable());
    }

    #[test]
    fn order_kind_maps_to_the_wire_values_the_exchange_expects() {
        use crate::types::OrderKind;
        assert_eq!(OrderKind::Fak.as_wire(), "FAK");
        assert_eq!(OrderKind::Gtc.as_wire(), "GTC");
    }

    #[tokio::test]
    async fn build_signed_body_produces_expected_wire_format() {
        use alloy::primitives::address;
        use rust_decimal_macros::dec;

        use crate::signer::InMemoryOrderSigner;

        let creds = ClobCredentials {
            builder_code: Some("GARNET".into()),
            ..test_creds()
        };
        let signer: Arc<dyn OrderSigner> = Arc::new(
            InMemoryOrderSigner::from_hex_key(
                "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
            )
            .unwrap(),
        );
        let ctx = ClobSigningContext {
            maker: address!("0x90F79bf6EB2c4f870365E785982E1f101E93b906"),
            signer: address!("0x90F79bf6EB2c4f870365E785982E1f101E93b906"),
            verifying_contract: address!("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"),
            signature_type: SignatureType::Eoa,
            chain_id: 137,
        };
        let client =
            GarnetClobClient::new_with_signer(ClobSettings::default(), creds, signer, ctx).unwrap();
        assert!(client.has_signer());

        let body = client
            .build_signed_body(
                "1234",
                Side::Buy,
                dec!(0.62),
                dec!(100),
                crate::types::OrderKind::Fak,
            )
            .await
            .unwrap();

        // Fields per CLOB v2 wire format (CTF Exchange V2 order body)
        assert_eq!(
            body.order.maker,
            "0x90f79bf6eb2c4f870365e785982e1f101e93b906"
        );
        assert_eq!(
            body.order.signer,
            "0x90f79bf6eb2c4f870365e785982e1f101e93b906"
        );
        assert_eq!(body.order.token_id, "1234");
        // 0.62 * 100 = 62 pUSD → 62_000_000 base units
        assert_eq!(body.order.maker_amount, "62000000");
        // 100 NO tokens → 100_000_000 base units
        assert_eq!(body.order.taker_amount, "100000000");
        // V2: side is the enum (serializes to "BUY"), expiration "0" for GTC.
        assert_eq!(body.order.side, Side::Buy);
        assert_eq!(body.order.expiration, "0");
        assert_eq!(body.order.signature_type, 0);
        // V2 signed fields present on the wire.
        assert_eq!(
            body.order.metadata,
            "0x0000000000000000000000000000000000000000000000000000000000000000"
        );
        // "GARNET" is not valid 0x-bytes32 → no builder attribution (zero).
        assert_eq!(
            body.order.builder,
            "0x0000000000000000000000000000000000000000000000000000000000000000"
        );
        // timestamp is milliseconds — far larger than a seconds-scale value.
        assert!(body.order.timestamp.parse::<u64>().unwrap() > 1_000_000_000_000);
        // Signature is 0x + 130 hex chars
        assert!(body.order.signature.starts_with("0x"));
        assert_eq!(body.order.signature.len(), 132);
        // Envelope: owner is the API key, not the wallet address.
        // Copying uses FAK only: GTC would leave the order resting in the book as a
        // maker leg, which is the opposite of copying a taker.
        assert_eq!(body.order_type, "FAK");
        assert_eq!(body.owner, "k");

        // Round-trip through serde to verify the JSON contract is stable.
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["orderType"], "FAK");
        assert_eq!(json["owner"], "k");
        assert_eq!(json["order"]["tokenId"], "1234");
        assert_eq!(json["order"]["makerAmount"], "62000000");
        assert_eq!(json["order"]["takerAmount"], "100000000");
        assert_eq!(json["order"]["side"], "BUY");
        assert_eq!(json["order"]["signatureType"], 0);
        // salt serializes as a JSON number, not a string.
        assert!(json["order"]["salt"].is_number());
        // No V1-only fields leak into the V2 body.
        assert!(json["order"].get("nonce").is_none());
        assert!(json["order"].get("feeRateBps").is_none());
        assert!(json["order"].get("taker").is_none());
    }

    #[tokio::test]
    async fn place_limit_order_posts_signed_payload_to_mock_server() {
        use alloy::primitives::address;
        use rust_decimal_macros::dec;
        use wiremock::matchers::{body_partial_json, header, header_exists, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use crate::signer::InMemoryOrderSigner;

        let server = MockServer::start().await;

        // Validate the request body has the right shape and the static
        // CLOB-v2 invariants — wire amounts in 6-decimal base units, GTC
        // order type, side as "BUY", V2 fields present.
        let expected = serde_json::json!({
            "order": {
                "makerAmount": "62000000",
                "takerAmount": "100000000",
                "tokenId": "1234",
                "side": "BUY",
                "signatureType": 0,
                "expiration": "0",
            },
            "orderType": "FAK",
        });
        Mock::given(method("POST"))
            .and(path("/order"))
            .and(header("content-type", "application/json"))
            // Standard HMAC-SHA256 API-key headers must accompany the POST.
            .and(header_exists("POLY_SIGNATURE"))
            .and(header_exists("POLY_TIMESTAMP"))
            .and(header_exists("POLY_API_KEY"))
            .and(body_partial_json(expected))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "orderID": "ord_abc",
                "status": "OPEN",
                "transactionHash": "0xdeadbeef"
            })))
            .mount(&server)
            .await;

        let trading = ClobSettings {
            clob_host: server.uri(),
            ..ClobSettings::default()
        };
        let creds = test_creds();
        let signer: Arc<dyn OrderSigner> = Arc::new(
            InMemoryOrderSigner::from_hex_key(
                "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
            )
            .unwrap(),
        );
        let ctx = ClobSigningContext {
            maker: address!("0x90F79bf6EB2c4f870365E785982E1f101E93b906"),
            signer: address!("0x90F79bf6EB2c4f870365E785982E1f101E93b906"),
            verifying_contract: address!("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"),
            signature_type: SignatureType::Eoa,
            chain_id: 137,
        };
        let client = GarnetClobClient::new_with_signer(trading, creds, signer, ctx).unwrap();

        let resp = client
            .place_limit_order(
                "1234",
                Side::Buy,
                dec!(0.62),
                dec!(100),
                crate::types::OrderKind::Fak,
            )
            .await
            .expect("order accepted");
        assert_eq!(resp.order_id, "ord_abc");
        assert_eq!(resp.status, ClobOrderStatus::Open);
        assert_eq!(resp.size, dec!(100));
    }

    #[test]
    fn a_book_tick_is_parsed_off_the_wire() {
        use rust_decimal_macros::dec;

        assert_eq!(parse_tick(Some("0.001"), "0xno"), Some(dec!(0.001)));
        // Scale off the wire must not matter: rust_decimal equality is
        // mathematical, and the value is used for division either way.
        assert_eq!(parse_tick(Some("0.0010"), "0xno"), Some(dec!(0.001)));
    }

    #[test]
    fn an_unusable_book_tick_declines_instead_of_guessing() {
        // All four mean the same thing downstream: price this order the way we
        // priced orders before any tick was read. Substituting a constant would
        // coarsen a valid price on a fine market.
        assert_eq!(parse_tick(None, "0xno"), None);
        assert_eq!(parse_tick(Some("not-a-number"), "0xno"), None);
        assert_eq!(parse_tick(Some("0"), "0xno"), None);
        assert_eq!(parse_tick(Some("-0.001"), "0xno"), None);
    }

    #[test]
    fn a_book_without_a_tick_still_parses() {
        // The tick is a bonus field on a response we need for its levels. A
        // missing tick may cost us the snap; it must not cost us the book.
        let body = r#"{"asset_id":"1","market":"0xm","hash":"h",
            "timestamp":"1785316074000","bids":[],"asks":[]}"#;
        let raw: RestBook = serde_json::from_str(body).expect("book without tick parses");
        assert!(raw.tick_size.is_none());
    }

    #[test]
    fn a_book_with_a_tick_carries_it() {
        let body = r#"{"asset_id":"1","market":"0xm","hash":"h",
            "timestamp":"1785316074000","tick_size":"0.001","bids":[],"asks":[]}"#;
        let raw: RestBook = serde_json::from_str(body).expect("book with tick parses");
        assert_eq!(raw.tick_size.as_deref(), Some("0.001"));
    }

    #[test]
    fn the_markets_minimum_order_size_is_read_off_the_book() {
        use rust_decimal_macros::dec;

        // Sizing enforces our own dollar floor and knows nothing about the
        // market's own minimum, so an order under it was sent and refused.
        let body = r#"{"asset_id":"1","market":"0xm","hash":"h",
            "timestamp":"1785316074000","tick_size":"0.001","min_order_size":5,
            "bids":[],"asks":[]}"#;
        let raw: RestBook = serde_json::from_str(body).expect("book parses");
        assert_eq!(
            parse_min_size(raw.min_order_size.as_deref(), "0xno"),
            Some(dec!(5))
        );

        // Unusable values leave the caller sending what it sized, which is what
        // happened before the field was read at all.
        assert_eq!(parse_min_size(None, "0xno"), None);
        assert_eq!(parse_min_size(Some("0"), "0xno"), None);
        assert_eq!(parse_min_size(Some("nope"), "0xno"), None);
    }

    #[test]
    fn a_numeric_tick_does_not_cost_us_the_book() {
        use rust_decimal_macros::dec;

        // The captured shape is a string, but the official SDK reads this field
        // as TryFromInto<Decimal>, which accepts a number — so the wire is not
        // pinned. A number must yield the tick, and above all must not fail the
        // response: the book is what every price decision runs on.
        let body = r#"{"asset_id":"1","market":"0xm","hash":"h",
            "timestamp":"1785316074000","tick_size":0.001,"bids":[],"asks":[]}"#;
        let raw: RestBook = serde_json::from_str(body).expect("numeric tick parses");
        assert_eq!(
            parse_tick(raw.tick_size.as_deref(), "0xno"),
            Some(dec!(0.001))
        );
    }

    #[test]
    fn an_absurd_tick_shape_costs_only_the_tick() {
        // A shape neither string nor number (null, object, array) declines the
        // tick and keeps the book.
        for raw_tick in ["null", "{}", "[]", "true"] {
            let body = format!(
                r#"{{"asset_id":"1","market":"0xm","hash":"h",
                "timestamp":"1785316074000","tick_size":{raw_tick},"bids":[],"asks":[]}}"#
            );
            let raw: RestBook =
                serde_json::from_str(&body).unwrap_or_else(|e| panic!("tick {raw_tick}: {e}"));
            assert!(raw.tick_size.is_none(), "tick {raw_tick} must decline");
        }
    }

    fn test_creds() -> ClobCredentials {
        ClobCredentials {
            address: "0x90F79bf6EB2c4f870365E785982E1f101E93b906".into(),
            api_key: "k".into(),
            api_secret: base64_secret(),
            passphrase: "p".into(),
            builder_code: None,
        }
    }

    fn base64_secret() -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(b"test_secret_key_bytes")
    }

    /// A client pointed at `uri`, with API-key auth and no signer.
    fn client_against(uri: String) -> GarnetClobClient {
        GarnetClobClient::new(
            ClobSettings {
                clob_host: uri,
                ..ClobSettings::default()
            },
            test_creds(),
        )
        .expect("client builds")
    }

    #[tokio::test]
    async fn a_throttled_cancel_is_retried_rather_than_abandoned() {
        // G-16: the cancel used to be single-shot while `cancel_all` retried, so
        // one 429 left an order resting that the caller was told had been
        // cancelled — G-7's two-live-orders case reached from the other side.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/order"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/order"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .with_priority(2)
            .mount(&server)
            .await;

        client_against(server.uri())
            .cancel_order("ord_1")
            .await
            .expect("the second attempt cancels it");
    }

    #[tokio::test]
    async fn market_meta_reads_what_the_copy_engine_filters_on() {
        // The copy engine rejects a leader's trade outright when it has no row
        // for the market, and measured 2026-08-26 that was 409 signals against
        // 363 copies -- more trades lost to not knowing a market than copied.
        // 124 of the 137 markets were in our table by the time we looked, so it
        // is a race with the scanner's sweep, not a gap: the market resolves
        // fine, just later than the trade arrives.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/markets/0xabc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "condition_id": "0xabc",
                "closed": false,
                "end_date_iso": "2026-09-01T00:00:00Z",
                "game_start_time": "2026-08-31T23:00:00Z",
                "tokens": []
            })))
            .mount(&server)
            .await;

        let m = client_against(server.uri())
            .market_meta("0xabc")
            .await
            .expect("request succeeds")
            .expect("the CLOB knows this market");

        assert_eq!(m.condition_id, "0xabc");
        assert!(!m.closed);
        assert!(m.end_date.is_some());
        assert!(m.game_start_time.is_some());
    }

    /// RTDS can deliver a trade with a blank `outcome` and only a token id
    /// (7.8% of the live tape, measured 2026-08-28). Mapping that token back
    /// onto a leg is what this endpoint is read for on the copy path, so the
    /// token ids have to survive the parse -- not just the outcome names.
    #[tokio::test]
    async fn market_meta_carries_the_token_to_outcome_map() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/markets/0xabc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "condition_id": "0xabc",
                "closed": false,
                "tokens": [
                    {"token_id": "111", "outcome": "Yes", "winner": false},
                    {"token_id": "222", "outcome": "No", "winner": false}
                ]
            })))
            .mount(&server)
            .await;

        let m = client_against(server.uri())
            .market_meta("0xabc")
            .await
            .expect("request succeeds")
            .expect("the CLOB knows this market");

        assert_eq!(m.outcome_for("111"), Some("Yes"));
        assert_eq!(m.outcome_for("222"), Some("No"));
        assert_eq!(m.outcome_for("333"), None);
    }

    #[tokio::test]
    async fn market_meta_is_none_when_the_clob_does_not_know_the_market() {
        // A body with no `condition_id` is the CLOB saying it has nothing, and
        // it must read as "unknown", never as a market with empty fields --
        // that would hand the filter chain an end_date of None and a `closed`
        // of false for a market that may be neither.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/markets/0xnope"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        assert!(client_against(server.uri())
            .market_meta("0xnope")
            .await
            .expect("request succeeds")
            .is_none());
    }

    #[tokio::test]
    async fn a_404_after_a_retried_cancel_means_the_order_is_gone_not_that_we_failed() {
        // The ambiguous case the retry introduces: the first attempt may have
        // landed and lost its reply, so the 404 on the second is as likely to be
        // our own cancel as a bad id. The caller asks "is it still resting?",
        // and the answer here is no either way.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/order"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/order"))
            .respond_with(ResponseTemplate::new(404))
            .with_priority(2)
            .mount(&server)
            .await;

        client_against(server.uri())
            .cancel_order("ord_1")
            .await
            .expect("gone is gone");
    }

    #[tokio::test]
    async fn a_404_on_the_first_attempt_is_still_not_found() {
        // Nothing was retried, so the 404 says exactly what it says: no such
        // order. Callers and `mock_e2e` both rely on this staying an error.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/order"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = client_against(server.uri())
            .cancel_order("nope")
            .await
            .expect_err("an unknown id is an error");
        assert!(matches!(err, ClobError::NotFound(_)), "got {err:?}");
    }
}
