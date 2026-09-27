//! The RTDS socket.
//!
//! Frame parsing lives in `garnet-core::detect`; here there is only the connection,
//! the subscription, reconnection and silence detection.

pub mod chain;
pub mod ws;

pub use ws::{
    backoff, classify, subscribe_frame, Feed, FeedClock, FrameKind, RTDS_STALL_TIMEOUT, RTDS_URL,
};
