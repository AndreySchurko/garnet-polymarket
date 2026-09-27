//! Order **build → sign → post** via the official `polymarket_client_sdk_v2`.
//!
//! The hand-rolled signer/REST path in [`crate::client`] predates the SDK
//! landing on crates.io. This module routes *order placement* through the
//! official SDK so EIP-712 signing, server protocol-version resolution
//! (V1/V2/V3 via `GET /version`), and the EIP-1271 deposit-wallet flow are all
//! handled by Polymarket's own code rather than re-implemented here.
//!
//! Cancellation, order/orderbook reads, and the heartbeat stay on the existing
//! HMAC REST path in [`crate::client`] — this module covers only build/sign/post.

use std::str::FromStr as _;
use std::time::{Duration, Instant};

use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::{
    Credentials, ExposeSecret as _, LocalSigner, Normal, SecretString, Signer,
};
use polymarket_client_sdk_v2::clob::types::{
    OrderType, Side as SdkSide, SignatureType as SdkSigType, TickSize,
};
use polymarket_client_sdk_v2::clob::{Client as SdkClient, Config};
use polymarket_client_sdk_v2::types::{Address as SdkAddress, U256 as SdkU256};
use rust_decimal::Decimal;
use tracing::{debug, warn};

use crate::error::ClobError;
use crate::signer::SignatureType;
use crate::types::{ClobCredentials, Side};

/// Concrete type of the authenticated CLOB SDK client we hold.
type AuthedClient = SdkClient<Authenticated<Normal>>;

/// Wall time past which an order placement is worth a warning.
///
/// SDK 0.7.0 backfills settlement hashes inside `post_order` by polling
/// `GET /trades` every 250 ms, up to a 30 s ceiling. That path early-returns
/// while the exchange still puts `transactionsHashes` on the response, so it
/// costs nothing today. One second is comfortably above a healthy placement
/// and far below the SDK's ceiling, so the warning fires when the async
/// execution rollout starts costing us latency — and not before.
const SLOW_PLACEMENT: Duration = Duration::from_secs(1);

/// Whether a placement took long enough to be worth a warning.
#[must_use]
const fn is_slow_placement(elapsed: Duration) -> bool {
    elapsed.as_millis() >= SLOW_PLACEMENT.as_millis()
}

/// Whether an SDK error message is one of the order builder's price-versus-tick
/// rejections.
///
/// Matched on message text because the SDK returns them as a plain error string
/// with no code to switch on. All three come from `polymarket_client_sdk_v2`
/// `clob/order_builder.rs`; the alignment one is new in 0.7.0.
///
/// The tick the builder checks against is the SDK's own cached value, refreshed
/// only by its `GET /tick-size` call and never invalidated on a
/// `tick_size_change`. Flagging these is how the tick-snapping work learns how
/// often that actually bites.
#[must_use]
fn is_tick_price_rejection(msg: &str) -> bool {
    msg.contains("not aligned to the minimum tick size")
        || msg.contains("too small or too large for the minimum tick size")
        || (msg.contains("decimal places") && msg.contains("Minimum tick size"))
}

/// Outcome of a successful build/sign/post round trip.
#[derive(Debug, Clone)]
pub(crate) struct PlacedOrder {
    /// Exchange-assigned order id.
    pub order_id: String,
    /// Raw status string (`OrderStatusType` rendered, e.g. `"LIVE"`/`"MATCHED"`).
    pub status: String,
    /// First on-chain transaction hash, if the order matched immediately.
    pub tx_hash: Option<String>,
}

/// SDK-backed order placer: an authenticated [`SdkClient`] plus the key material
/// needed to sign each order.
pub(crate) struct SdkOrder {
    client: AuthedClient,
    private_key: SecretString,
    chain_id: u64,
}

impl std::fmt::Debug for SdkOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SdkOrder")
            .field("chain_id", &self.chain_id)
            .field("private_key", &"***REDACTED***")
            .finish_non_exhaustive()
    }
}

