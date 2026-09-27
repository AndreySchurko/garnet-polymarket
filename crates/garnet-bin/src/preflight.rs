//! `garnet-core --preflight [--live]` — one read-only probe per dependency.
//!
//! The module's job is not "let it through or not" but to **name the broken thing**.
//! Every check prints its own line, the exit code is assembled from the lines, and a failure
//! of the live path stops looking like "startup failed".
//!
//! The decisions here are pure: the network yields numbers, and the numbers turn into a
//! verdict separately. That way the check "the balance holds USDC.e but not pUSD" has a test
//! rather than only a production run.

use garnet_blockchain::types::AllowanceStatus;
use rust_decimal::Decimal;

/// The result of one probe.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    /// What exactly was seen. On a failure, what to fix.
    pub detail: String,
}

impl Check {
    pub fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: true,
            detail: detail.into(),
        }
    }

    pub fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: false,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mark = if self.ok { "OK  " } else { "FAIL" };
        format!("{mark} {} — {}", self.name, self.detail)
    }
}

/// Who pays for transactions and whose address holds the outcome tokens.
///
/// On a Polymarket proxy account orders execute on behalf of the proxy, the relay pays the
/// gas, and with auto-payout enabled the winnings are credited without our involvement.
/// Checks of our EOA in that mode speak about the wrong address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// We hold the collateral ourselves, pay the gas ourselves and redeem ourselves.
    SelfCustody,
    /// The collateral and the tokens are on the proxy; Polymarket sends the transactions.
    Proxy,
}

impl Flow {
    #[must_use]
    pub fn of(signature_type: garnet_clob::SignatureType) -> Self {
        match signature_type {
            garnet_clob::SignatureType::Eoa => Self::SelfCustody,
            _ => Self::Proxy,
        }
    }
}

/// The exit code: zero only if not a single probe failed.
#[must_use]
pub fn report(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|c| !c.ok))
}

/// Wrap a probe's result into a report line.
///
/// The first unavailable dependency has no right to abort the run: the point of preflight is
/// to see every broken thing in one pass rather than fixing them one at a time with a
/// restart after each.
pub fn probe<T: std::fmt::Display, E: std::fmt::Display>(
    name: impl Into<String>,
    result: Result<T, E>,
) -> Check {
    match result {
        Ok(v) => Check::pass(name, v.to_string()),
        Err(e) => Check::fail(name, e.to_string()),
    }
}

/// The trading collateral is **pUSD**, not USDC.e.
///
/// USDC.e in the balance looks like money and is money, but no order will stand on it until
/// it has been wrapped through `CollateralOnramp`. The difference between "there is no
/// money" and "the money is in the wrong form" means different actions by the operator, and
/// the check has to tell them apart.
#[must_use]
pub fn collateral(pusd: Decimal, usdc_e: Decimal, floor: Decimal) -> Check {
    if pusd >= floor {
        return Check::pass("collateral", format!("pUSD {pusd}, floor {floor}"));
    }
    if pusd + usdc_e >= floor {
        return Check::fail(
            "collateral",
            format!("pUSD {pusd} against a floor of {floor}, but there is USDC.e {usdc_e}: a wrap is needed"),
        );
    }
    Check::fail(
        "collateral",
        format!("pUSD {pusd} and USDC.e {usdc_e}, {floor} required: top up the wallet"),
    )
}

/// Allowances to the exchange and the adapters.
///
/// A failure lists the missing ones by name: "an allowance is not granted" without a name
/// leaves six candidates.
#[must_use]
pub fn allowances(status: &AllowanceStatus, flow: Flow) -> Check {
    if flow == Flow::Proxy {
        return Check::pass(
            "allowances",
            "not ours: the outcome tokens sit on the proxy, and it grants the allowances",
        );
    }
    let missing: Vec<&str> = [
        ("pusd_to_ctf_exchange", status.pusd_to_ctf_exchange),
        (
            "pusd_to_neg_risk_exchange",
            status.pusd_to_neg_risk_exchange,
        ),
        ("ctf_to_ctf_exchange", status.ctf_to_ctf_exchange),
        ("ctf_to_neg_risk_exchange", status.ctf_to_neg_risk_exchange),
        ("ctf_to_neg_risk_adapter", status.ctf_to_neg_risk_adapter),
        (
            "ctf_to_collateral_adapter",
            status.ctf_to_collateral_adapter,
        ),
    ]
    .into_iter()
    .filter_map(|(name, set)| (!set).then_some(name))
    .collect();

    if missing.is_empty() {
        Check::pass("allowances", "all six are granted")
    } else {
        Check::fail("allowances", format!("not granted: {}", missing.join(", ")))
    }
}

