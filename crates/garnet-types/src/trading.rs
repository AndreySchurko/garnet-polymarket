//! The wallet's mode and the side of a trade.

use serde::{Deserialize, Serialize};

/// A wallet's mode. It is part of the position key: a wallet is switched between
/// shadow and live while holding open positions, and without the mode in the key
/// the paper and the real lots would add up into a single row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "mode", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Live,
    Shadow,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Live => "live",
            Mode::Shadow => "shadow",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}