impl SdkOrder {
    /// Authenticate an SDK client against `host` using the L2 credentials the
    /// bot already holds (no network round trip to derive new credentials).
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Config`] if the API key is not a UUID,
    /// [`ClobError::Auth`] if the private key is malformed, or
    /// [`ClobError::Sdk`] if the SDK client cannot be constructed/authenticated.
    pub(crate) async fn connect(
        host: &str,
        creds: &ClobCredentials,
        private_key: SecretString,
        signature_type: SignatureType,
        funder: Option<SdkAddress>,
        chain_id: u64,
    ) -> Result<Self, ClobError> {
        let signer = build_signer(private_key.expose_secret(), chain_id)?;

        let api_key = uuid::Uuid::from_str(creds.api_key.trim())
            .map_err(|e| ClobError::Config(format!("POLY_API_KEY is not a UUID: {e}")))?;
        let sdk_creds =
            Credentials::new(api_key, creds.api_secret.clone(), creds.passphrase.clone());

        let mut builder = SdkClient::new(host, Config::default())
            .map_err(|e| ClobError::Sdk(format!("client init: {e}")))?
            .authentication_builder(&signer)
            // Supplying credentials means `authenticate()` does not call the API
            // to create/derive a fresh key — it just elevates to Authenticated.
            .credentials(sdk_creds)
            .signature_type(map_sig_type(signature_type));
        if let Some(funder) = funder {
            builder = builder.funder(funder);
        }
        let client = builder
            .authenticate()
            .await
            .map_err(|e| ClobError::Sdk(format!("authenticate: {e}")))?;

        Ok(Self {
            client,
            private_key,
            chain_id,
        })
    }

    /// Build, EIP-712-sign, and POST a limit order of the given kind.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Config`] for an unparsable `token_id` or
    /// [`ClobError::Sdk`] if the SDK build/sign/post fails or the exchange
    /// rejects the order.
    #[allow(clippy::similar_names)] // `side` / `size` mirror the CLOB API
    pub(crate) async fn place_limit_order(
        &self,
        token_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        kind: crate::types::OrderKind,
    ) -> Result<PlacedOrder, ClobError> {
        let signer = build_signer(self.private_key.expose_secret(), self.chain_id)?;

        let token = parse_token_id(token_id)?;

        let started = Instant::now();
        let result = self
            .client
            .limit_order()
            .token_id(token)
            .side(map_side(side))
            .price(price)
            .size(size)
            .order_type(match kind {
                crate::types::OrderKind::Gtc => OrderType::GTC,
                crate::types::OrderKind::Fak => OrderType::FAK,
            })
            .build_sign_and_post(&signer)
            .await;
        let elapsed = started.elapsed();

        if is_slow_placement(elapsed) {
            warn!(
                elapsed_ms = elapsed.as_millis(),
                token_id, "order placement is blocking — SDK is polling for settlement hashes"
            );
        } else {
            // Deliberately not "order placed": at this point we know only that
            // the SDK call returned. It can return Ok carrying success = false,
            // which is the exchange rejecting the order in-band. Acceptance is
            // logged by the caller once that is settled.
            debug!(elapsed_ms = elapsed.as_millis(), "placement call returned");
        }

        let resp = result.map_err(|e| {
            let msg = e.to_string();
            if is_tick_price_rejection(&msg) {
                // Not a transient failure: the price we computed cannot exist on
                // this market's tick. Logged distinctly so the tick-snapping work
                // starts from evidence rather than guesswork.
                warn!(token_id, %price, error = %msg, "order rejected locally: price does not fit the market tick");
            }
            ClobError::Sdk(msg)
        })?;

        if !resp.success {
            return Err(ClobError::Sdk(
                resp.error_msg
                    .unwrap_or_else(|| "order rejected by CLOB".to_owned()),
            ));
        }

        Ok(PlacedOrder {
            order_id: resp.order_id,
            status: resp.status.to_string(),
            tx_hash: resp.transaction_hashes.first().map(|h| format!("{h:#x}")),
        })
    }

    /// Hand the market's current tick to the SDK's cache.
    ///
    /// The SDK fills that cache from its own `GET /tick-size` and never
    /// invalidates it, so without this its order builder validates prices
    /// against whatever the tick was the first time it asked.
    pub(crate) fn set_tick(&self, token_id: &str, tick: Decimal) {
        let Ok(token) = parse_token_id(token_id) else {
            warn!(token_id, "tick not pushed: token id is unparseable");
            return;
        };
        match tick_for_sdk(tick) {
            Some(sdk_tick) => self.client.set_tick_size(token, sdk_tick),
            None => {
                // The one-number invariant is broken for this token from here
                // on: we snap to `tick` while the SDK validates against what it
                // fetched itself. The log is the only place that is visible.
                warn!(token_id, %tick, "tick not pushed: the SDK enum has no name for it; its cache keeps the value it fetched");
            }
        }
    }
}

