//! Reconciling on-chain positions against the database.
//!
//! A divergence is **never fixed silently**: a mismatch means one of the two
//! pictures of the world is wrong, and choosing which one on the operator's behalf
//! is not allowed. The reconciler's job is to notice and to say so.

use garnet_db::{Db, Mode};
use rust_decimal::Decimal;

/// The minimum tolerance, in shares.
///
/// Below this there is nothing to argue about: `NUMERIC(18,6)` and the exchange's
/// rounding produce discrepancies in the last digits, and shouting about them teaches
/// the operator not to read the reconciler.
const MIN_TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 2); // 0.01

/// The share of a position's size that falls within tolerance: one tenth of a percent.
const RELATIVE_TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 3); // 0.001

/// A tolerance that scales with the position's size (invariant 48).
///
/// An absolute tolerance on a position of a hundred thousand shares is zero; on a
/// position of three shares it is everything. So it scales, but is propped up from
/// below by a minimum: otherwise on a one-share position the tolerance would shrink
/// to a thousandth and the reconciler would again be shouting about rounding.
#[must_use]
pub fn tolerance_for(ledger: Decimal) -> Decimal {
    (ledger.abs() * RELATIVE_TOLERANCE).max(MIN_TOLERANCE)
}

/// Orders in flight for one token, **in shares**.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InFlight {
    pub buy: Decimal,
    pub sell: Decimal,
}

/// How the comparison of the ledger with the chain ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Within tolerance.
    Agreed,
    /// The divergence is explained by an order in flight (invariant 48).
    Explained { delta: Decimal, in_flight: Decimal },
    /// There is no explanation. **This is the alarm** — the only one of the four.
    Unattributed { delta: Decimal },
    /// The balance could not be read. Invariant 47: this is not "agreed".
    Unknown,
}

/// Judge a single position.
///
/// `on_chain = None` means the read did not happen. Returning `Agreed` here would
/// mean saying "checked" without having checked — the same mistake as
/// `unwrap_or(1.0)` on an empty book (invariant 27): a substitution merges two
/// different outcomes.
#[must_use]
pub fn judge(in_db: Decimal, on_chain: Option<Decimal>, flight: InFlight) -> Verdict {
    let Some(on_chain) = on_chain else {
        return Verdict::Unknown;
    };
    let delta = on_chain - in_db;
    let tolerance = tolerance_for(in_db);
    if delta.abs() <= tolerance {
        return Verdict::Agreed;
    }

    // The sign of the delta chooses the side, and the choice is not symmetric. The
    // chain holds MORE than the ledger — a buy may have been taken on whose fill we
    // failed to book. The chain holds LESS — a sale may have gone out. Explaining a
    // shortfall by a buy in flight is not allowed: a buy brings tokens in, it does
    // not take them away.
    let in_flight = if delta > Decimal::ZERO {
        flight.buy
    } else {
        flight.sell
    };
    if delta.abs() <= in_flight + tolerance {
        Verdict::Explained { delta, in_flight }
    } else {
        Verdict::Unattributed { delta }
    }
}

/// On-chain token balances.
pub trait ChainBalances {
    fn balance_of(
        &self,
        token_id: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<Decimal>> + Send;
}

/// Where an alert goes. The NATS-backed implementation arrives with `garnet-bus`.
pub trait AlertSink {
    fn alert(
        &self,
        subject: &str,
        text: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    pub token_id: String,
    pub in_db: Decimal,
    pub on_chain: Decimal,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub checked: usize,
    /// **Alarms only**: divergences for which no explanation was found.
    pub divergences: Vec<Divergence>,
    /// Positions whose market is already resolved: the tokens are rightfully absent
    /// from the chain, and settlement will close them. This is waiting, not a
    /// divergence.
    pub awaiting_settlement: usize,
    /// Divergences explained by an order in flight (invariant 48). Counted alongside
    /// rather than silently: "explained" and "agreed" are different outcomes, and if
    /// there are suddenly many explained ones, that is news in itself.
    pub explained: usize,
    /// Tokens whose balance could not be read (invariant 47). Not "agreed".
    pub unreadable: Vec<String>,
}

/// One reconciliation pass. Only live positions are reconciled: shadow has no
/// on-chain leg.
///
/// Four outcomes per position, and exactly one of them is an alarm. A reconciler that
/// shouts often stops being read — and it is the only mechanism that notices a real
/// divergence.
///
/// A resolved market is excluded from reconciliation. With auto-payout enabled the
/// platform redeems a winning position itself, and no tokens remain on the proxy
/// **before** our settlement runs.
///
/// `in_flight_window_secs = 0` disables the explanation by order: the reconciler
/// returns to its earlier behaviour, where resolution is the only excuse.
pub async fn reconcile_once<C: ChainBalances, M: crate::detect::MarketSource, A: AlertSink>(
    db: &Db,
    chain: &C,
    markets: &M,
    alerts: &A,
    in_flight_window_secs: i64,
) -> anyhow::Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let since = chrono::Utc::now() - chrono::Duration::seconds(in_flight_window_secs.max(0));

    for (token_id, size, _cost) in db.equity().open_exposure(Mode::Live).await? {
        report.checked += 1;

        // A chain read may fail, and previously `?` brought down the whole pass on
        // it: one unreadable token left ALL the others unchecked, while the report
        // said nothing about it. Now an unreadable token costs only itself
        // (invariant 47).
        let on_chain = match chain.balance_of(&token_id).await {
            Ok(v) => v,
            Err(e) => {
                report.unreadable.push(token_id.clone());
                alerts
                    .alert(
                        "alert.reconcile_unreadable",
                        &format!("{token_id}: the balance could not be read — {e}"),
                    )
                    .await?;
                continue;
            }
        };

        // Agreement is checked before asking for orders: on the happy path there is
        // no point going to the database for an explanation that will not be needed.
        if (on_chain - size).abs() <= tolerance_for(size) {
            continue;
        }

        let flight = if in_flight_window_secs > 0 {
            let (buy, sell) = db
                .signals()
                .in_flight_shares(&token_id, Mode::Live, since)
                .await?;
            InFlight { buy, sell }
        } else {
            InFlight::default()
        };

        match judge(size, Some(on_chain), flight) {
            // We would not have reached here before asking for orders, but the
            // tolerance is the same.
            Verdict::Agreed => {}
            // Unreachable: the balance was read above, otherwise we would already
            // have left via `continue`. `unreachable!()` is still unfit here — a
            // panic would kill the reconciliation loop, that is, the very mechanism
            // that exists so that divergences become known. What was not read stays
            // not read.
            Verdict::Unknown => report.unreadable.push(token_id.clone()),
            Verdict::Explained { .. } => report.explained += 1,
            Verdict::Unattributed { .. } => {
                // A resolution explains vanished tokens — and it is checked last,
                // because it costs a trip for metadata.
                //
                // Unavailable metadata is not an excuse: in that case we know nothing
                // about the resolution, and silence would mean "checked and agreed".
                let resolved = markets
                    .get(&token_id)
                    .await
                    .ok()
                    .and_then(|m| m.resolved_outcome)
                    .is_some();
                if resolved && on_chain.is_zero() {
                    report.awaiting_settlement += 1;
                    continue;
                }

                report.divergences.push(Divergence {
                    token_id: token_id.clone(),
                    in_db: size,
                    on_chain,
                });
                alerts
                    .alert(
                        "alert.reconcile_divergence",
                        &format!("{token_id}: {size} in the database, {on_chain} on chain"),
                    )
                    .await?;
            }
        }
    }

    Ok(report)
}
