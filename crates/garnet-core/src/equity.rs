//! The equity snapshot.
//!
//! Live counts free funds from the on-chain USDC.e balance, shadow from the virtual
//! ledger. Open positions are valued at the current mid of the market; that is a
//! valuation, not proceeds, and the reports call it exactly that.

use garnet_db::{Db, Mode};
use rust_decimal::Decimal;

/// A source of current prices. In production the CLOB, in tests a stub.
pub trait PriceSource {
    fn mid(
        &self,
        token_id: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<Decimal>>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Equity {
    pub mode: Mode,
    pub cash_usd: Decimal,
    pub positions_value: Decimal,
    pub total_usd: Decimal,
    /// Positions for which no price is available: they are valued at zero, and that
    /// has to be visible rather than silently understating equity.
    pub unpriced: usize,
}

/// Computes and records a snapshot.
pub async fn snapshot_equity<P: PriceSource>(
    db: &Db,
    mode: Mode,
    cash_usd: Decimal,
    prices: &P,
) -> anyhow::Result<Equity> {
    let mut positions_value = Decimal::ZERO;
    let mut unpriced = 0usize;

    for (token_id, size, _cost) in db.equity().open_exposure(mode).await? {
        match prices.mid(&token_id).await? {
            Some(p) => positions_value += p * size,
            None => unpriced += 1,
        }
    }

    // The count of unpriced positions is written together with the snapshot: to
    // compute it and then lose it is to leave an understated total in the database
    // with no sign that it is incomplete.
    db.equity()
        .record(
            mode,
            cash_usd,
            positions_value,
            i32::try_from(unpriced).unwrap_or(i32::MAX),
        )
        .await?;

    Ok(Equity {
        mode,
        cash_usd,
        positions_value,
        total_usd: cash_usd + positions_value,
        unpriced,
    })
}
