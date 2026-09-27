//! The Bot API client: four methods and not one dependency beyond `reqwest`.
//!
//! The token sits in the **URL of every request**, so not one `reqwest` error leaves here
//! in its original form: its text contains the whole address, and a log with it ends up
//! in a ticket later. Everything we show passes through
//! [`hide`].

use serde_json::json;

pub struct Api {
    http: reqwest::Client,
    /// `<host>/bot<token>` — the secret is inside.
    base: String,
    token: String,
}

impl Api {
    /// # Errors
    ///
    /// The HTTP client could not be built.
    pub fn new(host: impl Into<String>, token: impl Into<String>) -> anyhow::Result<Self> {
        let token = token.into();
        Ok(Self {
            http: reqwest::Client::builder()
                // Longer than the long poll: otherwise the client tears down the very
                // request it asked to be held open.
                .timeout(std::time::Duration::from_secs(90))
                .build()?,
            base: format!("{}/bot{token}", host.into()),
            token,
        })
    }

    /// Strip the token out of any text a human will see.
    fn hide(&self, e: &impl std::fmt::Display) -> anyhow::Error {
        anyhow::anyhow!(e.to_string().replace(&self.token, "***"))
    }

    async fn call(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(body)
            .send()
            .await
            .map_err(|e| self.hide(&e))?;
        let v: serde_json::Value = resp.json().await.map_err(|e| self.hide(&e))?;
        if v["ok"].as_bool() == Some(false) {
            // The error description is written by Telegram and contains no token — but we
            // clean it too: a rule is cheaper than an exception to a rule.
            anyhow::bail!(
                "{method}: {}",
                v["description"]
                    .as_str()
                    .unwrap_or("refused")
                    .replace(&self.token, "***")
            );
        }
        Ok(v)
    }

    /// Fetch updates starting from `offset`.
    ///
    /// # Errors
    ///
    /// The network, or a refusal from Telegram.
    pub async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let v = self
            .call(
                "getUpdates",
                &json!({ "offset": offset, "timeout": timeout_secs }),
            )
            .await?;
        Ok(v["result"].as_array().cloned().unwrap_or_default())
    }

    /// # Errors
    ///
    /// The network, or a refusal from Telegram.
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        buttons: &[Vec<(String, String)>],
    ) -> anyhow::Result<()> {
        let mut body = json!({ "chat_id": chat_id, "text": text });
        if !buttons.is_empty() {
            body["reply_markup"] = json!({ "inline_keyboard": keyboard(buttons) });
        }
        self.call("sendMessage", &body).await?;
        Ok(())
    }

    /// # Errors
    ///
    /// The network, or a refusal from Telegram.
    pub async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> anyhow::Result<()> {
        self.call(
            "editMessageText",
            &json!({ "chat_id": chat_id, "message_id": message_id, "text": text }),
        )
        .await?;
        Ok(())
    }

    /// The reply to a button press. Without it the client shows a spinner until it times
    /// out.
    ///
    /// # Errors
    ///
    /// The network, or a refusal from Telegram.
    pub async fn answer_callback(&self, query_id: &str) -> anyhow::Result<()> {
        self.call(
            "answerCallbackQuery",
            &json!({ "callback_query_id": query_id }),
        )
        .await?;
        Ok(())
    }

    /// The command menu: what the client shows for "/".
    ///
    /// Without it only somebody who read the spec knows the list of commands. Set once at
    /// startup — Telegram remembers it on its own side.
    ///
    /// # Errors
    ///
    /// The network, or a refusal from Telegram.
    pub async fn set_commands(&self, commands: &[(&str, &str)]) -> anyhow::Result<()> {
        let list: Vec<serde_json::Value> = commands
            .iter()
            // Telegram accepts the name without a leading slash and in lower case only.
            .map(|(name, about)| json!({ "command": name.trim_start_matches('/'), "description": about }))
            .collect();
        self.call("setMyCommands", &json!({ "commands": list }))
            .await?;
        Ok(())
    }
}

fn keyboard(buttons: &[Vec<(String, String)>]) -> serde_json::Value {
    json!(buttons
        .iter()
        .map(|row| row
            .iter()
            .map(|(label, data)| match data.strip_prefix("webapp:") {
                // A mini-app opens only from a button like this: an ordinary link goes to the
                // browser, and the browser does not provide `initData`.
                Some(url) => json!({ "text": label, "web_app": { "url": url } }),
                None => json!({ "text": label, "callback_data": data }),
            })
            .collect::<Vec<_>>())
        .collect::<Vec<_>>())
}
