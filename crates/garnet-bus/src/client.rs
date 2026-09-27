//! Thin wrapper around `async-nats` providing typed publish/subscribe.

use async_nats::{Client, Subscriber};
use bytes::Bytes;
use serde::Serialize;

use crate::error::BusError;

/// NATS bus client.
///
/// Cloning is cheap — the inner `async_nats::Client` uses an `Arc` internally.
///
/// # Examples
///
/// ```no_run
/// # async fn run() -> Result<(), garnet_bus::BusError> {
/// let bus = garnet_bus::Bus::connect("nats://127.0.0.1:4223").await?;
/// bus.publish("garnet.trade.leader", &serde_json::json!({"wallet": "0xabc"})).await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct Bus {
    client: Client,
    /// The subject namespace. Empty means production.
    ///
    /// On 05.09.2026 the tests published to the same bus that `garnet-tg`
    /// listens on, and the operator received fixtures in the chat: "Resolved ·
    /// LIVE · 0xsettled", one message per test run. The same mistake as tests
    /// against the production database, and it is cured the same way — with a
    /// namespace of one's own, not with care.
    namespace: String,
}

impl Bus {
    /// Connect to a NATS server.
    ///
    /// # Errors
    ///
    /// Returns [`BusError::Nats`] if the connection cannot be established.
    pub async fn connect(url: &str) -> Result<Self, BusError> {
        Self::connect_in(url, "").await
    }

    /// Connect within a namespace of your own.
    ///
    /// An empty string means the production namespace and leaves subjects
    /// untouched: otherwise a production producer and a production subscriber
    /// would drift apart silently.
    ///
    /// # Errors
    ///
    /// Returns [`BusError::Nats`] if the connection cannot be established.
    pub async fn connect_in(url: &str, namespace: &str) -> Result<Self, BusError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|e| BusError::Nats(Box::new(e)))?;
        Ok(Self {
            client,
            namespace: namespace.to_string(),
        })
    }

    /// The subject, with the namespace applied.
    fn scoped(&self, subject: impl Into<String>) -> String {
        let subject = subject.into();
        if self.namespace.is_empty() {
            subject
        } else {
            format!("{}.{subject}", self.namespace)
        }
    }

    /// Publish a JSON-serialised message to `subject`.
    ///
    /// # Errors
    ///
    /// Returns [`BusError::Serialise`] if `payload` cannot be serialised, or
    /// [`BusError::Nats`] if the publish fails.
    pub async fn publish<T: Serialize>(
        &self,
        subject: impl Into<String>,
        payload: &T,
    ) -> Result<(), BusError> {
        let bytes = serde_json::to_vec(payload)?;
        self.client
            .publish(self.scoped(subject), Bytes::from(bytes))
            .await
            .map_err(|e| BusError::Nats(Box::new(e)))?;
        Ok(())
    }

    /// Subscribe to `subject`, returning an async stream of messages.
    ///
    /// # Errors
    ///
    /// Returns [`BusError::Nats`] if the subscription cannot be created.
    pub async fn subscribe(&self, subject: impl Into<String>) -> Result<Subscriber, BusError> {
        let sub = self
            .client
            .subscribe(self.scoped(subject))
            .await
            .map_err(|e| BusError::Nats(Box::new(e)))?;
        Ok(sub)
    }

    /// Flush any buffered messages to the NATS server.
    ///
    /// # Errors
    ///
    /// Returns [`BusError::Nats`] on flush failure.
    pub async fn flush(&self) -> Result<(), BusError> {
        self.client
            .flush()
            .await
            .map_err(|e| BusError::Nats(Box::new(e)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_error_display() {
        let e = BusError::NotConnected;
        assert!(!e.to_string().is_empty());
    }
}