/// Gas.
///
/// A redemption is a transaction. Without MATIC the orders go through as if nothing were
/// wrong, while a winning position does not turn into money.
#[must_use]
pub fn gas(matic: Decimal, floor: Decimal, flow: Flow) -> Check {
    if flow == Flow::Proxy {
        return Check::pass(
            "gas",
            "not ours: Polymarket's relay sends the proxy's transactions",
        );
    }
    if matic >= floor {
        Check::pass("gas", format!("MATIC {matic}, floor {floor}"))
    } else {
        Check::fail(
            "gas",
            format!("MATIC {matic} against a floor of {floor}: redemption will not go through"),
        )
    }
}

/// The stakes of wallets moved to live.
///
/// A smoke test costs a dollar only as long as the row in the database says a dollar: the
/// stake lives in the database and changes on the fly, so it cannot be treated as known
/// from the config.
#[must_use]
pub fn live_wallet_stakes(wallets: &[(String, Decimal)], cap: Decimal) -> Check {
    if wallets.is_empty() {
        return Check::fail("live wallets", "none: there is nothing to fire with");
    }
    let over: Vec<String> = wallets
        .iter()
        .filter(|(_, stake)| *stake > cap)
        .map(|(addr, stake)| format!("{addr} stakes {stake}"))
        .collect();

    if over.is_empty() {
        Check::pass(
            "live wallets",
            format!("{}, all within {cap}", wallets.len()),
        )
    } else {
        Check::fail(
            "live wallets",
            format!("above the ceiling of {cap}: {}", over.join("; ")),
        )
    }
}

/// The trading account, the signer and the holder of the collateral.
///
/// `POLY_ADDRESS` is the owner of the L2 key, and the signer has to be that owner: a key
/// issued by a different address authorises REST and signs orders on behalf of an account
/// the exchange will not credit them to.
///
/// The funder, meanwhile, **must** differ: that is the whole point of a proxy — it holds the
/// money, the EOA only signs. The first version of this check demanded equality and declared
/// Polymarket's standard arrangement broken.
#[must_use]
pub fn trading_identity(poly_address: &str, signer: &str, funder: Option<&str>) -> Check {
    if !poly_address.eq_ignore_ascii_case(signer) {
        return Check::fail(
            "trading account",
            format!("POLY_ADDRESS {poly_address} is not the signer {signer}: the key was issued by another address"),
        );
    }
    match funder {
        None => Check::pass("trading account", format!("{poly_address}, EOA flow")),
        Some(f) if f.eq_ignore_ascii_case(signer) => Check::fail(
            "trading account",
            format!(
                "the signature type goes through a proxy, yet the funder {f} is the signer itself"
            ),
        ),
        Some(f) => Check::pass(
            "trading account",
            format!("{poly_address} signs, {f} holds the collateral"),
        ),
    }
}

/// The signature type against what the wallet actually is.
///
/// Polymarket wallets come in two kinds with different signatures: signing in through Magic
/// Link gives a proxy (`POLY_PROXY`), signing in through a browser wallet gives a Gnosis
/// Safe (`POLY_GNOSIS_SAFE`). A mistake here is caught by nothing except a refusal from the
/// exchange on a live order, and it looks like a problem with the keys.
///
/// `funder_is_safe` — whether the funder's contract answered `masterCopy()`
/// Whether there is a loss stop.
///
/// Zero means "there is no stop" deliberately (a wrong limit is more dangerous than an
/// absent one, because it looks like protection), but before firing with real money it has
/// to be said out loud: until 06.09.2026 the killswitch knew five mechanical reasons and not
/// one about money — a bot that merely loses did not stop on its own.
pub fn loss_stop(limit_usd: Decimal) -> Check {
    if limit_usd > Decimal::ZERO {
        Check::pass("loss stop", format!("a limit of ${limit_usd} per UTC day"))
    } else {
        Check::fail(
            "loss stop",
            "not set: [risk] daily_loss_limit_usd = 0. The bot will not stop by itself, however much it loses",
        )
    }
}

