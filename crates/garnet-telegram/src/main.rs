//! `garnet-tg` — the operator's controls as a separate process.
//!
//! Separate on purpose: a hung long poll or a ban on the bot must not live in the same
//! process as trade execution. It shares exactly two things with trading — the database
//! (the wallet registry and the manual stop) and the bus (pushes).

use garnet_config::Config;
use garnet_db::Db;
use garnet_telegram::api::Api;
use garnet_telegram::poll::poll_once;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map_or("config.toml", String::as_str);
    let cfg = Config::load(path)?;

    // The mini-app's address: without it `/app` says honestly that the dashboard is not
    // configured.
    garnet_telegram::set_dashboard_url(cfg.dashboard.public_url.clone());
    garnet_telegram::set_flatten_name(cfg.flatten.name.clone());

    let db = Db::connect(&cfg.database_url).await?;

    // A command's reply printed to the console. It exists precisely so that what the bot
    // will say can be checked without occupying the chat and without depending on
    // Telegram being reachable: `garnet-tg --say "/pnl all"`.
    if let Some(i) = args.iter().position(|a| a == "--say") {
        let text = args.get(i + 1).cloned().unwrap_or_else(|| "/help".into());
        // Not a command means a button press: confirmations (the stake, the switch to
        // live, /kill) cannot otherwise be carried through from the console.
        let incoming = if text.starts_with('/') {
            garnet_telegram::update::Incoming::Message { chat_id: 0, text }
        } else {
            garnet_telegram::update::Incoming::Button {
                chat_id: 0,
                message_id: 0,
                query_id: String::new(),
                data: text,
            }
        };
        let reply = garnet_telegram::commands::handle(&db, &incoming).await?;
        match reply {
            Some(r) => {
                println!("{}", r.text);
                for row in &r.buttons {
                    let labels: Vec<&str> = row.iter().map(|(l, _)| l.as_str()).collect();
                    println!("[buttons: {}]", labels.join(" | "));
                }
            }
            None => println!("(the bot would have stayed silent)"),
        }
        return Ok(());
    }

    let token = std::env::var("TELEGRAM_BOT_TOKEN")
        .map_err(|_| anyhow::anyhow!("no TELEGRAM_BOT_TOKEN"))?;

    let chats = garnet_telegram::guard::owners(
        &cfg.telegram.allowed_chat_ids,
        std::env::var("TELEGRAM_OWNER_USER_IDS").ok().as_deref(),
    );

    // An empty allowlist is a working state, not an error: this is how the bot starts up
    // "mute" before the operator enters their chat id. But it has to be said out loud,
    // otherwise the silence looks like a breakage.
    if chats.is_empty() {
        eprintln!(
            "the allowlist is empty: the bot will answer nobody (see [telegram] in the config)"
        );
    }

    let api = std::sync::Arc::new(Api::new(cfg.telegram.api_host.clone(), token)?);
    println!("bot started, chats allowed: {}", chats.len());

    // The menu is set once at startup: Telegram remembers it on its own side, and a failure
    // here costs convenience but not function — commands are accepted without the client's
    // hint.
    if let Err(e) = api.set_commands(garnet_telegram::commands::MENU).await {
        eprintln!("the command menu was not updated: {e}");
    }

    // Pushes live in a task of their own: an unreachable NATS must not deprive the operator
    // of the controls, and an unreachable Telegram must not stop anything else.
    match garnet_bus::Bus::connect(&cfg.api.nats_url).await {
        Ok(bus) => {
            let api = std::sync::Arc::clone(&api);
            let db = db.clone();
            let chats = chats.clone();
            let fills = cfg.telegram.push_fills.clone();
            tokio::spawn(async move {
                if let Err(e) = garnet_telegram::push::listen(&bus, &api, &db, &chats, &fills).await
                {
                    eprintln!("the bus subscription was interrupted: {e}");
                }
            });
            println!(
                "pushes: {} (fills: {})",
                cfg.api.nats_url, cfg.telegram.push_fills
            );
        }
        Err(e) => {
            eprintln!("the bus is unreachable ({e}): there will be no pushes, commands still work")
        }
    }

    // The daily summary. An unparseable or empty hour means "there is no summary":
    // guessing the time of a message that is expected at a particular hour is not allowed.
    match garnet_telegram::summary::parse_at(&cfg.telegram.daily_summary_utc) {
        Some((h, m)) if !chats.is_empty() => {
            let api = std::sync::Arc::clone(&api);
            let db = db.clone();
            let chats = chats.clone();
            tokio::spawn(async move {
                garnet_telegram::summary::run(&api, &db, &chats, h, m).await;
            });
            println!("daily summary: {h:02}:{m:02} UTC");
        }
        Some(_) => {}
        None => println!("the daily summary is disabled"),
    }

    let mut offset = 0;
    loop {
        tokio::select! {
            r = poll_once(
                &api,
                &db,
                &chats,
                &mut offset,
                cfg.telegram.poll_timeout_secs,
            ) => {
                if let Err(e) = r {
                    // The network dropped, or Telegram refused. The pause keeps the
                    // requests from hammering for nothing; the offset is preserved, and
                    // the unread updates will wait for the connection to return.
                    eprintln!("poll: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("shutting down");
                return Ok(());
            }
        }
    }
}
