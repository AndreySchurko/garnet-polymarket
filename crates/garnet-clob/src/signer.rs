//! EIP-712 order signing abstraction for CLOB v2.
//!
//! The CLOB v2 `/order` endpoint requires an **EIP-712 signed** [`UnsignedOrder`].
//! Signing depends on a private key, which lives in `garnet-blockchain`; to
//! keep `garnet-clob` free of that dependency we expose a [`OrderSigner`]
//! trait here and let `garnet-blockchain` implement it.  The trait object is
//! injected into [`GarnetClobClient::new`](crate::client::GarnetClobClient::new).
//!
//! # EIP-712 domain (Polygon mainnet)
//!
//! - `name`: `"Polymarket CTF Exchange"` for the regular exchange or
//!   `"Polymarket Neg Risk CTF Exchange"` for negative-risk markets.
//! - `version`: `"1"`.
//! - `chainId`: `137`.
//! - `verifyingContract`: address of the CTF Exchange V2 (or Neg Risk variant).
//!
//! # `Order` type-hash payload (CTF Exchange **V2**)
//!
//! ```text
//! Order(uint256 salt,address maker,address signer,uint256 tokenId,
//!       uint256 makerAmount,uint256 takerAmount,uint8 side,
//!       uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)
//! ```
//!
//! This matches the official SDK type-hash string in
//! `rs-clob-client-v2/src/clob/client.rs` byte-for-byte. The V2 struct dropped
//! the V1 fields `taker`, `expiration`, `nonce`, and `feeRateBps` from the
//! *signed* payload and added `timestamp` (order creation, **milliseconds**),
//! `metadata` (bytes32, default zero) and `builder` (bytes32 builder-code
//! attribution, default zero). `expiration` now travels only on the outer JSON
//! request body, not in the signed struct.
//!
//! The Solidity struct **must** be named `Order` — alloy's `sol!` derives the
//! EIP-712 type name from the Rust identifier, and the on-chain contract hashes
//! `Order(...)`. Renaming it changes the type hash and invalidates every
//! signature.

use std::fmt;

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::{eip712_domain, Eip712Domain, SolStruct};
use async_trait::async_trait;

use crate::error::ClobError;
use crate::types::Side;

// ---------------------------------------------------------------------------
// EIP-712 `Order` Sol struct (CTF Exchange V2)
// ---------------------------------------------------------------------------

alloy::sol! {
    /// Solidity definition of the Polymarket CTF Exchange **V2** `Order` struct.
    ///
    /// The exact field order, names, and types **must** match the on-chain
    /// contract so that the EIP-712 type hash and signing hash are identical to
    /// what the exchange verifies. Do not reorder or rename fields.
    #[derive(Default)]
    struct Order {
        uint256 salt;
        address maker;
        address signer;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint8 side;
        uint8 signatureType;
        uint256 timestamp;
        bytes32 metadata;
        bytes32 builder;
    }
}

// ---------------------------------------------------------------------------
// Signature-type enum
// ---------------------------------------------------------------------------

/// EIP-712 signature variants accepted by the CTF Exchange V2.
///
/// Integer encoding matches the SDK's on-chain `SignatureType` enum and the
/// values documented at <https://docs.polymarket.com/api-reference/authentication>:
/// `EOA = 0`, `POLY_PROXY = 1`, `POLY_GNOSIS_SAFE = 2`, `POLY_1271 = 3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SignatureType {
    /// Plain EOA signature; `maker == signer`. Funder is the EOA.
    Eoa = 0,
    /// Polymarket proxy wallet; `maker` is the proxy, `signer` is the EOA.
    PolyProxy = 1,
    /// Gnosis Safe; `maker` is the Safe address, `signer` is the EOA.
    PolyGnosisSafe = 2,
    /// Deposit-wallet flow for new API users; orders validated via ERC-1271.
    ///
    /// The funder (deposit wallet) is both `maker` and `signer`. Note: the
    /// 1271 *signature wrapping* (Solady `TypedDataSign`) is **not** implemented
    /// by [`InMemoryOrderSigner`] — a deposit-wallet signer must be injected.
    Poly1271 = 3,
}

