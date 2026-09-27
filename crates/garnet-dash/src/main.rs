//! `garnet-dash` — a Telegram mini-app over the same database.
//!
//! It listens on **loopback only**: Caddy faces outwards and terminates TLS.
//! A separate process for the same reason as the bot: a reader must not fall over
//! together with trading, nor bring it down itself.

use garnet_config::Config;
use garnet_dash::api::{router, Ctx};
use garnet_db::Db;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map_or("config.toml", String::as_str);
    let cfg = Config::load(path)?;

    // The same token as the bot's: the `initData` signature is derived from it, and a
    // different token would mean the dashboard accepts passes from someone else's bot.
    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").map_err(|_| {
        anyhow::anyhow!("no TELEGRAM_BOT_TOKEN: without it the signature cannot be verified")
    })?;

    let owners = garnet_dash::owners(
        &cfg.telegram.allowed_chat_ids,
        std::env::var("TELEGRAM_OWNER_USER_IDS").ok().as_deref(),
    );
    if owners.is_empty() {
        // The same rule as the bot's: an empty list means "nobody", not "everybody".
        eprintln!("the owner list is empty: the dashboard will let nobody in");
    }

    let db = Db::connect(&cfg.database_url).await?;
    let ctx = Arc::new(Ctx {
        db,
        bot_token,
        owners,
        max_age_secs: cfg.dashboard.init_data_max_age_secs,
    });

    let addr = cfg.dashboard.bind.clone();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("the dashboard is listening on {addr}");

    axum::serve(listener, router(ctx))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            println!("shutting down");
        })
        .await?;
    Ok(())
}