/// (selector `0xa619486e`): that is the interface of a Gnosis Safe proxy.
#[must_use]
pub fn wallet_signature_match(
    signature_type: garnet_clob::SignatureType,
    funder_is_safe: Option<bool>,
) -> Check {
    use garnet_clob::SignatureType as S;
    match (signature_type, funder_is_safe) {
        (S::Eoa, _) | (_, None) => {
            Check::pass("signature type", "EOA: there is no wallet contract")
        }
        // Does not arrive via `resolve`: the config knows three types. Kept so that adding a
        // fourth one in the SDK does not pass silently.
        (S::Poly1271, _) => Check::pass(
            "signature type",
            "EIP-1271: the contract verifies the signature itself",
        ),
        (S::PolyGnosisSafe, Some(true)) => {
            Check::pass("signature type", "POLY_GNOSIS_SAFE, the funder is a Safe")
        }
        (S::PolyProxy, Some(false)) => {
            Check::pass("signature type", "POLY_PROXY, the funder is a proxy")
        }
        (S::PolyProxy, Some(true)) => Check::fail(
            "signature type",
            "the funder answers masterCopy(): this is a Gnosis Safe, POLY_GNOSIS_SAFE is required",
        ),
        (S::PolyGnosisSafe, Some(false)) => Check::fail(
            "signature type",
            "the funder is not a Safe: a Magic Link proxy requires POLY_PROXY",
        ),
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

use crate::clob_live::{self, LiveEnv};
use garnet_blockchain::traits::BlockchainClient;
use garnet_clob::traits::ClobClient;
use garnet_config::Config;
use garnet_db::{Db, Mode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The floors below which the live path counts as not ready.
#[derive(Debug, Clone)]
pub struct Thresholds {
    /// The minimum collateral in pUSD.
    pub collateral_usd: Decimal,
    /// The minimum MATIC for gas: without it a redemption will not go through.
    pub gas_matic: Decimal,
    /// The ceiling on a live wallet's stake for the duration of the smoke test.
    pub smoke_stake_cap: Decimal,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            collateral_usd: Decimal::new(5, 0),
            gas_matic: Decimal::new(5, 1),
            smoke_stake_cap: Decimal::ONE,
        }
    }
}

/// How long we wait for the first RTDS frame. Silence beyond that means the feed is not
/// running.
const RTDS_PROBE: Duration = Duration::from_secs(20);

/// The probes that do not require keys.
pub async fn run_base(cfg: &Config) -> Vec<Check> {
    let mut checks = Vec::new();

    let db = Db::connect(&cfg.database_url).await;
    match &db {
        Ok(db) => checks.push(probe(
            "postgres",
            db.migrate().await.map(|()| "migrations applied"),
        )),
        Err(e) => checks.push(Check::fail("postgres", e.to_string())),
    }

    let settings = garnet_clob::ClobSettings {
        clob_host: cfg.api.clob_host.clone(),
        ..Default::default()
    };
    match garnet_clob::GarnetClobClient::new_read_only(settings) {
        Ok(clob) => {
            checks.push(probe("clob", clob.health_check().await.map(|()| "answers")));
            checks.push(probe(
                "geo",
                clob.check_geoblock().await.map(|g| {
                    format!(
                        "ip {}, country {}, the front blocks: {}",
                        g.ip, g.country, g.blocked
                    )
                }),
            ));
        }
        Err(e) => checks.push(Check::fail("clob", e.to_string())),
    }

    checks.push(gamma_probe(&cfg.api.gamma_host).await);
    checks.push(rtds_probe(&cfg.api.rtds_url).await);
    checks
}

/// Gamma returns `feeSchedule` — the only source of the fee rate.
async fn gamma_probe(host: &str) -> Check {
    // Gamma answers 403 to the default User-Agent, so we set our own explicitly.
    let http = match reqwest::Client::builder().user_agent("garnet/2.0").build() {
        Ok(c) => c,
        Err(e) => return Check::fail("gamma", e.to_string()),
    };
    let url = format!("{host}/markets?closed=false&limit=1");
    match http.get(&url).send().await {
        Err(e) => Check::fail("gamma", e.to_string()),
        Ok(resp) => {
            match resp.json::<serde_json::Value>().await {
                Err(e) => Check::fail("gamma", format!("an unreadable response: {e}")),
                Ok(v) => {
                    let has_schedule = v
                        .as_array()
                        .and_then(|a| a.first())
                        .is_some_and(|m| m.get("feeSchedule").is_some());
                    if has_schedule {
                        Check::pass("gamma", "feeSchedule is in place")
                    } else {
                        Check::fail("gamma", "the response has no feeSchedule: there is nothing to compute the fee from")
                    }
                }
            }
        }
    }
}

/// Health is measured by flow: a socket that opened and stays silent is not healthy.
async fn rtds_probe(url: &str) -> Check {
    let seen = Arc::new(AtomicU64::new(0));
    let counter = seen.clone();
    let feed = garnet_feed::Feed::new(url.to_string());
    let _ = tokio::time::timeout(
        RTDS_PROBE,
        feed.run_once(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
        }),
    )
    .await;

    let n = seen.load(Ordering::Relaxed);
    if n > 0 {
        Check::pass("rtds", format!("frames in {} s: {n}", RTDS_PROBE.as_secs()))
    } else {
        Check::fail(
            "rtds",
            format!("not a single frame in {} s", RTDS_PROBE.as_secs()),
        )
    }
}

