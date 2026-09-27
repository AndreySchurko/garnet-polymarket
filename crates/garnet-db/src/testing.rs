//! Test isolation.
//!
//! Some of the system's passes — settlement, reconciliation, equity snapshots — are
//! global in meaning: they walk every open position. In a shared schema a neighbouring
//! test sees our rows and changes their counters, which is why every test gets a Postgres
//! schema of its own rather than a set of wallets of its own.

use crate::Db;
use sqlx::postgres::PgPool;

/// The database the tests go to.
///
/// The default is a **separate** database, not the trading one. On 2026-09-04 the default
/// pointed at the working one, nobody had set `TEST_DATABASE_URL`, and a test run added
/// eleven wallets to the production registry — one of them in live mode. A forgotten
/// variable must drop the tests somewhere there is nothing to break.
#[must_use]
pub fn default_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://garnet:garnet@localhost:5433/garnet_test".into())
}

fn base_url() -> String {
    default_url()
}

/// Creates a fresh schema and returns a connection that looks only into it.
pub async fn isolated_db(tag: &str) -> anyhow::Result<Db> {
    Ok(isolated(tag).await?.0)
}

/// A schema counter within the process.
///
/// One clock reading is not enough. `SystemTime::now()` is coarse on macOS, and two tests
/// in the same file that start together get the **same** nanosecond — the schema under the
/// second name already exists, and the test fails on `CREATE SCHEMA` rather than on what
/// it is checking. The pair "pid + counter" separates them regardless of clock
/// resolution: the counter within a process, the pid between processes (`cargo test` runs
/// one binary per crate).
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The same, but together with the schema's URL: it is needed where a **new connection**
/// to the same data is what is being checked — for example, that the operator's stop
/// survives a restart of the process.
pub async fn isolated(tag: &str) -> anyhow::Result<(Db, String)> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The timestamp comes LAST: the cleanup below cuts it out as a tail of digits, and
    // any suffix after it would render the cleanup meaningless again.
    let schema = format!("t_{tag}_{}_{seq}_{stamp}", std::process::id());

    let admin = PgPool::connect(&base_url()).await?;

    // Otherwise schemas from previous runs pile up endlessly: anything older than an
    // hour is dropped.
    //
    // The comparison is **numeric, on the tail of the name**, not lexicographic over the
    // whole name. Previously the timestamp was compared against the string `t_<digits>`
    // while the names look like `t_<tag>_..._<digits>`: a tag's letter is always greater
    // than a digit, the condition never held and nothing was ever dropped. By 19.09.2026
    // 337 schemas had accumulated in `garnet_test` — a cleanup that silently cleans
    // nothing is worse than none at all: people rely on it.
    // No more than a handful is removed at a time. Without a limit the cleanup tried to
    // drop EVERYTHING accumulated on every call: by 19.09.2026 that was 2411 schemas,
    // that is over two thousand `DROP SCHEMA CASCADE` statements per test, with a dozen
    // tests in parallel. Under that load the run started failing in whole suites — and
    // failing not where it was broken but where it timed out.
    //
    // The limit bounds that damage rather than removing it, and the arithmetic this comment
    // used to claim — "one run creates fewer than a hundred" — is wrong: one full run
    // creates **202** schemas, measured 26.09.2026. An hour later all of them are stale at
    // once, so the next run's first tests each select the same 20 and contend for them. On
    // 26.09.2026 that took out all nine tests in `matchup` together, every query returning
    // `None`; they passed one at a time, and passed in the suite again once `garnet_test`
    // had been emptied. So when a whole test binary fails while its tests pass singly,
    // count the schemas before suspecting the code.
    let cutoff = stamp.saturating_sub(3_600_000_000_000);
    let stale: Vec<String> = sqlx::query_scalar(
        r"SELECT nspname FROM pg_namespace
           WHERE nspname LIKE 't\_%'
             AND nspname ~ '_[0-9]+$'
             AND (substring(nspname from '([0-9]+)$'))::numeric < $1
           LIMIT 20",
    )
    .bind(rust_decimal::Decimal::from(cutoff as u64))
    .fetch_all(&admin)
    .await
    .unwrap_or_default();
    for old in stale {
        let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{old}\" CASCADE"))
            .execute(&admin)
            .await;
    }

    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await?;
    admin.close().await;

    let url = format!("{}?options=-c%20search_path%3D{}", base_url(), schema);
    let db = Db::connect(&url).await?;
    db.migrate().await?;
    Ok((db, url))
}