impl SignatureType {
    /// Integer representation as serialised in the EIP-712 payload.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

// ---------------------------------------------------------------------------
// Exchange variant
// ---------------------------------------------------------------------------

/// Which exchange contract verifies the order — drives the EIP-712 domain name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exchange {
    /// Regular CTF Exchange V2.
    CtfExchange,
    /// Negative-risk CTF Exchange V2.
    NegRiskCtfExchange,
}

impl Exchange {
    /// The EIP-712 `name` field for this exchange.
    #[must_use]
    pub fn domain_name(self) -> &'static str {
        match self {
            Self::CtfExchange => "Polymarket CTF Exchange",
            Self::NegRiskCtfExchange => "Polymarket Neg Risk CTF Exchange",
        }
    }
}

// ---------------------------------------------------------------------------
// UnsignedOrder
// ---------------------------------------------------------------------------

/// Logical order payload ready for EIP-712 signing.
///
/// Amounts use **6-decimal base units** (the on-chain decimals of pUSD / CTF
/// tokens) — convert from `Decimal` via [`UnsignedOrder::amounts_from_price_size`].
///
/// Per CLOB v2 invariants the caller does **not** set `taker`, `nonce`, or
/// `fee_rate_bps`; they remain zero and the exchange populates them.
#[derive(Debug, Clone)]
pub struct UnsignedOrder {
    /// Random 256-bit salt (CSPRNG).  Prevents hash collisions across orders.
    pub salt: U256,
    /// Maker address.  Equal to the proxy / Safe in proxy mode; equal to the
    /// signing EOA in [`SignatureType::Eoa`] mode.
    pub maker: Address,
    /// Signing EOA (recovered from the EIP-712 signature on-chain).
    pub signer: Address,
    /// ERC-1155 token ID being bought (NO token) or sold.
    pub token_id: U256,
    /// Amount the maker spends, in 6-decimal base units.
    pub maker_amount: U256,
    /// Amount the maker receives, in 6-decimal base units.
    pub taker_amount: U256,
    /// Unix expiration (seconds).  `0` = GTC.
    ///
    /// **Not** part of the signed V2 struct — it travels on the outer JSON
    /// request body only.  Retained here so the wire body can read it.
    pub expiration: U256,
    /// `BUY` = 0, `SELL` = 1.
    pub side: Side,
    /// EIP-712 signature variant.
    pub signature_type: SignatureType,
    /// Order creation timestamp in **milliseconds** since the Unix epoch
    /// (signed V2 field).
    pub timestamp: U256,
    /// Arbitrary 32-byte metadata (signed V2 field). Default `B256::ZERO`.
    pub metadata: B256,
    /// 32-byte builder-code attribution (signed V2 field). Default `B256::ZERO`
    /// = no attribution.
    pub builder: B256,
    /// Exchange variant — determines which EIP-712 domain to use.
    pub exchange: Exchange,
    /// Address of the exchange contract (verifying contract in the domain).
    pub verifying_contract: Address,
    /// Polygon chain ID (mainnet = `137`).
    pub chain_id: u64,
}

