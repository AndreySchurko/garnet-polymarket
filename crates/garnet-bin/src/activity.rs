//! The safety-net detection circuit: `/activity?user=`.
//!
//! On 31.08.2026 the RTDS `activity/trades` topic went silent platform-wide for hours, from
//! every IP at once — while REST kept returning the same trades throughout. One delivery
//! circuit means such an outage leaves the bot blind; two circuits write to one table, and
//! the dedup decides whose copy arrived first.

use std::time::Duration;

pub struct Activity {
    http: reqwest::Client,
    host: String,
}

impl Activity {
    /// # Errors
    ///
    /// The HTTP client could not be built.
    pub fn new(host: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("garnet/2.0")
                .timeout(Duration::from_secs(10))
                .build()?,
            host: host.into(),
        })
    }

    /// A wallet's latest actions.
    ///
    /// It returns more than trades: redemptions and splits come in the same list, and the
    /// parser sifts them out.
    ///
    /// # Errors
    ///
    /// A network failure or an unreadable response.
    pub async fn recent(&self, wallet: &str, limit: u32) -> anyhow::Result<serde_json::Value> {
        let url = format!("{}/activity?user={wallet}&limit={limit}", self.host);
        Ok(self.http.get(url).send().await?.json().await?)
    }
}
