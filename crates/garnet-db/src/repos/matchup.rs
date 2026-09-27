//! The quality of copying: our result against the leader's.
//!
//! Absolute P&L answers "did we make money" and does **not** answer "because of
//! execution or because of the choice of wallet". The controls we have — the slippage
//! threshold, the slice window, the rate limit, the exposure ceiling — tune only the
//! former. Turning them by the absolute result means looking at the wrong instrument
//! (invariant 49).
//!
//! Three decisions in this file are not cosmetic:
//!
//! * **the denominator starts at the moment the wallet was assigned**, not at the
//!   leader's whole history. Comparing ourselves against entries we could not by
//!   construction have seen means measuring the moment of assignment, not execution
//!   (invariant 25);
//! * **our half includes the fee.** On a market with a rate of 0.07 a gap without the
//!   fee is not an execution gap but our own undercount (invariant 4);
//! * **nothing to measure with is `None`, not zero.** Zero reads as "copied
//!   perfectly", and substituting it where there was no number merges two different
//!   outcomes (invariant 27).
//!
//! And separately invariant 50: a position the leader opened and we skipped does not
//! enter the "our percent against theirs" comparison at all — and the average gap over
//! the positions taken systematically flatters us by exactly the discarded tail. So the
//! skips are counted alongside, as a separate line, rather than dissolving into the
//! average.

use crate::Mode;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPool;

/// Fractions -> cents and fractions -> percent: the multiplier is the same.
const CENTS: Decimal = Decimal::ONE_HUNDRED;

/// How the comparison of the two halves ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchupStatus {
    /// Both halves exist — the gap is measurable.
    Matched,
    /// They entered, we took a decision — and opened no position. `reason` is the verdict
    /// from `signals`.
    Skipped { reason: String },
    /// They entered while no decision was taken in this mode at all: the wallet was
    /// running in another mode, was disabled, or the engine was not running at the time.
    ///
    /// Deliberately separate from [`MatchupStatus::Skipped`]. A skip is our own refusal,
    /// and it measures the truncated tail of copying (invariant 50); an unevaluated trade
    /// contains no decision of ours, and counting it as a skip would inflate the tail by
    /// exactly somebody else's mode.
    NotEvaluated,
    /// We hold, they have already exited — including by merging.
    HeOut,
}

/// Raw sums from SQL. The arithmetic lives in Rust so that it can be verified by a test
/// rather than only by eye in the query.
#[derive(Debug, sqlx::FromRow)]
struct RawMatchup {
    wallet: String,
    token_id: String,
    question: String,
    outcome_label: String,
    size_bought: Decimal,
    size_sold: Decimal,
    cost_usd: Decimal,
    proceeds_usd: Decimal,
    fees_usd: Decimal,
    closed_at: Option<DateTime<Utc>>,
    won: Option<bool>,
    payout_usd: Decimal,
    his_buy_size: Decimal,
    his_buy_cost: Decimal,
    his_sell_size: Decimal,
    his_sell_proceeds: Decimal,
    skip_verdict: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MatchupRow {
    pub wallet: String,
    pub token_id: String,
    pub question: String,
    pub outcome_label: String,
    pub mode: Mode,

    /// Our half: from `positions`, fees included.
    pub our_size: Decimal,
    /// The effective entry price: `(stake + fees) / shares bought`.
    pub our_avg: Decimal,
    /// The return in percent. `None` — the position is still open, there is no result.
    pub our_pct: Option<Decimal>,

    /// Their half: from `leader_trades` with `ts_trade >= wallets.created_at`.
    pub his_size: Decimal,
    pub his_avg_since: Decimal,
    /// `None` — they have not exited and the market has not resolved: there is nothing to
    /// compute from.
    pub his_pct: Option<Decimal>,

    pub status: MatchupStatus,
}

impl MatchupRow {
    /// The return gap in percentage points. Positive when they are ahead.
    ///
    /// `None` if either half is missing: the gap between a number and the absence of a
    /// number is not zero.
    #[must_use]
    pub fn gap(&self) -> Option<Decimal> {
        match (self.his_pct, self.our_pct) {
            (Some(his), Some(our)) => Some(his - our),
            _ => None,
        }
    }

