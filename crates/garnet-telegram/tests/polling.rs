//! The poll loop against real HTTP.
//!
//! The stub answers the way the Bot API does and records every request: what is checked is
//! not "we called a method" but what went out over the network — otherwise silence towards
//! a foreign chat is indistinguishable from a reply that was sent and lost.

use garnet_db::Db;
use garnet_telegram::api::Api;
use garnet_telegram::poll::poll_once;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;

const TOKEN: &str = "12345:secret";

async fn fresh(addr: &str) -> Db {
    let db = Db::connect(&garnet_db::testing::default_url())
        .await
        .unwrap();
    db.migrate().await.unwrap();
    sqlx::query("DELETE FROM wallets WHERE address = $1")
        .bind(addr)
        .execute(db.pool())
        .await
        .unwrap();
    db
}

/// A Bot API stub: it serves prepared responses and returns the requests it saw as
/// `(path, body)`.
fn stub(replies: Vec<String>) -> (String, mpsc::Receiver<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for body in replies {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 8192];
            let read = sock.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..read]).to_string();
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
            let payload = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).unwrap();
            sock.write_all(body.as_bytes()).unwrap();
            let _ = tx.send((path, payload));
        }
    });
    (host, rx)
}

fn update(update_id: i64, chat_id: i64, text: &str) -> String {
    serde_json::json!({
        "ok": true,
        "result": [{
            "update_id": update_id,
            "message": {
                "message_id": 1,
                "chat": { "id": chat_id, "type": "private" },
                "text": text
            }
        }]
    })
    .to_string()
}

#[tokio::test]
async fn a_command_from_a_listed_chat_is_answered() {
    let addr = "0x00000000000000000000000000000000000000b1";
    let db = fresh(addr).await;
    // getUpdates, then sendMessage.
    let (host, seen) = stub(vec![
        update(100, 42, &format!("/add {addr} whale")),
        r#"{"ok":true}"#.into(),
    ]);
    let api = Api::new(host, TOKEN).unwrap();

    let mut offset = 0;
    poll_once(&api, &db, &[42], &mut offset, 0).await.unwrap();

    let (path, _) = seen.recv().unwrap();
    assert!(path.contains("getUpdates"), "{path}");

    let (path, body) = seen.recv().unwrap();
    assert!(path.contains("sendMessage"), "{path}");
    assert!(
        body.contains("\"chat_id\":42"),
        "the reply goes to the same chat: {body}"
    );

    assert!(
        db.wallets().get(addr).await.unwrap().is_some(),
        "the command was executed"
    );
    assert_eq!(
        offset, 101,
        "the offset advanced: otherwise the same update arrives again"
    );
}

#[tokio::test]
async fn a_stranger_gets_nothing_at_all() {
    // Not "access denied" but silence: a reply confirms the bot exists and controls
    // something. The stub is ready to answer a second request — if one comes, the channel
    // will show it.
    let db = fresh("0x00000000000000000000000000000000000000b2").await;
    let (host, seen) = stub(vec![update(200, 999, "/wallets"), r#"{"ok":true}"#.into()]);
    let api = Api::new(host, TOKEN).unwrap();

    let mut offset = 0;
    poll_once(&api, &db, &[42], &mut offset, 0).await.unwrap();

    let (path, _) = seen.recv().unwrap();
    assert!(path.contains("getUpdates"));
    assert!(
        seen.try_recv().is_err(),
        "not a single request goes to a foreign chat"
    );
    assert_eq!(
        offset, 201,
        "but the update was read all the same: otherwise it is eternal"
    );
}

#[tokio::test]
async fn the_token_never_leaves_the_client() {
    // The token sits in the URL of every request, and naively printing a reqwest error
    // hands it over whole — into a log that ends up in a ticket.
    let (host, _seen) = stub(vec![]);
    let api = Api::new(host, TOKEN).unwrap();
    let err = api.send_message(1, "hello", &[]).await.unwrap_err();
    let shown = format!("{err:#}");
    assert!(
        !shown.contains("secret"),
        "the token leaked into the error: {shown}"
    );
}
