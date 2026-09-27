//! Checking the bus against the live NATS from docker-compose.
//!
//! The test skips itself if NATS is not up: it checks delivery, not the presence
//! of infrastructure.

use garnet_bus::{subjects, Bus};
use tokio_stream::StreamExt;

const URL: &str = "nats://127.0.0.1:4223";

/// A namespace of our own: the production bus is what `garnet-tg` listens on, and
/// a fixture published by a test ends up in the operator's chat.
fn namespace() -> String {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("t_bus_{ns}")
}

async fn bus_or_skip() -> Option<Bus> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        Bus::connect_in(URL, &namespace()),
    )
    .await
    {
        Ok(Ok(b)) => Some(b),
        _ => {
            eprintln!("NATS unreachable at {URL}: test skipped");
            None
        }
    }
}

#[tokio::test]
async fn event_published_is_event_received() {
    let Some(bus) = bus_or_skip().await else {
        return;
    };

    let mut sub = bus.subscribe(subjects::ORDER_FILLED).await.unwrap();
    bus.publish(
        subjects::ORDER_FILLED,
        &serde_json::json!({ "token_id": "tok_lal", "size": "59.5", "mode": "live" }),
    )
    .await
    .unwrap();
    bus.flush().await.unwrap();

    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), sub.next())
        .await
        .expect("no message within 3 s")
        .expect("the subscription closed");

    let got: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap();
    assert_eq!(got["token_id"], "tok_lal");
    assert_eq!(
        got["size"], "59.5",
        "Decimal travels as a string, not as f64"
    );
}

#[tokio::test]
async fn subjects_are_namespaced_by_kind() {
    // An event and an alert differ by prefix: a subscriber to alerts is not
    // obliged to parse the execution stream.
    assert!(subjects::ALERT_KILLSWITCH.starts_with("alert."));
    assert!(subjects::ALERT_RECONCILE_DIVERGENCE.starts_with("alert."));
    assert!(!subjects::ORDER_FILLED.starts_with("alert."));
    assert_eq!(subjects::SIGNAL_DETECTED, "signal.detected");
}

#[tokio::test]
async fn a_test_namespace_is_invisible_to_the_running_bot() {
    // 05.09.2026: the tests published to the same bus that `garnet-tg` listens
    // on, and the operator received fixtures in the chat — "Resolved · LIVE ·
    // 0xsettled", one message per run. The same mistake as tests against the
    // production database.
    let Ok(Ok(bare)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), Bus::connect(URL)).await
    else {
        eprintln!("NATS unreachable at {URL}: test skipped");
        return;
    };
    let scoped = Bus::connect_in(URL, &namespace()).await.unwrap();

    let mut heard_bare = bare.subscribe(subjects::POSITION_SETTLED).await.unwrap();
    let mut heard_scoped = scoped.subscribe(subjects::POSITION_SETTLED).await.unwrap();
    // A subscription is registered on the server asynchronously, and without this
    // "nobody heard it" would mean "we did not manage to subscribe in time"
    // rather than isolation.
    bare.flush().await.unwrap();
    scoped.flush().await.unwrap();

    scoped
        .publish(
            subjects::POSITION_SETTLED,
            &serde_json::json!({ "wallet": "0xsettled" }),
        )
        .await
        .unwrap();
    scoped.flush().await.unwrap();
    bare.flush().await.unwrap();

    let mine = tokio::time::timeout(std::time::Duration::from_secs(3), heard_scoped.next())
        .await
        .expect("our own namespace is obliged to deliver")
        .expect("the subscription closed");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&mine.payload).unwrap()["wallet"],
        "0xsettled"
    );

    // The production subscriber must hear nothing at all.
    let leaked =
        tokio::time::timeout(std::time::Duration::from_millis(500), heard_bare.next()).await;
    assert!(
        leaked.is_err(),
        "the test event leaked into the production bus"
    );
}

#[tokio::test]
async fn an_empty_namespace_leaves_the_subject_alone() {
    // An empty namespace must leave the subject untouched: otherwise a production
    // producer and a production subscriber would drift apart silently. Checked on
    // a probe subject and **not** on a production one: publishing to `alert.*`
    // would reach the operator's chat — precisely the defect these tests close.
    let Ok(Ok(a)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), Bus::connect_in(URL, "")).await
    else {
        eprintln!("NATS unreachable at {URL}: test skipped");
        return;
    };
    let b = Bus::connect_in(URL, "").await.unwrap();
    let probe = format!("t_probe.{}", namespace());

    let mut sub = a.subscribe(probe.clone()).await.unwrap();
    a.flush().await.unwrap(); // subscription on another connection: wait for the server
    b.publish(probe, &serde_json::json!({ "text": "probe" }))
        .await
        .unwrap();
    b.flush().await.unwrap();

    // A known flake, undiagnosed: the message fails to arrive in roughly one run
    // out of five. Waiting does not cure it — twenty seconds helped exactly as
    // much as three — so the timeout is left short rather than raised just in
    // case: a long timeout differs from a short one only by staying silent
    // longer. See docs/ARCHITECTURE.md.
    let msg = tokio::time::timeout(std::time::Duration::from_secs(3), sub.next())
        .await
        .expect("an empty namespace changes nothing")
        .expect("the subscription closed");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&msg.payload).unwrap()["text"],
        "probe"
    );
}
