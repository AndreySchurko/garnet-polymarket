//! The dashboard's HTTP endpoints.
//!
//! The dashboard is a **reader**. The only thing it changes is a wallet's nickname:
//! everything that moves money lives in the bot, where there is confirmation by
//! button. A browser login endpoint does not exist here at all — not because it is
//! closed off at Caddy, but because there is none: a door locked from outside is
//! still a door.
//!
//! Every request carries `initData` in a header and is verified afresh: there are no
//! sessions and no cookies, so there is nothing to steal and nothing to rotate.

use crate::initdata::check;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use garnet_db::{Db, Mode};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Clone)]
pub struct Ctx {
    pub db: Db,
    pub bot_token: String,
    /// Who the dashboard is open to. An empty list means nobody: the same rule as the bot's.
    pub owners: Vec<i64>,
    /// How long a signed string stays valid.
    pub max_age_secs: i64,
}

/// Assemble the application.
pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/summary", get(summary))
        .route("/api/equity", get(equity))
        .route("/api/wallets", get(wallets))
        .route("/api/positions", get(positions))
        .route("/api/signals", get(signals))
        .route("/api/nickname", post(nickname))
        .with_state(ctx)
}

/// The mini-app page. Baked into the binary: a file sitting next to the binary is one
/// more thing that drifts apart on deployment.
async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

