//! The order-book types moved to the shared vocabulary: both the client and the
//! trading logic know them, and keeping two copies means diverging one day on the
//! `received_at` field, which the freshness check depends on.

pub use garnet_types::market::{BookSnapshot, PriceLevel};
