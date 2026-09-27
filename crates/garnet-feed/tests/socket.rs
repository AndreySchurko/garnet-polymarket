//! Tested against a live in-process WebSocket server.
//!
//! The main test here is that we **write nothing to the socket but the subscription**.
//! The predecessor's measurement showed that any keepalive cuts delivery by 2.5x, and a regression
//! of that kind is invisible in the logs — the connection lives, it simply stops
//! bringing trades.

use futures_util::{SinkExt, StreamExt};
use garnet_feed::{
    backoff, classify, subscribe_frame, ws::Disconnect, Feed, FeedClock, FrameKind,
    RTDS_STALL_TIMEOUT,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// Brings up a server, sends `frames`, then closes the connection.
/// Returns the address and a list of what the client sent.
async fn server(frames: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

        // Wait for the subscription.
        if let Some(Ok(Message::Text(t))) = ws.next().await {
            sink.lock().unwrap().push(t);
        }
        for f in frames {
            ws.send(Message::Text(f)).await.unwrap();
        }
        // A short pause, to catch anything the client writes on its own.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(Some(Ok(msg))) =
            tokio::time::timeout(Duration::from_millis(50), ws.next()).await
        {
            if let Message::Text(t) = msg {
                sink.lock().unwrap().push(t);
            }
        }
        let _ = ws.close(None).await;
    });

    (format!("ws://{addr}"), received)
}

#[tokio::test]
async fn the_subscribe_frame_is_the_only_thing_we_write() {
    let (url, received) = server(vec![
        r#"{"topic":"activity","type":"trades","payload":{}}"#.into(),
        r#"{"topic":"activity","type":"trades","payload":{}}"#.into(),
    ])
    .await;

    let mut seen = 0;
    let (reason, delivered) = Feed::new(url).run_once(|_| seen += 1).await.unwrap();

    assert_eq!(reason, Disconnect::Closed);
    assert_eq!(delivered, 2);
    assert_eq!(seen, 2, "every frame reaches the handler");

    let written = received.lock().unwrap().clone();
    assert_eq!(
        written.len(),
        1,
        "more than one message went into the socket: {written:?}"
    );
    assert_eq!(written[0], subscribe_frame());
}

#[tokio::test]
async fn the_subscription_asks_for_activity_trades() {
    let f: serde_json::Value = serde_json::from_str(&subscribe_frame()).unwrap();
    assert_eq!(f["action"], "subscribe");
    assert_eq!(f["subscriptions"][0]["topic"], "activity");
    assert_eq!(f["subscriptions"][0]["type"], "trades");
}

#[tokio::test]
async fn a_closing_peer_ends_the_connection_cleanly() {
    let (url, _) = server(vec![]).await;
    let (reason, delivered) = Feed::new(url).run_once(|_| {}).await.unwrap();
    assert_eq!(reason, Disconnect::Closed);
    assert_eq!(delivered, 0);
}

#[test]
fn backoff_is_capped_and_jittered() {
    for attempt in 0..8_u32 {
        let ceiling = Duration::from_secs((1_u64 << attempt).min(60));
        for _ in 0..50 {
            assert!(backoff(attempt) <= ceiling, "attempt {attempt}");
        }
    }
    // There is jitter: otherwise after a platform-wide outage every client returns at
    // once.
    let draws: Vec<Duration> = (0..50).map(|_| backoff(5)).collect();
    assert!(draws.iter().any(|d| *d != draws[0]));
}

#[test]
fn the_stall_timeout_stays_well_under_the_killswitch_window() {
    // A reconnection must happen before the killswitch fires on a stalled feed, or we
    // will be halting trading instead of reconnecting.
    assert!(RTDS_STALL_TIMEOUT < Duration::from_secs(300));
    assert!(garnet_feed::ws::is_flowing(Duration::from_secs(10)));
    assert!(!garnet_feed::ws::is_flowing(Duration::from_secs(60)));
}