/// Who has arrived. An error is returned as the same 401 without detail: the difference
/// between "the signature does not match" and "you are not on the list" is a hint to
/// whoever is guessing.
fn who(ctx: &Ctx, headers: &HeaderMap) -> Result<i64, StatusCode> {
    let raw = headers
        .get("X-Init-Data")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let id = check(raw, &ctx.bot_token, ctx.max_age_secs).map_err(|_| StatusCode::UNAUTHORIZED)?;
    if ctx.owners.contains(&id) {
        Ok(id)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn mode_of(raw: Option<&String>) -> Mode {
    match raw.map(String::as_str) {
        Some("shadow") => Mode::Shadow,
        _ => Mode::Live,
    }
}

#[derive(Debug, Deserialize)]
pub struct Params {
    pub mode: Option<String>,
    pub period: Option<String>,
    pub n: Option<i64>,
}

fn since(period: Option<&String>) -> Option<chrono::DateTime<chrono::Utc>> {
    let hours = match period.map(String::as_str) {
        Some("all") => return None,
        Some("week") => 24 * 7,
        _ => 24,
    };
    Some(chrono::Utc::now() - chrono::Duration::hours(hours))
}

/// The main view: the balance of both modes and the result over a period.
async fn summary(
    State(ctx): State<Arc<Ctx>>,
    headers: HeaderMap,
    Query(q): Query<Params>,
) -> Result<impl IntoResponse, StatusCode> {
    who(&ctx, &headers)?;
    let mut modes = Vec::new();
    for mode in [Mode::Live, Mode::Shadow] {
        let latest = ctx.db.equity().latest(mode).await.map_err(fail)?;
        modes.push(json!({
            "mode": mode.as_str(),
            "cash_usd": latest.as_ref().map(|s| s.cash_usd.to_string()),
            "positions_value": latest.as_ref().map(|s| s.positions_value.to_string()),
            "total_usd": latest.as_ref().map(|s| s.total_usd.to_string()),
            // Unpriced positions did not enter the value: the total is understated, and
            // the reader must see that rather than take the figure as complete.
            "unpriced": latest.as_ref().map_or(0, |s| s.unpriced),
            "ts": latest.as_ref().map(|s| s.ts.to_rfc3339()),
        }));
    }

    let pnl = ctx
        .db
        .reports()
        .realised_pnl(since(q.period.as_ref()))
        .await
        .map_err(fail)?;

    Ok(Json(json!({
        "balances": modes,
        "pnl": pnl.iter().map(|r| json!({
            "mode": r.mode.as_str(),
            "closed": r.closed,
            "won": r.won,
            "cost_usd": r.cost_usd.to_string(),
            "fees_usd": r.fees_usd.to_string(),
            "payout_usd": r.payout_usd.to_string(),
            "proceeds_usd": r.proceeds_usd.to_string(),
            // The net result — already after fees.
            "pnl_usd": r.pnl_usd.to_string(),
        })).collect::<Vec<_>>(),
    })))
}

/// The points of the equity chart.
async fn equity(
    State(ctx): State<Arc<Ctx>>,
    headers: HeaderMap,
    Query(q): Query<Params>,
) -> Result<impl IntoResponse, StatusCode> {
    who(&ctx, &headers)?;
    let rows = ctx
        .db
        .equity()
        .series(mode_of(q.mode.as_ref()), 500)
        .await
        .map_err(fail)?;
    Ok(Json(json!(rows
        .iter()
        .map(|s| json!({
            "ts": s.ts.to_rfc3339(),
            "total_usd": s.total_usd.to_string(),
            "cash_usd": s.cash_usd.to_string(),
            "unpriced": s.unpriced,
        }))
        .collect::<Vec<_>>())))
}

async fn wallets(
    State(ctx): State<Arc<Ctx>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    who(&ctx, &headers)?;
    let list = ctx.db.wallets().list().await.map_err(fail)?;
    let day = ctx
        .db
        .reports()
        .pnl_by_wallet(since(None))
        .await
        .map_err(fail)?;
    Ok(Json(json!(list
        .iter()
        .map(|w| json!({
            "address": w.address,
            // The nickname takes priority over the address throughout the interface.
            "display": w.display(),
            "nickname": w.nickname,
            "mode": w.mode.as_str(),
            "stake_usd": w.stake_usd.to_string(),
            // The database holds a fraction; the outside gets percent, as a human types it.
            "slippage_pct": (w.max_slippage_pct * rust_decimal::Decimal::ONE_HUNDRED).normalize().to_string(),
            "enabled": w.enabled,
            "pnl_day": day.get(&w.address).copied().unwrap_or_default().to_string(),
        }))
        .collect::<Vec<_>>())))
}

async fn positions(
    State(ctx): State<Arc<Ctx>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    who(&ctx, &headers)?;
    let rows = ctx.db.reports().open_positions().await.map_err(fail)?;
    Ok(Json(json!(rows
        .iter()
        .map(|p| json!({
            "wallet": p.wallet,
            "display": p.nickname.clone().unwrap_or_else(|| p.wallet.clone()),
            "token_id": p.token_id,
            "mode": p.mode.as_str(),
            "size": p.size.normalize().to_string(),
            "cost_usd": p.cost_usd.round_dp(2).normalize().to_string(),
            // The text is captured at the moment of the event: the market may have closed,
            // while the row must stay readable.
            "outcome": p.outcome_text,
            "opened_at": p.opened_at.to_rfc3339(),
        }))
        .collect::<Vec<_>>())))
}

async fn signals(
    State(ctx): State<Arc<Ctx>>,
    headers: HeaderMap,
    Query(q): Query<Params>,
) -> Result<impl IntoResponse, StatusCode> {
    who(&ctx, &headers)?;
    let rows = ctx
        .db
        .reports()
        .recent_signals(q.n.unwrap_or(30).clamp(1, 200))
        .await
        .map_err(fail)?;
    Ok(Json(json!(rows
        .iter()
        .map(|s| json!({
            "ts": s.ts_signal.to_rfc3339(),
            "wallet": s.wallet,
            "display": s.nickname.clone().unwrap_or_else(|| s.wallet.clone()),
            "mode": s.mode.as_str(),
            "verdict": s.verdict,
            "target_size_usd": s.target_size_usd.round_dp(2).normalize().to_string(),
            "market": s.market_text,
            "outcome": s.outcome_text,
        }))
        .collect::<Vec<_>>())))
}

/// The nickname length limit, in characters.
const NICKNAME_MAX: usize = 64;

#[derive(Debug, Deserialize)]
pub struct NicknameBody {
    pub address: String,
    pub nickname: String,
}

/// The dashboard's only write.
async fn nickname(
    State(ctx): State<Arc<Ctx>>,
    headers: HeaderMap,
    Json(body): Json<NicknameBody>,
) -> Result<impl IntoResponse, StatusCode> {
    let by = who(&ctx, &headers)?;
    // A nickname is printed in every list in the bot and the dashboard, and a Telegram
    // message is truncated at 4096 bytes: a long name crowds out the neighbouring rows.
    // Quotes and brackets are left as they are — escaping is the concern of the
    // presentation, not of the storage.
    if body.nickname.chars().count() > NICKNAME_MAX {
        return Err(StatusCode::BAD_REQUEST);
    }
    if ctx
        .db
        .wallets()
        .get(&body.address)
        .await
        .map_err(fail)?
        .is_none()
    {
        return Err(StatusCode::NOT_FOUND);
    }
    ctx.db
        .wallets()
        .set_nickname(&body.address, &body.nickname, &format!("dashboard:{by}"))
        .await
        .map_err(fail)?;
    Ok(Json(json!({ "ok": true })))
}

/// A database failure. A code goes out, not a message: sqlx's text contains the query.
fn fail(e: anyhow::Error) -> StatusCode {
    eprintln!("dashboard: {e}");
    StatusCode::INTERNAL_SERVER_ERROR
}
