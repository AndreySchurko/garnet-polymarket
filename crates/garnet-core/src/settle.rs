//! The settlement worker.
//!
//! Three rules, each paid for by a defect in the predecessor:
//!   * a market counts as resolved **only** by `tokens[].winner` — a sale at 0.99 is
//!     arithmetically indistinguishable from a redemption, and on that basis the
//!     predecessor fabricated 3,659 false resolutions;
//!   * a win is determined by comparing `token_id` values, not labels;
//!   * the queue runs **from old to new** with an attempt counter: in the
//!     predecessor it ran newest-first and jammed completely.

use crate::detect::MarketSource;
use garnet_db::{Db, Mode};
use rust_decimal::Decimal;

/// Redemption of a winning position. In shadow, simply a credit to the ledger.
///
/// It takes the metadata whole rather than a `token_id`: the contract addresses an
/// outcome by the pair "condition + outcome index", and a `token_id` does not contain
/// that.
pub trait Redeemer {
    fn redeem(
        &self,
        meta: &crate::market_meta::MarketMeta,
        size: Decimal,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<String>>> + Send;
}

/// A resolved position in full: what has to be said to the operator and to the bus.
///
/// A counter is not enough for that: resolution is the only outcome of the trading
/// path, and "closed 1" says neither which market, nor whether we won, nor what it
/// cost. The rows are returned outwards and `bin` publishes them: the core knows
/// nothing about the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub position_id: i64,
    pub wallet: String,
    pub token_id: String,
    pub mode: Mode,
    /// The real label of the winning side: Up/Down, Over/Under, a team name.
    pub resolved_outcome: String,
    pub won: bool,
    /// How many shares lived to resolution.
    pub size: Decimal,
    pub payout_usd: Decimal,
    pub cost_usd: Decimal,
    pub proceeds_usd: Decimal,
    pub fees_usd: Decimal,
    /// The redemption hash. `None` when the platform credits the payout.
    pub tx_hash: Option<String>,
}

impl Settled {
    /// The position's bottom line: payout and sale proceeds against stake and fees.
    #[must_use]
    pub fn pnl_usd(&self) -> Decimal {
        self.payout_usd + self.proceeds_usd - self.cost_usd - self.fees_usd
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SettleReport {
    /// How many positions were checked.
    pub checked: usize,
    /// How many turned out to be resolved.
    pub resolved: usize,
    /// The closed positions in full. The length is how many redemptions were
    /// submitted (live) or payouts credited (shadow).
    pub settled: Vec<Settled>,
    /// Positions whose metadata could not be read: `(token_id, reason)`.
    ///
    /// Invariant 13 — a divergence is never fixed silently. A skip without a trace
    /// cost a day: the payout for one market was sitting in the account while
    /// "checked 3, resolved 0" read as "the markets are still live".
    pub failed: Vec<(String, String)>,
}

/// One pass over the queue.
pub async fn settle_once<M: MarketSource, R: Redeemer>(
    db: &Db,
    markets: &M,
    redeemer: &R,
    limit: i64,
) -> anyhow::Result<SettleReport> {
    let mut report = SettleReport::default();

    for pos in db.positions().open_for_settlement(limit).await? {
        report.checked += 1;
        db.positions().note_attempt(pos.id).await?;

        let meta = match markets.get(&pos.token_id).await {
            Ok(m) => m,
            Err(e) => {
                // The attempt is already counted, we will come back on the next
                // pass — but the reason must live to reach the report. In full:
                // `to_string()` prints only anyhow's outer context, and on
                // 06.09.2026 the journal received "market metadata for <token>"
                // with no source — the same blindness that was cleaned out of the
                // loops by moving from `{e}` to `{e:#}`.
                report.failed.push((pos.token_id.clone(), format!("{e:#}")));
                continue;
            }
        };

        // No resolution without winner. Neither the price, nor the market being
        // closed, nor a sale at 0.99 is a resolution.
        let (Some(won), Some(label)) = (meta.we_won(), meta.resolved_outcome.clone()) else {
            continue;
        };
        report.resolved += 1;

        let size = pos.open_size();
        let payout = if won { size } else { Decimal::ZERO };

        let tx = if won && pos.mode == Mode::Live {
            redeemer.redeem(&meta, size).await?
        } else {
            None
        };

        db.settlements()
            .record(pos.id, &pos.token_id, &label, won, payout, tx.as_deref())
            .await?;
        db.positions().close(pos.id).await?;
        report.settled.push(Settled {
            position_id: pos.id,
            wallet: pos.wallet.clone(),
            token_id: pos.token_id.clone(),
            mode: pos.mode,
            resolved_outcome: label,
            won,
            size,
            payout_usd: payout,
            cost_usd: pos.cost_usd,
            proceeds_usd: pos.proceeds_usd,
            fees_usd: pos.fees_usd,
            tx_hash: tx,
        });
    }

    Ok(report)
}