// ---------------------------------------------------------------------------
// The control topic
// ---------------------------------------------------------------------------
//
// The predecessor subscribed only to `activity/trades` and measured health by a single number: the
// age of the last frame. On 31.08.2026 the topic went down platform-wide, and there was
// nothing to tell "the leaders are quiet" from "we have gone deaf": both states look
// like silence. The control topic is a second pulse, beating regardless of whether
// anyone is trading.

#[test]
fn the_subscription_asks_for_the_control_topic_with_its_type() {
    // A subscription without `type` is rejected in full: RTDS's live response on
    // 04.09.2026 was `{"message": "Invalid request body"}`, and the trades fall away
    // along with the control topic. The control topic's type is `update` (measured
    // 31.08).
    let f: serde_json::Value = serde_json::from_str(&subscribe_frame()).unwrap();
    let subs: Vec<(&str, &str)> = f["subscriptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["topic"].as_str().unwrap(),
                s["type"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert!(subs.contains(&("activity", "trades")), "{subs:?}");
    assert!(subs.contains(&("crypto_prices", "update")), "{subs:?}");
}

#[test]
fn a_fresh_clock_counts_from_start_not_from_never() {
    // The first health tick arrives before the first frame. If a clock with no frames
    // answers "never", a just-started process declares the feed dead and hits the
    // killswitch — which is exactly what happened on the first live run.
    let clock = FeedClock::new();
    assert!(
        clock.since_last_frame() < Duration::from_secs(5),
        "a fresh clock counts from the start of the process"
    );
    assert!(clock.since_last_trade() < Duration::from_secs(5));
    assert!(
        clock.last_trade_at().is_none(),
        "while there were no trades at all"
    );
}

#[test]
fn frames_are_classified_by_topic() {
    assert_eq!(
        classify(r#"{"topic":"activity","type":"trades","payload":{}}"#),
        FrameKind::Trade
    );
    assert_eq!(
        classify(r#"{"topic":"crypto_prices","payload":{"symbol":"btcusdt"}}"#),
        FrameKind::Control
    );
    assert_eq!(classify("{}"), FrameKind::Other);
    assert_eq!(classify("not json"), FrameKind::Other);
}

#[tokio::test]
async fn a_control_frame_proves_we_are_alive_but_is_not_a_trade() {
    // A control frame does not go to the detector: it says nothing about the leaders.
    // But it proves the socket is delivering — and that is its whole point.
    let (url, _) = server(vec![
        r#"{"topic":"crypto_prices","payload":{"symbol":"btcusdt","value":"110000"}}"#.into(),
        r#"{"topic":"activity","type":"trades","payload":{}}"#.into(),
    ])
    .await;

    let clock = FeedClock::new();
    let mut seen = 0;
    let (_, delivered) = Feed::new(url)
        .with_clock(clock.clone())
        .run_once(|_| seen += 1)
        .await
        .unwrap();

    assert_eq!(delivered, 2, "both frames were delivered");
    assert_eq!(seen, 1, "only the trading one went to the detector");
    assert!(clock.since_last_trade() < Duration::from_secs(5));
    assert!(clock.since_last_frame() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_socket_that_only_carries_control_frames_has_no_trades() {
    // Exactly the state of 31.08: the socket is alive, there are no trades. Only two
    // clocks can tell this apart, and here they are.
    let (url, _) = server(vec![
        r#"{"topic":"crypto_prices","payload":{"symbol":"btcusdt"}}"#.into(),
    ])
    .await;

    let clock = FeedClock::new();
    Feed::new(url)
        .with_clock(clock.clone())
        .run_once(|_| {})
        .await
        .unwrap();

    assert!(
        clock.since_last_frame() < Duration::from_secs(5),
        "the socket is delivering"
    );
    assert!(
        clock.last_trade_at().is_none(),
        "not a single trade: this is not a dead feed but quiet leaders"
    );
}
