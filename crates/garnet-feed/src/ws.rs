//! The RTDS connection loop.
//!
//! # Why we write nothing to this socket
//!
//! Measured in the predecessor on 2026-08-11: three connections to one endpoint, one
//! host, one hour, differing only in keepalive.
//!
//! | what the client sends       | trades  | stalls | did they all close? |
//! |----------------------------|---------|-----------|----------------|
//! | a text `ping` every 5 s     |  74,365 | 1      | yes, after 39 min |
//! | a WebSocket Ping every 15 s | 179,627 | 2      | no, one hung for >200 s |
//! | nothing                     | 185,477 | 6      | yes, never silent for >117 s |
//!
//! The difference is not in the frequency of stalls — the silent connection stalled
//! most often — but in **how long delivery stays down and whether the socket admits
//! that it died**. A keepalive buys rare stalls lasting tens of minutes; silence gives
//! frequent ones, each ending in a close within two minutes. The bottom line by volume
//! delivered is 2.5x in favour of silence.
//!
//! The only thing we write besides the subscription frame is a `Pong` in reply to a
//! `Ping`: refusing to answer a Ping is treated by the protocol as grounds to close the
//! connection. Across every capture, RTDS never sent one.

use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// The RTDS endpoint.
pub const RTDS_URL: &str = "wss://ws-live-data.polymarket.com";

/// The subscription frame: trades and the control topic.
///
/// The second topic is not about trading. It exists to tell "the leaders are quiet"
/// from "we have gone deaf": both states look like silence in `activity/trades`, and in
/// the predecessor there was nothing to distinguish them. On 31.08.2026 the trades
/// topic went down platform-wide, and it was `crypto_prices` that proved, under manual
/// diagnosis, that the socket was alive.
///
/// `type` is mandatory on the control topic: a subscription without it is rejected
/// **in full** — a live response on 04.09.2026 of `{"message": "Invalid request body"}` —
/// and the trades fall away along with the control topic.
#[must_use]
pub fn subscribe_frame() -> String {
    serde_json::json!({
        "action": "subscribe",
        "subscriptions": [
            { "topic": "activity", "type": "trades" },
            { "topic": "crypto_prices", "type": "update" }
        ]
    })
    .to_string()
}

/// What kind of frame arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Leader trades — the only thing that goes to the detector.
    Trade,
    /// The control topic: it proves delivery and says nothing about trading.
    Control,
    /// Everything else: subscription acknowledgements, housekeeping frames.
    Other,
}

/// Classifies a frame by its topic.
///
/// By substring rather than by parsing: frames arrive dozens per second, and a needless
/// JSON parse on the receiving side is latency on the signal path. The full parse is
/// done by the worker, and only for trading frames.
#[must_use]
pub fn classify(raw: &str) -> FrameKind {
    if raw.contains(r#""topic":"crypto_prices""#) {
        FrameKind::Control
    } else if raw.contains(r#""topic":"activity""#) && raw.contains(r#""type":"trades""#) {
        FrameKind::Trade
    } else {
        FrameKind::Other
    }
}

/// The feed's two clocks.
///
/// `last_frame` — any frame: proof that the socket is delivering.
/// `last_trade` — leader trades only. Health is computed from both: silent trades with a
/// live control topic mean a quiet market, while silence on both means we have gone
/// deaf.
#[derive(Clone, Default)]
pub struct FeedClock {
    inner: std::sync::Arc<ClockInner>,
}

struct ClockInner {
    /// The moment the clock was created. Until any frame arrives, age is measured from
    /// it: otherwise a just-started process declares the feed dead without having
    /// received anything, and hits the killswitch at boot.
    started_at: tokio::time::Instant,
    last_frame: std::sync::Mutex<Option<tokio::time::Instant>>,
    last_trade: std::sync::Mutex<Option<tokio::time::Instant>>,
}

impl Default for ClockInner {
    fn default() -> Self {
        Self {
            started_at: tokio::time::Instant::now(),
            last_frame: std::sync::Mutex::new(None),
            last_trade: std::sync::Mutex::new(None),
        }
    }
}

impl FeedClock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn mark(&self, kind: FrameKind) {
        let now = tokio::time::Instant::now();
        *self.inner.last_frame.lock().unwrap() = Some(now);
        if kind == FrameKind::Trade {
            *self.inner.last_trade.lock().unwrap() = Some(now);
        }
    }

    /// The age of the last frame on any topic.
    #[must_use]
    pub fn since_last_frame(&self) -> Duration {
        self.age(*self.inner.last_frame.lock().unwrap())
    }

    /// The age of the last leader trade.
    #[must_use]
    pub fn since_last_trade(&self) -> Duration {
        self.age(*self.inner.last_trade.lock().unwrap())
    }

    /// A missing mark is not "never" but "since startup": a young process must count as
    /// healthy until the silence threshold has elapsed.
    fn age(&self, mark: Option<tokio::time::Instant>) -> Duration {
        mark.unwrap_or(self.inner.started_at).elapsed()
    }

    #[must_use]
    pub fn last_trade_at(&self) -> Option<tokio::time::Instant> {
        *self.inner.last_trade.lock().unwrap()
    }
}