    /// The entry difference in cents, signed **against us**: a positive number means we
    /// paid more than the leader.
    ///
    /// `None` — nothing to measure with. Zero here would mean a perfect copy, and
    /// returning it in place of "there was no price" means lying in the bot's favour.
    #[must_use]
    pub fn entry_diff_c(&self) -> Option<Decimal> {
        if self.our_avg <= Decimal::ZERO || self.his_avg_since <= Decimal::ZERO {
            return None;
        }
        Some((self.our_avg - self.his_avg_since) * CENTS)
    }
}

#[derive(Debug, Clone)]
pub struct MatchupReport {
    pub mode: Mode,
    pub rows: Vec<MatchupRow>,
    /// The average return gap over the rows where it is measurable.
    pub avg_gap_pts: Option<Decimal>,
    /// The average entry difference in cents over the rows where it is measurable.
    pub avg_entry_diff_c: Option<Decimal>,
    /// How many rows yielded a measurable entry difference.
    pub n_measurable: usize,
    /// How many leader entries we weighed and did not take. They do not enter the averages
    /// (invariant 50).
    pub n_skipped: usize,
    /// How many leader entries were not evaluated in this mode at all. These are not
    /// skips: adding them to `n_skipped` would bring back the very conflation.
    pub n_unevaluated: usize,
}

pub struct MatchupRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> MatchupRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// The comparison of the halves over a window. `since = None` — over all time.
    ///
    /// The window selects **rows**; it does not affect how the halves are computed:
    /// `his_avg_since` is always taken from the moment the wallet was assigned, otherwise
    /// it is no longer invariant 25 but an average over an arbitrary stretch.
    ///
    /// The mode is an argument rather than a grouping column: a paper result cannot be
    /// added to a real one, and the only reliable way of not doing so is never letting
    /// them end up in the same result set.
    pub async fn report(
        &self,
        mode: Mode,
        since: Option<DateTime<Utc>>,
    ) -> anyhow::Result<MatchupReport> {
        let raw = sqlx::query_as::<_, RawMatchup>(
            "WITH his AS (
               SELECT t.wallet, t.token_id,
                      COALESCE(sum(t.size)          FILTER (WHERE t.side = 'buy'),  0) AS his_buy_size,
                      COALESCE(sum(t.price * t.size) FILTER (WHERE t.side = 'buy'),  0) AS his_buy_cost,
                      COALESCE(sum(t.size)          FILTER (WHERE t.side = 'sell'), 0) AS his_sell_size,
                      COALESCE(sum(t.price * t.size) FILTER (WHERE t.side = 'sell'), 0) AS his_sell_proceeds,
                      min(t.ts_trade) AS his_first_ts
                 FROM leader_trades t
                 JOIN wallets w ON w.address = t.wallet
                -- Invariant 25: only from the moment the wallet was assigned.
                WHERE t.ts_trade >= w.created_at
                GROUP BY t.wallet, t.token_id
             ), ours AS (
               SELECT p.wallet, p.token_id, p.opened_at, p.closed_at,
                      p.size_bought, p.size_sold, p.cost_usd, p.proceeds_usd, p.fees_usd,
                      s.won, COALESCE(s.payout_usd, 0) AS payout_usd
                 FROM positions p
                 LEFT JOIN settlements s ON s.position_id = p.id
                WHERE p.mode = $1::mode
             ), skips AS (
               SELECT DISTINCT ON (s.wallet, t.token_id) s.wallet, t.token_id, s.verdict
                 FROM signals s
                 JOIN leader_trades t ON t.id = s.leader_trade_id
                WHERE s.mode = $1::mode
                ORDER BY s.wallet, t.token_id, s.id DESC
             ), txt AS (
               SELECT DISTINCT ON (token_id) token_id, market_text, outcome_text
                 FROM leader_trades ORDER BY token_id, id DESC
             )
             SELECT COALESCE(o.wallet, h.wallet)                      AS wallet,
                    COALESCE(o.token_id, h.token_id)                  AS token_id,
                    COALESCE(m.question, txt.market_text, '')         AS question,
                    COALESCE(m.outcome_label, txt.outcome_text, '')   AS outcome_label,
                    COALESCE(o.size_bought, 0)                        AS size_bought,
                    COALESCE(o.size_sold, 0)                          AS size_sold,
                    COALESCE(o.cost_usd, 0)                           AS cost_usd,
                    COALESCE(o.proceeds_usd, 0)                       AS proceeds_usd,
                    COALESCE(o.fees_usd, 0)                           AS fees_usd,
                    o.closed_at                                       AS closed_at,
                    o.won                                             AS won,
                    COALESCE(o.payout_usd, 0)                         AS payout_usd,
                    COALESCE(h.his_buy_size, 0)                       AS his_buy_size,
                    COALESCE(h.his_buy_cost, 0)                       AS his_buy_cost,
                    COALESCE(h.his_sell_size, 0)                      AS his_sell_size,
                    COALESCE(h.his_sell_proceeds, 0)                  AS his_sell_proceeds,
                    sk.verdict                                        AS skip_verdict
               FROM ours o
               FULL OUTER JOIN his h ON h.wallet = o.wallet AND h.token_id = o.token_id
               LEFT JOIN markets m  ON m.token_id  = COALESCE(o.token_id, h.token_id)
               LEFT JOIN txt        ON txt.token_id = COALESCE(o.token_id, h.token_id)
               LEFT JOIN skips sk   ON sk.wallet    = COALESCE(o.wallet, h.wallet)
                                   AND sk.token_id  = COALESCE(o.token_id, h.token_id)
              WHERE ($2::timestamptz IS NULL
                     OR COALESCE(o.opened_at, h.his_first_ts) >= $2)
              ORDER BY COALESCE(o.opened_at, h.his_first_ts) DESC",
        )
        .bind(mode.as_str())
        .bind(since)
        .fetch_all(self.pool)
        .await?;

