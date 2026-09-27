//! Blockchain client settings.
//!
//! Out of the predecessor's config the client needed six fields — they are here.
//! The rest of it concerned wallet selection and the maker track.

#[derive(Debug, Clone)]
pub struct BlockchainSettings {
    /// The source of Polygon gas prices.
    pub gas_station_url: String,
    pub gas_cache_ttl_sec: u64,
    /// The source of the MATIC price, for converting transaction cost into USDC.
    pub matic_price_url: String,
    pub matic_price_ttl_sec: u64,
    /// The ceiling on transaction cost. Above it, we do not send.
    pub max_gas_cost_usdc: rust_decimal::Decimal,
    pub rpc_request_timeout_sec: u64,
}

impl Default for BlockchainSettings {
    fn default() -> Self {
        Self {
            gas_station_url: "https://gasstation.polygon.technology/v2".to_string(),
            gas_cache_ttl_sec: 30,
            matic_price_url: String::new(),
            matic_price_ttl_sec: 300,
            max_gas_cost_usdc: rust_decimal::Decimal::new(50, 2),
            rpc_request_timeout_sec: 20,
        }
    }
}
