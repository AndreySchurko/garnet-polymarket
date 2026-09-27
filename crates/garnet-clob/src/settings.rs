//! Client settings.
//!
//! In the predecessor the client read `TradingConfig` — a struct of hundreds of
//! fields, of which it needed three. Here are exactly those three: the rest in
//! Garnet lives either in the database (a wallet's mode, stake and slippage) or in
//! the process config.

#[derive(Debug, Clone)]
pub struct ClobSettings {
    /// The CLOB base address.
    pub clob_host: String,
    /// The ceiling on orders per minute that the client itself enforces.
    pub clob_rate_limit_orders_per_min: u32,
    /// The share of the shared egress budget allotted to the CLOB.
    pub egress_clob_rps: f64,
}

impl Default for ClobSettings {
    fn default() -> Self {
        Self {
            clob_host: "https://clob.polymarket.com".to_string(),
            clob_rate_limit_orders_per_min: 60,
            egress_clob_rps: 5.0,
        }
    }
}