        let rows: Vec<MatchupRow> = raw.into_iter().map(|r| build(r, mode)).collect();

        let gaps: Vec<Decimal> = rows.iter().filter_map(MatchupRow::gap).collect();
        let diffs: Vec<Decimal> = rows.iter().filter_map(MatchupRow::entry_diff_c).collect();
        let n_skipped = rows
            .iter()
            .filter(|r| matches!(r.status, MatchupStatus::Skipped { .. }))
            .count();
        let n_unevaluated = rows
            .iter()
            .filter(|r| r.status == MatchupStatus::NotEvaluated)
            .count();

        Ok(MatchupReport {
            mode,
            avg_gap_pts: mean(&gaps),
            avg_entry_diff_c: mean(&diffs),
            n_measurable: diffs.len(),
            n_skipped,
            n_unevaluated,
            rows,
        })
    }
}

/// The average over the measurable ones. An empty set gives `None`, not zero: "nothing
/// to average" and "zero on average" are different answers.
fn mean(xs: &[Decimal]) -> Option<Decimal> {
    if xs.is_empty() {
        return None;
    }
    let sum: Decimal = xs.iter().copied().sum();
    sum.checked_div(Decimal::from(xs.len() as u64))
}

fn build(r: RawMatchup, mode: Mode) -> MatchupRow {
    // Our entry price — stake AND fees (invariant 4).
    let invested = r.cost_usd + r.fees_usd;
    let our_avg = if r.size_bought > Decimal::ZERO {
        invested.checked_div(r.size_bought).unwrap_or(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };

    // Only a closed position has a result: while it is open its bottom line is a quote,
    // not an event.
    let our_pct = if r.closed_at.is_some() && invested > Decimal::ZERO {
        ((r.payout_usd + r.proceeds_usd - invested) * CENTS).checked_div(invested)
    } else {
        None
    };

    let his_avg_since = if r.his_buy_size > Decimal::ZERO {
        r.his_buy_cost
            .checked_div(r.his_buy_size)
            .unwrap_or(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };
    let his_net = r.his_buy_size - r.his_sell_size;

    // Their half is computed once they have exited in full (the result is known from
    // their own sales) or once the market has resolved (the result is known from the
    // resolution). While they hold and the market is open there is nothing to compute
    // from — that is `None`.
    let his_pct = if r.his_buy_cost <= Decimal::ZERO {
        None
    } else if his_net <= Decimal::ZERO {
        ((r.his_sell_proceeds - r.his_buy_cost) * CENTS).checked_div(r.his_buy_cost)
    } else {
        r.won.and_then(|won| {
            let payout = if won { his_net } else { Decimal::ZERO };
            ((r.his_sell_proceeds + payout - r.his_buy_cost) * CENTS).checked_div(r.his_buy_cost)
        })
    };

    let our_open = r.size_bought - r.size_sold;
    let status = if r.size_bought <= Decimal::ZERO {
        match r.skip_verdict {
            None => MatchupStatus::NotEvaluated,
            // We decided to copy — and there is no position. This is not a skip by rule
            // but a copy that never happened: the exchange refused, or the fill never
            // arrived. Merging it with a slippage refusal would hide an exchange refusal
            // inside the statistics of our own thresholds.
            Some(v) if v == "copy" => MatchupStatus::Skipped {
                reason: "decided to copy, there is no position".into(),
            },
            Some(v) => MatchupStatus::Skipped { reason: v },
        }
    } else if r.his_buy_size > Decimal::ZERO && his_net <= Decimal::ZERO && our_open > Decimal::ZERO
    {
        MatchupStatus::HeOut
    } else {
        MatchupStatus::Matched
    };

    MatchupRow {
        wallet: r.wallet,
        token_id: r.token_id,
        question: r.question,
        outcome_label: r.outcome_label,
        mode,
        our_size: r.size_bought,
        our_avg,
        our_pct,
        his_size: his_net,
        his_avg_since,
        his_pct,
        status,
    }
}