/// How often we check for silence.
const STALL_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// We reconnect if the socket has been silent for longer than this.
///
/// A dead RTDS subscription does not look like a dead socket: the peer stops delivering,
/// the connection stays open and error-free, and `next()` simply never wakes up. This
/// watchdog is the only thing that notices.
///
/// 45 seconds, not 120: a stall here is chronic rather than exceptional — the predecessor's
/// production accumulated 52 reconnections a day, and at a two-minute threshold that is
/// about 17% of the day spent reading a feed that is not arriving. A copy made against
/// a stale book is worse than a copy not placed, and a reconnection costs less than a
/// second.
pub const RTDS_STALL_TIMEOUT: Duration = Duration::from_secs(45);

/// The delay before reconnecting: doubling with random jitter.
///
/// The jitter exists so that after a platform-wide outage every client does not come
/// rushing back at the same instant.
#[must_use]
pub fn backoff(attempt: u32) -> Duration {
    garnet_clob::backoff::full_jitter(attempt, Duration::from_secs(1), Duration::from_secs(60))
}

/// Why the connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disconnect {
    /// The peer closed the connection.
    Closed,
    /// Silence for longer than [`RTDS_STALL_TIMEOUT`].
    Stalled,
}

pub struct Feed {
    url: String,
    clock: FeedClock,
}

impl Feed {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            clock: FeedClock::new(),
        }
    }

    /// Its own clock: the health loop, which lives outside the feed, reads it.
    #[must_use]
    pub fn with_clock(mut self, clock: FeedClock) -> Self {
        self.clock = clock;
        self
    }

    #[must_use]
    pub fn clock(&self) -> FeedClock {
        self.clock.clone()
    }

    /// One connection: subscribe and read until it is closed or falls silent.
    ///
    /// Returns the reason and the number of frames delivered. The callback is invoked for
    /// every text frame; parsing is not our concern.
    pub async fn run_once<F>(&self, mut on_frame: F) -> anyhow::Result<(Disconnect, u64)>
    where
        F: FnMut(&str),
    {
        let (mut ws, _) = connect_async(&self.url).await?;
        ws.send(Message::Text(subscribe_frame())).await?;

        let mut delivered: u64 = 0;
        let mut last_data = tokio::time::Instant::now();
        let mut ticker = tokio::time::interval(STALL_CHECK_INTERVAL);
        ticker.tick().await; // the first tick arrives immediately

        loop {
            tokio::select! {
                msg = ws.next() => {
                    let Some(msg) = msg else { return Ok((Disconnect::Closed, delivered)) };
                    match msg? {
                        Message::Text(text) => {
                            last_data = tokio::time::Instant::now();
                            delivered = delivered.saturating_add(1);
                            let kind = classify(&text);
                            self.clock.mark(kind);
                            // Only trades go to the detector: a control frame
                            // proves delivery and means nothing more.
                            if kind == FrameKind::Trade {
                                on_frame(&text);
                            }
                        }
                        // The only write besides the subscription — and only at
                        // the peer's request.
                        Message::Ping(p) => ws.send(Message::Pong(p)).await?,
                        Message::Close(_) => return Ok((Disconnect::Closed, delivered)),
                        _ => {}
                    }
                }
                _ = ticker.tick() => {
                    if last_data.elapsed() >= RTDS_STALL_TIMEOUT {
                        return Ok((Disconnect::Stalled, delivered));
                    }
                }
            }
        }
    }

    /// An endless loop with reconnection.
    ///
    /// A successful connection that delivered at least one frame resets the attempt
    /// counter: otherwise a long healthy session would leave us on a minute-long pause
    /// after the very first break.
    pub async fn run_forever<F>(&self, mut on_frame: F) -> !
    where
        F: FnMut(&str),
    {
        let mut attempt = 0_u32;
        loop {
            match self.run_once(&mut on_frame).await {
                Ok((reason, delivered)) => {
                    tracing::warn!(?reason, delivered, "the RTDS connection ended");
                    if delivered > 0 {
                        attempt = 0;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "the RTDS connection was not established"),
            }
            tokio::time::sleep(backoff(attempt)).await;
            attempt = attempt.saturating_add(1);
        }
    }
}

/// The feed's freshness, for health.
///
/// Health is measured by flow, not by whether the process is alive: in the
/// predecessor both guards reported OK while the bot was blind for 6.5 hours.
#[must_use]
pub fn is_flowing(since_last_frame: Duration) -> bool {
    since_last_frame < RTDS_STALL_TIMEOUT
}