/// The live path's probes. They require keys and read the chain.
pub async fn run_live(cfg: &Config, t: &Thresholds) -> Vec<Check> {
    let mut checks = Vec::new();

    let live = match clob_live::resolve(&LiveEnv::from_process()) {
        Err(e) => return vec![Check::fail("keys", e.to_string())],
        Ok(None) => return vec![Check::fail("keys", "not set: there is no live path")],
        Ok(Some(live)) => live,
    };

    let flow = Flow::of(live.signature_type);
    let funder = live.funder.map(|f| format!("{f:#x}"));

    // The signature type against what the wallet actually is: a mistake here is caught only
    // by a refusal from the exchange on a live order.
    checks.push(match &funder {
        None => wallet_signature_match(live.signature_type, None),
        Some(f) => match is_gnosis_safe(f).await {
            Ok(is_safe) => wallet_signature_match(live.signature_type, Some(is_safe)),
            Err(e) => Check::fail(
                "signature type",
                format!("the funder's contract was not read: {e}"),
            ),
        },
    });

    // TRADING_MODE is read by garnet-blockchain: in TEST no redemption is sent.
    let mode = std::env::var("TRADING_MODE").unwrap_or_default();
    checks.push(if mode.eq_ignore_ascii_case("LIVE") {
        Check::pass("TRADING_MODE", "LIVE")
    } else {
        Check::fail(
            "TRADING_MODE",
            format!("`{mode}`: chain transactions will not go out"),
        )
    });

    match garnet_blockchain::client::GarnetBlockchainClient::from_env() {
        Err(e) => checks.push(Check::fail("chain", e.to_string())),
        Ok(chain) => {
            // The signer is known only to the chain: the L2 key has to belong to it.
            checks.push(trading_identity(
                &live.credentials.address,
                &format!("{:#x}", chain.eoa_address()),
                funder.as_deref(),
            ));
            match chain.balances_all().await {
                Err(e) => checks.push(Check::fail("balances", e.to_string())),
                Ok(b) => {
                    checks.push(collateral(b.pusd_balance, b.usdc_balance, t.collateral_usd));
                    checks.push(gas(b.matic_balance, t.gas_matic, flow));
                }
            }
            match chain.check_v2_allowances().await {
                Err(e) => checks.push(Check::fail("allowances", e.to_string())),
                Ok(status) => checks.push(allowances(&status, flow)),
            }
        }
    }

    // The only probe that proves the L2 keys are accepted by the exchange: a signed read
    // request. No order needs to be submitted for it.
    let settings = garnet_clob::ClobSettings {
        clob_host: cfg.api.clob_host.clone(),
        ..Default::default()
    };
    match clob_live::connect(live, settings).await {
        Err(e) => checks.push(Check::fail("signature", e.to_string())),
        Ok(client) => checks.push(probe(
            "signature",
            client
                .get_open_orders()
                .await
                .map(|o| format!("keys accepted, open orders {}", o.len())),
        )),
    }

    match Db::connect(&cfg.database_url).await {
        Err(e) => checks.push(Check::fail("live wallets", e.to_string())),
        Ok(db) => match db.wallets().list().await {
            Err(e) => checks.push(Check::fail("live wallets", e.to_string())),
            Ok(ws) => {
                let live_ones: Vec<(String, Decimal)> = ws
                    .into_iter()
                    .filter(|w| w.mode == Mode::Live && w.enabled)
                    .map(|w| (w.address, w.stake_usd))
                    .collect();
                checks.push(live_wallet_stakes(&live_ones, t.smoke_stake_cap));
            }
        },
    }

    checks
}

/// Whether the contract answers `masterCopy()` — the interface of a Gnosis Safe proxy.
///
/// The selector is `0xa619486e`. A Magic Link proxy has no such method, and the call returns
/// empty.
async fn is_gnosis_safe(address: &str) -> anyhow::Result<bool> {
    let rpc = std::env::var("POLYGON_RPC_URLS")
        .map_err(|_| anyhow::anyhow!("POLYGON_RPC_URLS is not set"))?;
    let url = rpc
        .split(',')
        .next()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow::anyhow!("POLYGON_RPC_URLS is empty"))?
        .to_string();

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_call",
        "params": [{ "to": address, "data": "0xa619486e" }, "latest"],
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;

    // An empty result or an execution error means "there is no such method".
    Ok(resp["result"]
        .as_str()
        .is_some_and(|r| r.len() >= 66 && !r.trim_start_matches("0x").trim_matches('0').is_empty()))
}