impl UnsignedOrder {
    /// Convert a `(price, size)` pair in [`rust_decimal::Decimal`] into the
    /// `(maker_amount, taker_amount)` pair expected by the exchange.
    ///
    /// All Polymarket V2 amounts are 6-decimal integers (pUSD and CTF tokens
    /// both expose 6 decimals on Polygon).  For a `BUY`:
    ///
    /// - `maker_amount = price * size * 10^6` (pUSD spent)
    /// - `taker_amount = size * 10^6`         (NO tokens received)
    ///
    /// For a `SELL` the roles flip.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Config`] if any value is negative or overflows
    /// `u128` after scaling.
    #[allow(clippy::similar_names)] // side / size mirror the CLOB API
    pub fn amounts_from_price_size(
        side: Side,
        price: rust_decimal::Decimal,
        size: rust_decimal::Decimal,
    ) -> Result<(U256, U256), ClobError> {
        use rust_decimal::prelude::ToPrimitive;
        let scale = rust_decimal::Decimal::from(1_000_000u64);
        let to_u256 = |v: rust_decimal::Decimal, label: &str| -> Result<U256, ClobError> {
            if v.is_sign_negative() {
                return Err(ClobError::Config(format!("{label} must be non-negative")));
            }
            let scaled = (v * scale).round();
            let as_u128 = scaled.to_u128().ok_or_else(|| {
                ClobError::Config(format!("{label} overflows u128 when scaled to base units"))
            })?;
            Ok(U256::from(as_u128))
        };

        let size_units = to_u256(size, "size")?;
        let cost_units = to_u256(price * size, "cost")?;

        Ok(match side {
            Side::Buy => (cost_units, size_units),
            Side::Sell => (size_units, cost_units),
        })
    }

    /// Build the EIP-712 domain corresponding to this order.
    #[must_use]
    pub fn domain(&self) -> Eip712Domain {
        eip712_domain! {
            name: self.exchange.domain_name(),
            version: "2",
            chain_id: self.chain_id,
            verifying_contract: self.verifying_contract,
        }
    }

    /// Convert to the [`alloy::sol!`]-generated `Order` struct.  This is the
    /// value actually passed to `Signer::sign_typed_data`.
    #[must_use]
    pub fn to_sol(&self) -> Order {
        Order {
            salt: self.salt,
            maker: self.maker,
            signer: self.signer,
            tokenId: self.token_id,
            makerAmount: self.maker_amount,
            takerAmount: self.taker_amount,
            side: self.side.as_u8(),
            signatureType: self.signature_type.as_u8(),
            timestamp: self.timestamp,
            metadata: self.metadata,
            builder: self.builder,
        }
    }

    /// EIP-712 signing hash (`0x1901 || domainSeparator || hashStruct(order)`)
    /// — useful for snapshot tests.
    #[must_use]
    pub fn eip712_signing_hash(&self) -> [u8; 32] {
        self.to_sol().eip712_signing_hash(&self.domain()).0
    }
}

// ---------------------------------------------------------------------------
// SignedOrder
// ---------------------------------------------------------------------------

/// [`UnsignedOrder`] + 65-byte EIP-712 signature (`r || s || v`).
#[derive(Debug, Clone)]
pub struct SignedOrder {
    /// The original payload.
    pub order: UnsignedOrder,
    /// `r || s || v` — exactly 65 bytes.
    pub signature: [u8; 65],
}

impl SignedOrder {
    /// 0x-prefixed hex of the signature, as the CLOB endpoint expects.
    #[must_use]
    pub fn signature_hex(&self) -> String {
        let mut out = String::with_capacity(2 + 130);
        out.push_str("0x");
        for b in self.signature {
            use std::fmt::Write as _;
            write!(out, "{b:02x}").expect("write to String");
        }
        out
    }
}

// ---------------------------------------------------------------------------
// OrderSigner trait
// ---------------------------------------------------------------------------

/// Async EIP-712 order signer.
///
/// `garnet-blockchain` implements this for `GarnetBlockchainClient` (real
/// `PrivateKeySigner`); tests use [`InMemoryOrderSigner`] from this crate.
///
/// # Thread safety
///
/// Implementations must be `Send + Sync`; the trait object is held as
/// `Arc<dyn OrderSigner>` and shared across the trading engine.
#[async_trait]
pub trait OrderSigner: Send + Sync {
    /// Sign `order` and return the `r || s || v` bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Auth`] if signing fails (key error, hardware
    /// signer unreachable, etc.).
    async fn sign_order(&self, order: &UnsignedOrder) -> Result<SignedOrder, ClobError>;
}

