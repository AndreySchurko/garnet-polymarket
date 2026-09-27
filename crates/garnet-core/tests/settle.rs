use garnet_core::detect::MarketSource;
use garnet_core::market_meta::{parse_clob_market, MarketMeta};
use garnet_core::settle::{settle_once, Redeemer};
use garnet_db::{Db, Mode};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Mutex;

fn load_fixture(name: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct StubMarkets {
    asked: Mutex<Vec<String>>,
}

impl StubMarkets {
    fn new() -> Self {
        Self {
            asked: Mutex::new(Vec::new()),
        }
    }
    fn order(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl MarketSource for StubMarkets {
    async fn get(&self, token_id: &str) -> anyhow::Result<MarketMeta> {
        self.asked.lock().unwrap().push(token_id.to_string());
        let fixture = match token_id {
            // a crypto pair: Down won
            "tok_sol_up" | "tok_sol_down" => "clob_resolved_down.json",
            // not resolved yet
            "tok_up" | "tok_down" => "clob_crypto.json",
            "tok_lal" | "tok_bos" => "clob_sports.json",
            // An error with a source: this is what a real read failure looks like —
            // `with_context` on the outside, the cause inside.
            "tok_q_chain" => {
                return Err(
                    anyhow::anyhow!("the token's book did not name the condition")
                        .context("market metadata for tok_q_chain"),
                )
            }
            // queue tokens: the order of traversal is recorded, there is no metadata
            q if q.starts_with("tok_q") => anyhow::bail!("metadata unavailable: {q}"),
            other => anyhow::bail!("no fixture for {other}"),
        };
        parse_clob_market(&load_fixture(fixture), token_id)
    }
}

struct StubRedeemer {
    calls: Mutex<Vec<(String, Decimal)>>,
}

impl StubRedeemer {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
    fn calls(&self) -> Vec<(String, Decimal)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Redeemer for StubRedeemer {
    async fn redeem(&self, meta: &MarketMeta, size: Decimal) -> anyhow::Result<Option<String>> {
        // Redemption addresses an outcome by number: without it the winning side
        // cannot be told from the losing one within the same condition.
        assert!(
            meta.outcome_index.is_some(),
            "the outcome position must be known"
        );
        self.calls
            .lock()
            .unwrap()
            .push((meta.token_id.clone(), size));
        Ok(Some("0xredeem".into()))
    }
}

/// A Postgres schema of its own per test: settlement and reconciliation passes are
/// global in meaning, and in a shared schema a neighbouring test would change our rows.
async fn db(wallet: &str) -> Db {
    let db = garnet_db::testing::isolated_db("settle").await.unwrap();
    db.wallets().add(wallet, None).await.unwrap();
    db
}

#[tokio::test]
async fn winning_side_settles_by_token_id_not_by_label() {
    let db = db("0xsettle_win").await;
    // We bought Down at 0.40; Down is exactly what won.
    let pos = db
        .positions()
        .apply_buy(
            "0xsettle_win",
            "tok_sol_down",
            Mode::Live,
            dec!(100),
            dec!(40),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let redeemer = StubRedeemer::new();
    let report = settle_once(&db, &StubMarkets::new(), &redeemer, 100)
        .await
        .unwrap();
    assert!(report.resolved >= 1);

    let s = db
        .settlements()
        .for_position(pos.id)
        .await
        .unwrap()
        .expect("the settlement was recorded");
    assert!(s.won, "Down won — that is a win, not a loss");
    assert_eq!(
        s.payout_usd,
        dec!(100),
        "1.0 per share on the winning token_id"
    );
    assert_eq!(s.resolved_outcome, "Down", "the label is not Yes/No");
    assert_eq!(
        redeemer.calls(),
        vec![("tok_sol_down".to_string(), dec!(100))]
    );
}

#[tokio::test]
async fn losing_side_of_the_same_market_pays_nothing() {
    let db = db("0xsettle_lose").await;
    let pos = db
        .positions()
        .apply_buy(
            "0xsettle_lose",
            "tok_sol_up",
            Mode::Live,
            dec!(100),
            dec!(60),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let redeemer = StubRedeemer::new();
    settle_once(&db, &StubMarkets::new(), &redeemer, 100)
        .await
        .unwrap();

    let s = db
        .settlements()
        .for_position(pos.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!s.won);
    assert_eq!(s.payout_usd, Decimal::ZERO);
    assert!(redeemer.calls().is_empty(), "a losing side is not redeemed");
}

#[tokio::test]
async fn shadow_win_is_credited_without_a_transaction() {
    let db = db("0xsettle_shadow").await;
    let pos = db
        .positions()
        .apply_buy(
            "0xsettle_shadow",
            "tok_sol_down",
            Mode::Shadow,
            dec!(50),
            dec!(20),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let redeemer = StubRedeemer::new();
    settle_once(&db, &StubMarkets::new(), &redeemer, 100)
        .await
        .unwrap();

    let s = db
        .settlements()
        .for_position(pos.id)
        .await
        .unwrap()
        .unwrap();
    assert!(s.won);
    assert_eq!(s.payout_usd, dec!(50));
    assert_eq!(s.tx_hash, None, "there is no transaction in shadow");
    assert!(
        redeemer.calls().is_empty(),
        "shadow never goes to the blockchain"
    );
}

#[tokio::test]
async fn an_unresolved_market_is_not_settled_and_counts_an_attempt() {
    let db = db("0xsettle_open").await;
    let pos = db
        .positions()
        .apply_buy(
            "0xsettle_open",
            "tok_up",
            Mode::Live,
            dec!(10),
            dec!(5),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    settle_once(&db, &StubMarkets::new(), &StubRedeemer::new(), 100)
        .await
        .unwrap();

    assert!(
        db.settlements()
            .for_position(pos.id)
            .await
            .unwrap()
            .is_none(),
        "a sale at 0.99 and a closed market are not a resolution"
    );
    let after = db
        .positions()
        .get("0xsettle_open", "tok_up", Mode::Live)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.attempts, 1, "the attempt must be counted");
    assert!(after.closed_at.is_none(), "the position stays in the queue");
}

#[tokio::test]
async fn queue_is_walked_oldest_first() {
    let db = db("0xsettle_queue").await;
    for (tok, age) in [
        ("tok_q_new", "1 hour"),
        ("tok_q_old", "9 days"),
        ("tok_q_mid", "3 days"),
    ] {
        db.positions()
            .apply_buy(
                "0xsettle_queue",
                tok,
                Mode::Live,
                dec!(10),
                dec!(4),
                Decimal::ZERO,
            )
            .await
            .unwrap();
        sqlx::query(
            "UPDATE positions SET opened_at = now() - $2::interval
                     WHERE token_id = $1 AND wallet = '0xsettle_queue'",
        )
        .bind(tok)
        .bind(age)
        .execute(db.pool())
        .await
        .unwrap();
    }

    let markets = StubMarkets::new();
    settle_once(&db, &markets, &StubRedeemer::new(), 100)
        .await
        .unwrap();

    let ours: Vec<String> = markets
        .order()
        .into_iter()
        .filter(|t| t.starts_with("tok_q"))
        .collect();
    assert_eq!(
        ours,
        vec!["tok_q_old", "tok_q_mid", "tok_q_new"],
        "oldest to newest: newest-first jammed the predecessor's queue"
    );
}

#[tokio::test]
async fn an_unreachable_market_is_counted_and_named() {
    // Invariant 13: a divergence in the ledger is never fixed silently. The smoke test
    // of 04.09 cost three positions and nine attempts of silence — `Err(_) => continue`
    // left neither a counter nor a reason, and "checked 3, resolved 0" read as "the
    // markets are still live" while the payout for one of them was already in the
    // account.
    let db = db("0xsettle_blind").await;
    db.positions()
        .apply_buy(
            "0xsettle_blind",
            "tok_q_gone",
            Mode::Live,
            dec!(3),
            dec!(0.81),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let report = settle_once(&db, &StubMarkets::new(), &StubRedeemer::new(), 10)
        .await
        .unwrap();

    assert_eq!(report.checked, 1);
    assert_eq!(report.resolved, 0);
    assert_eq!(
        report.failed.len(),
        1,
        "an unread market must be visible as a number"
    );
    let (token, why) = &report.failed[0];
    assert_eq!(token, "tok_q_gone");
    assert!(
        why.contains("metadata unavailable"),
        "the reason must live to reach the report: {why}"
    );
}

#[tokio::test]
async fn a_failure_reason_carries_its_whole_chain() {
    // anyhow's `to_string()` prints only the outer context. On 06.09.2026 settlement
    // wrote "market metadata for <token>" into the journal and not a word about
    // whether the book was silent, Gamma empty or the network gone — there is nothing
    // to fix from such a line. The loops were cured of this by moving to `{e:#}`; the
    // settlement report kept the old blindness.
    let db = db("0xsettle_chain").await;
    db.positions()
        .apply_buy(
            "0xsettle_chain",
            "tok_q_chain",
            Mode::Live,
            dec!(3),
            dec!(0.81),
            Decimal::ZERO,
        )
        .await
        .unwrap();

    let report = settle_once(&db, &StubMarkets::new(), &StubRedeemer::new(), 10)
        .await
        .unwrap();

    let (_, why) = &report.failed[0];
    assert!(
        why.contains("market metadata"),
        "the outer context must remain: {why}"
    );
    assert!(
        why.contains("the token's book did not name the condition"),
        "the source must survive: {why}"
    );
}
