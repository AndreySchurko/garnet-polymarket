//! Trading-path events on NATS.
//!
//! The bus is not a precondition of trading. An unreachable NATS must cost us events, not
//! trades: a dependency of that kind dropping out stopped the whole circuit in the
//! predecessor. So there is not one path here that returns an error outwards: a failed
//! publish is visible in the log and ends there.
//!
//! The only producer of events is the trading path. The readers (Telegram, the dashboard)
//! arrive separately and do not affect this side.

use garnet_bus::Bus;

#[derive(Clone)]
pub struct Events {
    bus: Option<Bus>,
}

impl Events {
    /// Without a bus: events go nowhere, trading carries on as usual.
    #[must_use]
    pub fn off() -> Self {
        Self { bus: None }
    }

    /// # Errors
    ///
    /// NATS is unreachable. The caller decides whether to fail: the binary continues without
    /// a bus, while a bus test fails.
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        Self::connect_in(url, "").await
    }

    /// Connect within a subject namespace of your own.
    ///
    /// An empty string means the production namespace. Tests need their own: on 05.09.2026
    /// they published to the same bus `garnet-tg` listens on, and the operator received
    /// fixtures in the chat, one message per run.
    ///
    /// # Errors
    ///
    /// NATS is unreachable.
    pub async fn connect_in(url: &str, namespace: &str) -> anyhow::Result<Self> {
        Ok(Self {
            bus: Some(Bus::connect_in(url, namespace).await?),
        })
    }

    /// Publish an event. A publish failure does not interrupt trading.
    pub async fn emit(&self, subject: &str, payload: &serde_json::Value) {
        let Some(bus) = &self.bus else { return };
        if let Err(e) = bus.publish(subject, payload).await {
            eprintln!("the event {subject} was not published: {e}");
        }
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.bus.is_some()
    }
}