// ---------------------------------------------------------------------------
// In-memory signer for tests
// ---------------------------------------------------------------------------

/// EIP-712 signer backed by an in-memory [`alloy::signers::local::PrivateKeySigner`].
///
/// Useful in tests and TEST trading mode when a real `GarnetBlockchainClient`
/// is not wired in.  Production deployments must inject the blockchain client
/// implementation so the key material is loaded from `PRIVATE_KEY` once.
pub struct InMemoryOrderSigner {
    inner: alloy::signers::local::PrivateKeySigner,
}

impl fmt::Debug for InMemoryOrderSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemoryOrderSigner")
            .field("address", &self.inner.address())
            .finish()
    }
}

impl InMemoryOrderSigner {
    /// Wrap an existing [`alloy::signers::local::PrivateKeySigner`].
    #[must_use]
    pub fn new(inner: alloy::signers::local::PrivateKeySigner) -> Self {
        Self { inner }
    }

    /// Parse a hex-encoded private key (with or without `0x` prefix).
    ///
    /// # Errors
    ///
    /// Returns [`ClobError::Auth`] if the key is malformed.
    pub fn from_hex_key(hex: &str) -> Result<Self, ClobError> {
        let inner: alloy::signers::local::PrivateKeySigner = hex
            .trim()
            .parse()
            .map_err(|e| ClobError::Auth(format!("parse private key: {e}")))?;
        Ok(Self { inner })
    }

    /// Public address of this signer.
    #[must_use]
    pub fn address(&self) -> Address {
        self.inner.address()
    }
}