/// Build a chain-bound local signer from a hex private key.
///
/// Returns `impl Signer` so the concrete `LocalSigner<SigningKey>` generic does
/// not have to be named (the SDK does not re-export a `PrivateKeySigner` alias).
fn build_signer(private_key: &str, chain_id: u64) -> Result<impl Signer, ClobError> {
    let signer = LocalSigner::from_str(private_key.trim())
        .map_err(|e| ClobError::Auth(format!("parse PRIVATE_KEY: {e}")))?;
    Ok(signer.with_chain_id(Some(chain_id)))
}

/// Map the crate's [`Side`] onto the SDK side.
fn map_side(side: Side) -> SdkSide {
    match side {
        Side::Buy => SdkSide::Buy,
        Side::Sell => SdkSide::Sell,
    }
}

/// Parse a CLOB token id — decimal, or `0x`-prefixed hex — into the SDK's `U256`.
///
/// # Errors
///
/// [`ClobError::Config`] when the string is neither.
fn parse_token_id(token_id: &str) -> Result<SdkU256, ClobError> {
    SdkU256::from_str(token_id)
        .or_else(|_| SdkU256::from_str_radix(token_id.trim_start_matches("0x"), 16))
        .map_err(|e| ClobError::Config(format!("invalid token_id `{token_id}`: {e}")))
}

/// Convert a wire tick into the SDK's enum, if it names one.
///
/// `TickSize::try_from` matches on exact equality against the six values the
/// SDK knows. Returns `None` for anything else — the exchange is still the
/// authority for our own snapping, so the caller keeps the value and skips only
/// the hand-off.
#[must_use]
fn tick_for_sdk(tick: Decimal) -> Option<TickSize> {
    TickSize::try_from(tick).ok()
}

