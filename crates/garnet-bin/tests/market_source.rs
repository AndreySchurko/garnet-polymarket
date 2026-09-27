//! The path to the metadata of a **resolved** market.
//!
//! Measured 2026-09-04 and paid for in money: `GET /book?token_id=` on a resolved market
//! answers 404 "No orderbook exists for the requested token id". The book vanishes along
//! with trading, and the condition was exactly what we used to fetch from it — the token ->
//! condition path breaks at precisely the moment settlement needs the `winner`.
//! Three smoke-test positions stood open while the payout had already been credited.
//!
//! The fallback path is Gamma queried by the token itself, and necessarily with
//! `closed=true`: without the flag it returns a closed market as an empty array.

use garnet_bin::market_source::HttpMarkets;
use garnet_core::detect::MarketSource;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;

const CID: &str = "0x17577aa0f823a43afd36e3c9f77bf9007f09656a37d4a7bb9734397f7442659e";
const TOKEN: &str = "78750515827071533171749126745679443330143284910428860198446982709710167332570";

/// Live API responses, captured on 2026-09-04.
fn reply(path: &str) -> (u16, String) {
    if path.starts_with("/book") {
        // The market is resolved: there is no book any more.
        return (
            404,
            r#"{"error":"No orderbook exists for the requested token id"}"#.into(),
        );
    }
    if path.starts_with("/markets?") {
        // Gamma. Without `closed=true` a closed market is invisible.
        if !path.contains("closed=true") {
            return (200, "[]".into());
        }
        return (
            200,
            format!(
                r#"[{{"conditionId":"{CID}","question":"Will Ipswich Town FC vs. Liverpool FC end in a draw?","closed":true,"negRisk":true,"feesEnabled":true,"feeSchedule":{{"exponent":1,"rate":0.05,"takerOnly":true,"rebateRate":0.15}}}}]"#
            ),
        );
    }
    if path == format!("/markets/{CID}") {
        return (
            200,
            format!(
                r#"{{"condition_id":"{CID}","question":"Will Ipswich Town FC vs. Liverpool FC end in a draw?","closed":true,"neg_risk":true,"end_date_iso":"2026-09-04T21:00:00Z","tokens":[{{"token_id":"12145274755290","outcome":"Yes","winner":false}},{{"token_id":"{TOKEN}","outcome":"No","winner":true}}]}}"#
            ),
        );
    }
    (404, r#"{"error":"unexpected path"}"#.into())
}

/// A tiny HTTP stub: it answers `n` requests and dies.
fn stub(n: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..n {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let read = sock.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..read]).to_string();
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (code, body) = reply(&path);
            let head = format!(
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).unwrap();
            sock.write_all(body.as_bytes()).unwrap();
            seen.push(path);
        }
        seen
    });
    (host, handle)
}

#[tokio::test]
async fn a_resolved_market_is_read_although_its_book_is_gone() {
    // book(404) -> gamma by token(closed=true) -> clob /markets/<cid>
    let (host, server) = stub(3);
    let markets = HttpMarkets::new(host.clone(), host).unwrap();

    let meta = markets
        .get(TOKEN)
        .await
        .expect("a resolved market must be readable");

    assert_eq!(meta.condition_id, CID);
    assert_eq!(meta.outcome_label, "No");
    assert_eq!(
        meta.we_won(),
        Some(true),
        "a win is determined by token_id, not by label"
    );
    assert_eq!(meta.resolved_outcome.as_deref(), Some("No"));
    assert_eq!(meta.fee.rate, rust_decimal_macros::dec!(0.05));

    let seen = server.join().unwrap();
    assert!(
        seen.iter().any(|p| p.starts_with("/book")),
        "the fast path is tried first"
    );
    assert!(
        seen.iter()
            .any(|p| p.contains("clob_token_ids") && p.contains("closed=true")),
        "the fallback path goes to Gamma by token with closed=true: {seen:?}"
    );
}
