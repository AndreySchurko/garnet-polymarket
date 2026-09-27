//! The dashboard's endpoints: who is let in and what is returned.
//!
//! The real router is tested rather than the functions in isolation: "who is one of
//! us" is decided in one place, and the only way past it is a route somebody forgot
//! to cover with it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use garnet_dash::api::{router, Ctx};
use garnet_db::Mode;
use http_body_util::BodyExt as _;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tower::ServiceExt as _;

const TOKEN: &str = "12345:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw";
const OWNER: i64 = 6_218_292_348;

fn signed(user_id: i64, token: &str) -> String {
    use hmac::{Hmac, Mac as _};
    use sha2::Sha256;

    let user = format!(r#"{{"id":{user_id},"first_name":"O"}}"#);
    let auth_date = chrono::Utc::now().timestamp().to_string();
    let check = format!("auth_date={auth_date}\nuser={user}");

    let mut secret = Hmac::<Sha256>::new_from_slice(b"WebAppData").unwrap();
    secret.update(token.as_bytes());
    let secret = secret.finalize().into_bytes();
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
    mac.update(check.as_bytes());
    let hash = hex::encode(mac.finalize().into_bytes());

    let enc: String = user
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("auth_date={auth_date}&user={enc}&hash={hash}")
}

async fn ctx(tag: &str) -> Arc<Ctx> {
    let db = garnet_db::testing::isolated_db(tag).await.unwrap();
    Arc::new(Ctx {
        db,
        bot_token: TOKEN.into(),
        owners: vec![OWNER],
        max_age_secs: 86_400,
    })
}

async fn get(ctx: &Arc<Ctx>, path: &str, init: Option<&str>) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().uri(path);
    if let Some(i) = init {
        req = req.header("X-Init-Data", i);
    }
    let resp = router(Arc::clone(ctx))
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn every_api_route_refuses_a_request_without_init_data() {
    // Forgetting to cover a single route is the only way around the check, so it is
    // verified on all of them.
    let ctx = ctx("dash_auth").await;
    for path in [
        "/api/summary",
        "/api/equity",
        "/api/wallets",
        "/api/positions",
        "/api/signals",
    ] {
        let (status, _) = get(&ctx, path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn a_signature_from_another_bot_is_refused() {
    let ctx = ctx("dash_other_bot").await;
    let (status, _) = get(&ctx, "/api/wallets", Some(&signed(OWNER, "999:OTHER"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_stranger_with_a_valid_signature_is_still_refused() {
    // The signature proves the person came from Telegram, not that they are one of us.
    let ctx = ctx("dash_stranger").await;
    let (status, _) = get(&ctx, "/api/wallets", Some(&signed(777, TOKEN))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_owner_sees_the_registry_with_percentages_not_fractions() {
    let ctx = ctx("dash_owner").await;
    ctx.db.wallets().add("0xd1", Some("whale-D")).await.unwrap();
    ctx.db
        .wallets()
        .set_slippage("0xd1", dec!(0.02))
        .await
        .unwrap();

    let (status, body) = get(&ctx, "/api/wallets", Some(&signed(OWNER, TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body[0]["display"], "whale-D",
        "the nickname outranks the address"
    );
    assert_eq!(
        body[0]["slippage_pct"], "2",
        "percent goes out, a fraction is stored"
    );
}

#[tokio::test]
async fn the_summary_carries_the_unpriced_count() {
    // A total understated by unpriced positions must admit as much in the dashboard
    // too — otherwise the figure looks more precise than it is.
    let ctx = ctx("dash_unpriced").await;
    ctx.db
        .equity()
        .record(Mode::Live, dec!(100), dec!(0), 3)
        .await
        .unwrap();

    let (status, body) = get(&ctx, "/api/summary", Some(&signed(OWNER, TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    let live = body["balances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["mode"] == "live")
        .unwrap();
    assert_eq!(live["unpriced"], 3);
}

#[tokio::test]
async fn the_dashboard_has_no_browser_login_at_all() {
    // In the predecessor these endpoints existed and were closed off at Caddy. A
    // door locked from outside is still a door: here there simply is none.
    let ctx = ctx("dash_nologin").await;
    for path in ["/login", "/api/v1/auth/login", "/api/v1/auth/rotate-jwt"] {
        let (status, _) = get(&ctx, path, Some(&signed(OWNER, TOKEN))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn renaming_a_wallet_is_the_only_thing_the_dashboard_writes() {
    let ctx = ctx("dash_rename").await;
    ctx.db.wallets().add("0xd2", None).await.unwrap();

    let resp = router(Arc::clone(&ctx))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/nickname")
                .header("X-Init-Data", signed(OWNER, TOKEN))
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"address":"0xd2","nickname":"whale-2"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let w = ctx.db.wallets().get("0xd2").await.unwrap().unwrap();
    assert_eq!(w.nickname.as_deref(), Some("whale-2"));

    let actor: String = sqlx::query_scalar(
        "SELECT actor FROM wallet_events WHERE wallet = '0xd2' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(ctx.db.pool())
    .await
    .unwrap();
    assert_eq!(
        actor,
        format!("dashboard:{OWNER}"),
        "the trace names the author"
    );
}

#[tokio::test]
async fn renaming_a_stranger_wallet_is_not_a_way_to_create_one() {
    let ctx = ctx("dash_rename_missing").await;
    let resp = router(Arc::clone(&ctx))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/nickname")
                .header("X-Init-Data", signed(OWNER, TOKEN))
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"address":"0xnope","nickname":"whale"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_nickname_has_a_length_the_interface_can_hold() {
    // A nickname is printed in every list in the bot and the dashboard. A kilobyte-long
    // string is not a "long name" but a way to make everything else unreadable — and, in
    // Telegram, where a message is truncated at 4096 bytes, to crowd out the
    // neighbouring rows.
    let ctx = ctx("dash_long_nick").await;
    ctx.db.wallets().add("0xd3", None).await.unwrap();

    let long = "x".repeat(200);
    let resp = router(Arc::clone(&ctx))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/nickname")
                .header("X-Init-Data", signed(OWNER, TOKEN))
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "address": "0xd3", "nickname": long }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        ctx.db
            .wallets()
            .get("0xd3")
            .await
            .unwrap()
            .unwrap()
            .nickname
            .is_none(),
        "a refusal changes nothing"
    );
}

#[tokio::test]
async fn a_nickname_keeps_its_quotes_and_brackets() {
    // Escaping is the concern of the presentation, not of the storage: `whale "big"` is
    // a legitimate name, and mangling it on write means lying to the operator about
    // what they entered.
    let ctx = ctx("dash_quoted_nick").await;
    ctx.db.wallets().add("0xd4", None).await.unwrap();

    let resp = router(Arc::clone(&ctx))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/nickname")
                .header("X-Init-Data", signed(OWNER, TOKEN))
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "address": "0xd4", "nickname": r#"whale "big""# })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        ctx.db
            .wallets()
            .get("0xd4")
            .await
            .unwrap()
            .unwrap()
            .nickname
            .as_deref(),
        Some(r#"whale "big""#)
    );
}