/// Map the crate's [`SignatureType`] onto the SDK signature type.
fn map_sig_type(sig: SignatureType) -> SdkSigType {
    match sig {
        SignatureType::Eoa => SdkSigType::Eoa,
        SignatureType::PolyProxy => SdkSigType::Proxy,
        SignatureType::PolyGnosisSafe => SdkSigType::GnosisSafe,
        SignatureType::Poly1271 => SdkSigType::Poly1271,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tick_the_sdk_enum_cannot_name_is_not_pushed() {
        use rust_decimal_macros::dec;

        // The exchange is the authority on the tick even when the SDK's
        // six-variant enum has no name for it: our own snapping still uses it,
        // and only the hand-off is skipped.
        assert!(tick_for_sdk(dec!(0.02)).is_none());
        assert!(tick_for_sdk(dec!(0.005)).is_some());
        assert!(tick_for_sdk(dec!(0.0025)).is_some());
        assert!(tick_for_sdk(dec!(0.0001)).is_some());
    }

    #[test]
    fn trailing_zeros_off_the_wire_still_name_a_tick() {
        // rust_decimal equality is mathematical, not scale-sensitive, so a
        // "0.0010" tick string must still resolve to Thousandth.
        let from_wire = Decimal::from_str("0.0010").expect("parses");
        assert!(tick_for_sdk(from_wire).is_some());
    }

    #[test]
    fn a_token_id_parses_in_both_notations() {
        // The CLOB hands out decimal token ids; our own configs and logs
        // sometimes carry the 0x-hex form.
        assert!(parse_token_id("123456789").is_ok());
        assert!(parse_token_id("0x1f").is_ok());
        assert!(parse_token_id("not-a-token").is_err());
    }

    #[test]
    fn side_mapping_round_trips() {
        assert!(matches!(map_side(Side::Buy), SdkSide::Buy));
        assert!(matches!(map_side(Side::Sell), SdkSide::Sell));
    }

    #[test]
    fn sig_type_mapping_covers_all_variants() {
        assert!(matches!(map_sig_type(SignatureType::Eoa), SdkSigType::Eoa));
        assert!(matches!(
            map_sig_type(SignatureType::PolyProxy),
            SdkSigType::Proxy
        ));
        assert!(matches!(
            map_sig_type(SignatureType::PolyGnosisSafe),
            SdkSigType::GnosisSafe
        ));
        assert!(matches!(
            map_sig_type(SignatureType::Poly1271),
            SdkSigType::Poly1271
        ));
    }

    #[test]
    fn build_signer_rejects_garbage_key() {
        assert!(build_signer("not-a-key", 137).is_err());
    }

    #[test]
    fn build_signer_accepts_valid_key() {
        let key = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";
        assert!(build_signer(key, 137).is_ok());
    }

    #[test]
    fn a_prompt_placement_is_not_slow() {
        assert!(!is_slow_placement(Duration::from_millis(250)));
    }

    #[test]
    fn a_placement_past_the_threshold_is_slow() {
        // SDK 0.7.0 polls GET /trades every 250 ms for up to 30 s to backfill
        // settlement hashes. Today the response already carries them and the
        // backfill early-returns, so anything at or past a second means the
        // async execution pipeline has started rolling out and our order path
        // is now blocking on it.
        assert!(is_slow_placement(Duration::from_secs(1)));
        assert!(is_slow_placement(Duration::from_secs(30)));
    }

    #[test]
    fn the_threshold_is_far_below_the_sdk_polling_ceiling() {
        // A threshold at or above the SDK's own 30 s ceiling could never fire.
        assert!(SLOW_PLACEMENT < Duration::from_secs(30));
    }

    #[test]
    fn recognises_the_alignment_rejection_new_in_0_7_0() {
        // Verbatim from polymarket_client_sdk_v2 0.7.0 order_builder.rs, which
        // added `price_aligned_to_tick_size(price, tick) = (price % tick).is_zero()`.
        assert!(is_tick_price_rejection(
            "Price 0.007 is not aligned to the minimum tick size 0.005"
        ));
    }

    #[test]
    fn recognises_the_decimal_places_rejection() {
        assert!(is_tick_price_rejection(
            "Unable to build Order: Price 0.9965 has 4 decimal places. Minimum tick size 0.01 has 2 decimal places. Price decimal places <= minimum tick size decimal places"
        ));
    }

    #[test]
    fn recognises_the_out_of_range_rejection() {
        assert!(is_tick_price_rejection(
            "Price 0.0001 is too small or too large for the minimum tick size 0.01"
        ));
    }

    #[test]
    fn does_not_claim_unrelated_sdk_errors() {
        // Misclassifying these would send the tick work chasing ghosts.
        assert!(!is_tick_price_rejection("authenticate: 401 Unauthorized"));
        assert!(!is_tick_price_rejection("error sending request for url"));
        assert!(!is_tick_price_rejection("insufficient balance"));
    }
}