#[async_trait]
impl OrderSigner for InMemoryOrderSigner {
    async fn sign_order(&self, order: &UnsignedOrder) -> Result<SignedOrder, ClobError> {
        use alloy::signers::SignerSync;

        let sol = order.to_sol();
        let domain = order.domain();
        let sig = self
            .inner
            .sign_typed_data_sync(&sol, &domain)
            .map_err(|e| ClobError::Auth(format!("EIP-712 sign: {e}")))?;
        Ok(SignedOrder {
            order: order.clone(),
            signature: sig.as_bytes(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use rust_decimal_macros::dec;

    /// Deterministic test private key.  Public address:
    /// `0x90F79bf6EB2c4f870365E785982E1f101E93b906`.
    const TEST_KEY_HEX: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";

    fn fixture_order() -> UnsignedOrder {
        let (maker_amount, taker_amount) =
            UnsignedOrder::amounts_from_price_size(Side::Buy, dec!(0.62), dec!(100)).unwrap();
        UnsignedOrder {
            salt: U256::from(42u64),
            maker: address!("0x90F79bf6EB2c4f870365E785982E1f101E93b906"),
            signer: address!("0x90F79bf6EB2c4f870365E785982E1f101E93b906"),
            token_id: U256::from(1234u64),
            maker_amount,
            taker_amount,
            expiration: U256::ZERO,
            side: Side::Buy,
            signature_type: SignatureType::Eoa,
            timestamp: U256::from(1_700_000_000_000u64),
            metadata: B256::ZERO,
            builder: B256::ZERO,
            exchange: Exchange::CtfExchange,
            verifying_contract: address!("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E"),
            chain_id: 137,
        }
    }

    #[test]
    fn signature_type_repr_matches_solidity_enum() {
        assert_eq!(SignatureType::Eoa.as_u8(), 0);
        assert_eq!(SignatureType::PolyProxy.as_u8(), 1);
        assert_eq!(SignatureType::PolyGnosisSafe.as_u8(), 2);
        assert_eq!(SignatureType::Poly1271.as_u8(), 3);
    }

    #[test]
    fn eip712_type_hash_matches_official_v2_string() {
        // Must equal keccak256 of the official SDK V2 type-hash string
        // (rs-clob-client-v2/src/clob/client.rs).
        use alloy::primitives::keccak256;
        const V2_TYPE: &str = concat!(
            "Order(uint256 salt,address maker,address signer,uint256 tokenId,",
            "uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,",
            "uint256 timestamp,bytes32 metadata,bytes32 builder)"
        );
        assert_eq!(
            Order::eip712_type_hash(&Order::default()).0,
            keccak256(V2_TYPE).0
        );
    }

    #[test]
    fn amounts_from_price_size_buy_round_trip() {
        let (maker, taker) =
            UnsignedOrder::amounts_from_price_size(Side::Buy, dec!(0.625), dec!(40)).unwrap();
        // 0.625 * 40 = 25.0 pUSD → 25_000_000 base units
        assert_eq!(maker, U256::from(25_000_000u64));
        // 40 shares → 40_000_000 base units
        assert_eq!(taker, U256::from(40_000_000u64));
    }

    #[test]
    fn amounts_from_price_size_sell_swaps() {
        let (maker, taker) =
            UnsignedOrder::amounts_from_price_size(Side::Sell, dec!(0.50), dec!(10)).unwrap();
        // SELL: maker provides shares, takes pUSD
        assert_eq!(maker, U256::from(10_000_000u64));
        assert_eq!(taker, U256::from(5_000_000u64));
    }

    #[test]
    fn amounts_reject_negative() {
        let err =
            UnsignedOrder::amounts_from_price_size(Side::Buy, dec!(-1), dec!(10)).unwrap_err();
        assert!(matches!(err, ClobError::Config(_)));
    }

    #[test]
    fn domain_name_picks_correct_string() {
        assert_eq!(
            Exchange::CtfExchange.domain_name(),
            "Polymarket CTF Exchange"
        );
        assert_eq!(
            Exchange::NegRiskCtfExchange.domain_name(),
            "Polymarket Neg Risk CTF Exchange"
        );
    }

    #[test]
    fn eip712_signing_hash_is_deterministic() {
        let order = fixture_order();
        let h1 = order.eip712_signing_hash();
        let h2 = order.eip712_signing_hash();
        assert_eq!(h1, h2, "hash must be deterministic");
        // Sanity: 32 non-zero bytes.
        assert_ne!(h1, [0u8; 32]);
    }

    #[tokio::test]
    async fn in_memory_signer_produces_65_byte_sig() {
        let key = InMemoryOrderSigner::from_hex_key(TEST_KEY_HEX).unwrap();
        let order = fixture_order();
        let signed_order = key.sign_order(&order).await.unwrap();
        assert_eq!(signed_order.signature.len(), 65);
        // r and s are 32 bytes each; v is the last byte and must be 27 or 28.
        let v = signed_order.signature[64];
        assert!(v == 27 || v == 28, "expected v ∈ {{27, 28}}, got {v}");
        // Hex format
        let hex = signed_order.signature_hex();
        assert_eq!(hex.len(), 2 + 130);
        assert!(hex.starts_with("0x"));
    }

    #[tokio::test]
    async fn signer_signature_is_deterministic_for_fixed_inputs() {
        let key = InMemoryOrderSigner::from_hex_key(TEST_KEY_HEX).unwrap();
        let order = fixture_order();
        let a = key.sign_order(&order).await.unwrap();
        let b = key.sign_order(&order).await.unwrap();
        assert_eq!(
            a.signature, b.signature,
            "same order must produce same signature"
        );
    }

    #[tokio::test]
    async fn signer_address_matches_test_key() {
        let signer = InMemoryOrderSigner::from_hex_key(TEST_KEY_HEX).unwrap();
        assert_eq!(
            signer.address(),
            address!("0x90F79bf6EB2c4f870365E785982E1f101E93b906"),
        );
    }
}
