//! A Telegram update -> a command.
//!
//! We do not choose the shape of an update, and it is richer than we need: besides
//! messages there are edits, channel posts, poll answers. Anything that is neither a
//! message nor a button press is discarded quietly — panicking on an unknown update is not
//! allowed, or the poll loop stalls on the very first foreign event.

use garnet_types::Mode;
use rust_decimal::Decimal;
use std::str::FromStr as _;

/// What we react to at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Message {
        chat_id: i64,
        text: String,
    },
    /// An inline button press. `query_id` is mandatory for the reply: without it the client
    /// shows a spinner until it times out.
    Button {
        chat_id: i64,
        message_id: i64,
        query_id: String,
        data: String,
    },
}

/// The window the result is computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Day,
    Week,
    All,
}

impl Period {
    /// The period's boundary. `None` — over all time.
    #[must_use]
    pub fn since(self) -> Option<chrono::DateTime<chrono::Utc>> {
        let hours = match self {
            Period::Day => 24,
            Period::Week => 24 * 7,
            Period::All => return None,
        };
        Some(chrono::Utc::now() - chrono::Duration::hours(hours))
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Period::Day => "over 24 hours",
            Period::Week => "over the week",
            Period::All => "over all time",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Wallets,
    Add {
        address: String,
        nickname: Option<String>,
    },
    Wallet {
        address: String,
    },
    Mode {
        address: String,
        mode: Mode,
    },
    Stake {
        address: String,
        usd: Decimal,
    },
    Slippage {
        address: String,
        pct: Decimal,
    },
    /// `/slippage` with no arguments: the distribution of the price miss. A threshold moved
    /// by guesswork is moved in both directions equally often.
    SlippageProfile,
    Enable {
        address: String,
        enabled: bool,
    },
    Kill,
    Resume,
    Health,
    Balance,
    /// What we hold now. `all` adds the recently closed ones.
    Positions {
        all: bool,
    },
    /// The result over a period.
    Pnl {
        period: Period,
    },
    /// The quality of copying: our half against the leader's.
    Matchup {
        period: Period,
    },
    /// The race between delivery circuits: who brings trades and who duplicates them.
    Sources {
        period: Period,
    },
    /// The emergency close of positions. `None` — show the modes without doing anything.
    Flatten {
        mode: Option<String>,
    },
    /// The depth of the entry queue per wallet: `fire_limit` is set from it.
    FireRate,
    /// Latency per stage of the signal path.
    Latency {
        period: Period,
    },
    /// Plain text, not a command. It has meaning in exactly one place — the emergency
    /// close's confirmation phrase; everywhere else the bot stays silent, as it did before.
    Plain(String),
    /// The last `n` decisions.
    Signals {
        n: i64,
    },
    /// Open the dashboard mini-app.
    App,
    /// There is no scanner yet — the command exists to say so in words.
    Scan,
    Help,
    /// The command was recognised, the arguments were not. The text explains what is wrong.
    Malformed(String),
    /// The command was not recognised at all.
    Unknown(String),
    /// Not a command: plain text.
    None,
}

/// Parse an update. `None` — "none of our business".
#[must_use]
pub fn parse_update(update: &serde_json::Value) -> Option<Incoming> {
    if let Some(cb) = update.get("callback_query") {
        return Some(Incoming::Button {
            chat_id: cb["message"]["chat"]["id"].as_i64()?,
            message_id: cb["message"]["message_id"].as_i64()?,
            query_id: cb["id"].as_str()?.to_string(),
            data: cb["data"].as_str().unwrap_or_default().to_string(),
        });
    }
    let msg = update.get("message")?;
    Some(Incoming::Message {
        chat_id: msg["chat"]["id"].as_i64()?,
        text: msg["text"].as_str().unwrap_or_default().to_string(),
    })
}

/// Parse a message's text into a command.
#[must_use]
pub fn parse_command(text: &str) -> Command {
    let text = text.trim();
    if text.is_empty() {
        return Command::None;
    }
    if !text.starts_with('/') {
        return Command::Plain(text.to_string());
    }
    let mut parts = text.split_whitespace();
    let head = parts.next().unwrap_or_default();
    // In a group the client appends `@bot_name` to every command.
    let name = head.split('@').next().unwrap_or(head);
    let args: Vec<&str> = parts.collect();

    match name {
        "/wallets" | "/start" => Command::Wallets,
        "/help" => Command::Help,
        "/kill" => Command::Kill,
        "/resume" => Command::Resume,
        "/health" => Command::Health,
        "/balance" => Command::Balance,
        "/scan" => Command::Scan,
        "/app" => Command::App,
        "/positions" => match args.first().copied() {
            None | Some("open") => Command::Positions { all: false },
            Some("all") => Command::Positions { all: true },
            Some(other) => {
                Command::Malformed(format!("\"{other}\" is not it: /positions [open|all]"))
            }
        },
        "/pnl" => match args.first().copied() {
            None | Some("day") => Command::Pnl {
                period: Period::Day,
            },
            Some("week") => Command::Pnl {
                period: Period::Week,
            },
            Some("all") => Command::Pnl {
                period: Period::All,
            },
            Some(other) => {
                Command::Malformed(format!("\"{other}\" is not it: /pnl [day|week|all]"))
            }
        },
        "/firerate" => Command::FireRate,
        "/latency" => match args.first().copied() {
            None | Some("day") => Command::Latency {
                period: Period::Day,
            },
            Some("week") => Command::Latency {
                period: Period::Week,
            },
            Some("all") => Command::Latency {
                period: Period::All,
            },
            Some(other) => {
                Command::Malformed(format!("\"{other}\" is not it: /latency [day|week|all]"))
            }
        },
        "/flatten" => Command::Flatten {
            mode: args.first().map(|s| (*s).to_string()),
        },
        "/sources" => match args.first().copied() {
            None | Some("day") => Command::Sources {
                period: Period::Day,
            },
            Some("week") => Command::Sources {
                period: Period::Week,
            },
            Some("all") => Command::Sources {
                period: Period::All,
            },
            Some(other) => {
                Command::Malformed(format!("\"{other}\" is not it: /sources [day|week|all]"))
            }
        },
        "/matchup" => match args.first().copied() {
            None | Some("day") => Command::Matchup {
                period: Period::Day,
            },
            Some("week") => Command::Matchup {
                period: Period::Week,
            },
            Some("all") => Command::Matchup {
                period: Period::All,
            },
            Some(other) => {
                Command::Malformed(format!("\"{other}\" is not it: /matchup [day|week|all]"))
            }
        },
        "/signals" => match args.first() {
            None => Command::Signals { n: 10 },
            // The list is read by eye: a hundred lines in one message is unreadable, and
            // Telegram cuts it off at 4096 bytes anyway.
            Some(n) => match n.parse::<i64>() {
                Ok(n) if (1..=50).contains(&n) => Command::Signals { n },
                _ => Command::Malformed("how many rows? /signals [1-50]".into()),
            },
        },
        "/add" => match args.split_first() {
            None => Command::Malformed("an address is required: /add <address> [nickname]".into()),
            Some((addr, rest)) => match address(addr) {
                Err(e) => Command::Malformed(e),
                Ok(address) => Command::Add {
                    address,
                    nickname: Some(rest.join(" ")).filter(|s| !s.is_empty()),
                },
            },
        },
        "/wallet" => one_address(&args)
            .map_or_else(Command::Malformed, |address| Command::Wallet { address }),
        "/mode" => match (one_address(&args), args.get(1)) {
            (Err(e), _) => Command::Malformed(e),
            (Ok(_), None) => {
                Command::Malformed("a mode is required: /mode <address> live|shadow".into())
            }
            (Ok(address), Some(m)) => match *m {
                "live" => Command::Mode {
                    address,
                    mode: Mode::Live,
                },
                "shadow" => Command::Mode {
                    address,
                    mode: Mode::Shadow,
                },
                other => Command::Malformed(format!("the mode is live or shadow, not \"{other}\"")),
            },
        },
        "/stake" => match (one_address(&args), positive(args.get(1))) {
            (Err(e), _) | (Ok(_), Err(e)) => Command::Malformed(e),
            (Ok(address), Ok(usd)) => Command::Stake { address, usd },
        },
        "/slippage" if args.is_empty() => Command::SlippageProfile,
        "/slippage" => match (one_address(&args), positive(args.get(1))) {
            (Err(e), _) | (Ok(_), Err(e)) => Command::Malformed(e),
            (Ok(address), Ok(pct)) => Command::Slippage { address, pct },
        },
        "/on" => one_address(&args).map_or_else(Command::Malformed, |address| Command::Enable {
            address,
            enabled: true,
        }),
        "/off" => one_address(&args).map_or_else(Command::Malformed, |address| Command::Enable {
            address,
            enabled: false,
        }),
        other => Command::Unknown(other.to_string()),
    }
}

fn one_address(args: &[&str]) -> Result<String, String> {
    args.first()
        .ok_or_else(|| "a wallet address is required".to_string())
        .and_then(|a| address(a))
}

/// The address is lowercased: an RTDS frame brings it that way, while a human copies it
/// from an explorer in checksum form, and the comparison would miss.
fn address(raw: &str) -> Result<String, String> {
    let ok =
        raw.len() == 42 && raw.starts_with("0x") && raw[2..].chars().all(|c| c.is_ascii_hexdigit());
    if ok {
        Ok(raw.to_lowercase())
    } else {
        Err(format!(
            "\"{raw}\" does not look like an address: 0x and 40 hex characters are required"
        ))
    }
}

/// A non-negative number. A negative stake would reach the database and would mean an
/// order with an inverted sign somewhere nobody expects one.
fn positive(raw: Option<&&str>) -> Result<Decimal, String> {
    let raw = raw.ok_or_else(|| "a number is required".to_string())?;
    let v = Decimal::from_str(raw).map_err(|_| format!("\"{raw}\" is not a number"))?;
    if v < Decimal::ZERO {
        return Err(format!("\"{raw}\" is negative"));
    }
    Ok(v)
}
