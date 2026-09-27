//! Error types for the NATS bus layer.

use thiserror::Error;

/// Errors that can occur during NATS bus operations.
#[derive(Debug, Error)]
pub enum BusError {
    /// NATS connection or I/O error.
    #[error("NATS error: {0}")]
    Nats(#[from] async_nats::Error),

    /// Failed to serialise a message payload.
    #[error("serialisation error: {0}")]
    Serialise(#[from] serde_json::Error),

    /// The bus client has been disconnected or dropped.
    #[error("bus client is not connected")]
    NotConnected,
}
