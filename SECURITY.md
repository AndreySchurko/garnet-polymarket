# Security Policy

Garnet holds API credentials and a private key that can move funds. A defect
here is not a crash — it is money. Please treat it accordingly.

## Reporting a vulnerability

**Do not open a public issue.**

Email **andreyschurko@gmail.com** with:

- what the problem is, and what an attacker gains from it;
- the smallest sequence of steps that demonstrates it;
- affected version or commit.

You will get an acknowledgement within 72 hours. If a fix is warranted, you will
be told when it lands, and credited by whatever name you prefer — or not
credited, if you would rather not be.

Please do not test against anyone else's deployment, and do not use a live
account to prove a point. A local stack reproduces everything that matters.

## In scope

- key or credential leakage: into logs, error messages, core dumps, Telegram
  messages, dashboard responses, or the database;
- authorisation gaps in the Telegram bot or the dashboard mini-app — anything
  that lets a chat id outside `allowed_chat_ids` read state or place orders;
- signature or nonce handling in order construction that a third party could
  exploit;
- a way to bypass the killswitch, the loss stop, or the manual confirmation on
  guarded actions;
- accounting defects that let the bot trade funds it does not have, or lose
  track of funds it does.

## Out of scope

- losing money because the market moved, or because a copied wallet traded
  badly. Garnet copies the operator's decisions without judging them; that is
  the design, documented in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md);
- rate limits, outages or breaking changes at Polymarket, Polygon RPC providers
  or Telegram;
- the deliberately non-default development ports in `docker-compose.yml`, or the
  development credentials in it;
- findings that require the attacker to already have your `.env` file or shell
  access to your host.

## Operator checklist

Most real-world incidents are not code defects. If you run Garnet:

- `chmod 600 .env`, and never commit it — `.gitignore` already covers it, along
  with `config.toml` and database dumps;
- keep the dashboard bound to `127.0.0.1` and terminate TLS in front of it;
- keep `allowed_chat_ids` non-empty and correct. An empty list means the bot
  answers nobody, which is the safe default;
- use a trading account that holds only what you are prepared to lose, not your
  main wallet;
- run in `shadow` mode first. The whole point of shadow mode is that it costs
  nothing to discover that your configuration is wrong.
