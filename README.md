<div align="center">

<h1><img src="docs/assets/garnet-logo.png" alt="Garnet — Polymarket copy-trading bot" width="300"></h1>

### Self-hosted Polymarket copy-trading bot, written in Rust

**No scoring. No screening. No opinions.**
You name the wallets — the bot copies them, fast, and never second-guesses you.

[![License: BUSL-1.1](https://img.shields.io/badge/License-BUSL--1.1-6d28d9?style=for-the-badge)](LICENSE)
[![Free for individuals](https://img.shields.io/badge/Individuals-free%20forever-16a34a?style=for-the-badge)](LICENSE-COMMERCIAL.md)
[![Rust](https://img.shields.io/badge/Rust-stable-b7410e?style=for-the-badge&logo=rust&logoColor=white)](rust-toolchain.toml)
[![Tests](https://img.shields.io/badge/tests-669-0ea5e9?style=for-the-badge)](#testing)
[![Warnings](https://img.shields.io/badge/compiler%20warnings-0-16a34a?style=for-the-badge)](#testing)

[**Wallet Reports**](#-wallet-intelligence--the-part-that-actually-makes-money) ·
[**Quick start**](#-quick-start) ·
[**Your key**](#-your-private-key) ·
[**Architecture**](docs/ARCHITECTURE.md) ·
[**Services**](#-services) ·
[**Support the project**](#-support-the-project) ·
[**Telegram control**](#-telegram) ·
[**Contact**](#-contact)

</div>

---

## The uncomfortable truth this project is built on

> **A copy-trading bot is worth nothing without profitable wallets to copy.**

Everything else — execution speed, slippage control, settlement accounting — only
decides *how much* of someone else's edge survives the trip to your account. It
cannot manufacture an edge that isn't there.

So Garnet does one thing and does it honestly:

| | |
|---|---|
| **You** decide which wallets are worth copying | judgement, research, or a [report](#-wallet-intelligence--the-part-that-actually-makes-money) |
| **Garnet** copies them without hesitation | detection, sizing, execution, accounting, settlement |

Every rejection the bot is allowed to make is one of **seven** named reasons, and
each new one requires a deliberate decision — never a refactor:

```
wallet_disabled · slippage_exceeded · insufficient_balance
market_not_tradable · duplicate · exposure_capped · rate_limited
```

---

## ⚡ What it actually does

<table>
<tr><td width="50%" valign="top">

**Three independent detection circuits**

One truth, delivered three ways — because on 31.08.2026 Polymarket's
`activity/trades` topic went down platform-wide, from every IP at once, for
hours.

- **RTDS websocket** — fastest. Nothing is ever sent to it; a keepalive cuts
  delivery by 2.5×.
- **`/activity` polling** — the safety net.
- **Polygon logs** — decoded from the verified **V2** ABI, independent of
  Polymarket's infrastructure entirely.

All three collapse onto one dedup key. A third delivery, not a third truth.

</td><td width="50%" valign="top">

**Execution that knows what the exchange actually does**

Each of these was paid for with a real defect:

- orders are **FAK** only — `GTC` would make you a maker, the opposite of
  copying a taker
- size is **snapped to the exchange's grid**, not rounded
- fill price comes from the **trade feed**, not from the order — the difference
  measured 15%
- **"rejected" and "unknown" are different outcomes.** Retrying an accepted
  order once bought $2.00 where $1.00 was intended

</td></tr>
<tr><td valign="top">

**Live and shadow, side by side, always**

Shadow is a measuring instrument, not a toy: it pays the **same fee**, uses the
**same order path**, and reads the **same book**. Otherwise it would flatter
itself by exactly the amount you forgot to charge it.

The killswitch stops live trading and deliberately leaves shadow running.

</td><td valign="top">

**Accounting that reconciles against the chain**

Settlement is decided by `tokens[].winner` and `token_id` only — never by
outcome labels, because those are Up/Down, Over/Under or team names, and
settling by label books every win as a total loss. That mistake once fabricated
**3,659** false resolutions that way.

</td></tr>
</table>

### The flow

```
  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
  │ RTDS socket  │  │  /activity   │  │ Polygon logs │   three deliveries
  │  (fastest)   │  │ (safety net) │  │ (independent)│   of one truth
  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘
         └─────────────────┼─────────────────┘
                           ▼
              dedup  (tx_hash, wallet, token_id, side)
                           ▼
    ┌──────────────────────────────────────────────────┐
    │  DECIDE   slice window · slippage · balance      │
    │           exposure cap · fire rate              │  → 7 named reasons
    └──────────────────────┬───────────────────────────┘
                           ▼
    ┌──────────────────────────────────────────────────┐
    │  EXECUTE  FAK · grid-snapped size · $1 minimum   │
    │           LIVE ──────────────┐   SHADOW          │
    │           real CLOB order    │   same fee, same  │
    │                              │   book, no touch  │
    └──────────────────────┬───────┴──────────────────-┘
                           ▼
    ┌──────────────────────────────────────────────────┐
    │  ACCOUNT  positions · fills · equity snapshots   │
    │  EXIT     leader sells · merges · time-based     │
    │  SETTLE   tokens[].winner → payout → P&L         │
    │  RECONCILE  ledger vs. chain, every 10 minutes   │
    └──────────────────────┬───────────────────────────┘
                           ▼
         Telegram bot  ·  mini-app dashboard  ·  NATS events
                           ▲
                    garnet-watch  ← a separate process, because a watcher
                                     living inside what it watches dies with it
```

### Risk controls, and what each one is actually for

| Control | What it does | Why it exists |
|---|---|---|
| **Killswitch** | stops live trading, keeps positions | lives in the **database** — a stop that a restart undoes is not a stop |
| **Daily loss stop** | latches for the UTC day | a bot that merely loses money will not stop on its own |
| **Exposure cap** | ceiling per **event**, not per token | outcomes of one condition are correlated; a per-token cap is bypassed by buying the neighbour |
| **Fire rate limit** | entries per wallet per window | one leader fired 12 decisions in a minute across 12 markets: **94% of the loss**, ROI −15.74% vs +3.48% |
| **Slice window** | collapses one leader order into one of ours | 43% of buys were pieces of an already-copied order; first slice **+7.4%**, later slices **−15%** |
| **Health monitor** | measured by **flow**, not liveness | both guards once reported OK while the bot was blind for 6.5 hours |
| **`panic` flatten** | closes everything | requires a **typed phrase**, not a button — and the phrase includes your installation's own name, so one copied from a chat log won't fire here |
| **Reconciler** | ledger vs. chain | "could not read" is reported as its own outcome, never as "agreed" |

---

## 🔐 Your private key

> In 2026 GitHub filled up with "Polymarket copy-trading bots" that shipped the
> operator's `.env` to someone else's server — in most cases through a poisoned npm
> dependency. You are right to ask what this one does with your key. Here is the
> answer, and how to check it yourself instead of taking it on trust.

**Where it lives.** In `.env` on your own machine — `chmod 600`, ignored by git — read
from the environment at start-up. It signs orders locally (EIP-712). The signature goes
to the exchange; the key does not.

**Where it never goes.** Not into the logs: the start-up config is printed in full, and
the key, the L2 API secret and the passphrase appear in it as `***REDACTED***` — a test,
`the_private_key_never_reaches_a_log_line`, holds that line. The key is read in exactly
two places, `garnet-bin/src/clob_live.rs` and `garnet-blockchain/src/client.rs`, and
handed straight to the signer as a `SecretString`.

**No phone-home.** No telemetry, no licence server, no update check. This is the
complete list of hosts Garnet's own code talks to; all but the geoblock probe are
settings in `config.toml`, and the Polymarket SDK is built with its `clob` feature only
and pointed at `clob_host`:

| Host | What for |
|---|---|
| `clob.polymarket.com` | order books, orders |
| `polymarket.com/api/geoblock` | preflight only: is your server's IP allowed to trade |
| `gamma-api.polymarket.com` | market metadata |
| `data-api.polymarket.com` | `/activity` polling — the safety-net detection circuit |
| `ws-live-data.polymarket.com` | the RTDS feed — the fast detection circuit |
| your `POLYGON_RPC_URLS` | chain logs, balances, redemption |
| `gasstation.polygon.technology` | gas price |
| `api.telegram.org` | your own `garnet-tg`, with your own token |

The dashboard page additionally loads `telegram-web-app.js` from `telegram.org` in your
browser — a Telegram Mini App cannot run without it. To see the list for yourself
(it also prints documentation links and the `example.com` placeholders from tests):

```bash
grep -rhoE '(https?|wss?)://[a-zA-Z0-9.-]+' --include='*.rs' --include='*.toml' --include='*.html' crates config.example.toml | sort -u
```

**No npm.** The engine is Rust from end to end; the only JavaScript in the repository
is the dashboard's inline script. Every dependency is pinned in `Cargo.lock`, and the
Polymarket SDK is pinned to an exact version (`=0.7.0`). `cargo audit` is clean as of
2026-09-30 but for one entry: RUSTSEC-2023-0071 in `rsa`, which sits in `Cargo.lock` as a
dependency of `sqlx-mysql` and is compiled into nothing — Garnet speaks PostgreSQL only.
The three warnings it also prints (`derivative`, `paste`, `lru`) come in through `alloy`.

**You don't need a key to try it.** With no keys in the environment the live path does
not come up at all — `live path disabled: no keys are set, live wallets will be
refused` — and [shadow mode](#start-in-shadow-mode-always) runs on public data alone.

**When you do go live:**

- use a **dedicated wallet** funded with what you are prepared to lose — never your main one;
- Garnet asks for one signing key and **never for a seed phrase**. Anything that asks for a
  mnemonic to "copy trades" is not a copy-trading bot;
- read before you run: `cargo tree`, `cargo audit`, and the `grep` above.

Found a way the key could leak? That is exactly what [SECURITY.md](SECURITY.md) is for.

---

## 📊 Wallet Intelligence — the part that actually makes money

<div align="center">

### ⬡ You have the engine. Now you need the wallets.

*Reports produced on my own crawler infrastructure, from on-chain Polymarket
history, to filters you specify.*

**→ [✉️ andreyschurko@gmail.com](mailto:andreyschurko@gmail.com) ←**

</div>

The scanner and crawler that generate these reports are **not part of this
repository** — they run on my server, against a continuously accumulated dataset.
What you get is the output: wallets, their numbers, and the analysis.

### How it works

```
  1. You send your filters        2. I run them against        3. You get a report
     ──────────────────              the full dataset            ─────────────────
     min ROI, min trades            ─────────────────            ranked wallets
     hold time, market type         months of Polymarket         full metrics
     drawdown, recency              trading history              CSV + analysis
     position size, win rate        on-chain, verifiable         ready for /add
```

### Tiers

<table>
<tr>
<th width="33%">🔹 One-Shot</th>
<th width="33%">🔸 Subscription</th>
<th width="33%">💎 Institutional</th>
</tr>
<tr valign="top">
<td>

**A single report, on demand**

- your filter set, one run
- ranked wallet list with full metrics
- CSV + written analysis
- addresses ready to paste into `/add`
- delivered within an agreed window

*For: trying this out, or a one-time portfolio refresh.*

</td>
<td>

**Fresh wallets, continuously**

- regular re-runs on your filters
- **new** qualifying wallets as they appear
- **degradation alerts** — when a wallet you copy stops working
- refinement of filters between runs
- history, so you can see what changed

*For: running the bot as an ongoing operation.*

</td>
<td>

**Custom scope**

- bespoke filters and derived metrics
- larger universes, longer histories
- raw data export / delivery format of your choice
- cross-market coverage
- direct line, priority turnaround
- commercial license bundled

*For: funds, prop desks, professional operations.*

</td>
</tr>
<tr>
<td align="center"><b>Payment in crypto</b><br><a href="mailto:andreyschurko@gmail.com">Get a quote →</a></td>
<td align="center"><b>Crypto or invoice</b><br><a href="mailto:andreyschurko@gmail.com">Contact →</a></td>
</tr>
</table>

### What a report contains

<details>
<summary><b>Example report (illustrative structure — not real data)</b></summary>

| Wallet | Closed | Win % | ROI | Median hold | Avg size | Max DD | Last active | Markets |
|---|---|---|---|---|---|---|---|---|
| `0xa1b2…c3d4` | 218 | 61.9% | +18.4% | 3.1 h | $340 | −12.1% | 4 h ago | sports, crypto |
| `0xe5f6…7890` | 94 | 54.3% | +26.7% | 27.4 h | $1,180 | −21.8% | 1 d ago | politics |
| `0x1122…3344` | 512 | 58.1% | +9.2% | 1.4 h | $95 | −7.4% | 20 min ago | crypto |

Plus, per wallet:

- **entry-quality profile** — how much of the return survives a taker fill at
  your slippage threshold
- **slice behaviour** — whether one decision arrives as one order or fifteen
  (this decides your `slice_window_secs`)
- **firing pattern** — burst frequency, which sets your `fire_limit`
- **concentration** — how much of the P&L came from how few positions
- **survival** — how the wallet's performance held up month over month, not just
  in aggregate

The metrics are chosen to answer one question: *how much of this wallet's edge
will still be there after your bot pays the spread and the fee.*

</details>

> [!IMPORTANT]
> Reports are **data and analysis**, not investment advice, and not a promise of
> returns. Every number is computed from public on-chain history, which you can
> verify yourself. Past performance of any wallet does not predict its future
> performance — and part of what the reports measure is precisely how often it
> doesn't.

---

## 🚀 Quick start

### Requirements

- **Rust** stable — see [`rust-toolchain.toml`](rust-toolchain.toml)
- **Docker** — for Postgres, Redis and NATS
- a **Polymarket account** with L2 API keys ([get one](#-recommended-infrastructure))
- a **Telegram bot token** from [@BotFather](https://t.me/BotFather)
- a VPS if you want it running 24/7 ([recommendations below](#-recommended-infrastructure))

### Install

```bash
git clone https://github.com/AndreySchurko/garnet-polymarket.git
cd garnet-polymarket

# infrastructure: Postgres :5433, Redis :6380, NATS :4223
docker compose up -d

# configuration
cp .env.example .env            # secrets — fill in six values, then chmod 600
cp config.example.toml config.toml

cargo build --release --bin garnet-core --bin garnet-tg --bin garnet-dash
```

### Check everything before risking a cent

```bash
./target/release/garnet-core --config config.toml --preflight
```

Thirteen probes, one line per dependency. It verifies your signature type
**against the chain** — if your funder answers `masterCopy()`, it is a Gnosis
Safe and `POLY_SIGNATURE_TYPE` must say so. Getting this wrong means every order
is rejected, and the preflight tells you before the first one is.

Add `--live` to also refuse to start when your safety limits are switched off.

### Start in shadow mode. Always.

```bash
./target/release/garnet-core --config config.toml    # trading core
./target/release/garnet-tg   --config config.toml    # Telegram control
./target/release/garnet-dash --config config.toml    # mini-app dashboard
```

Then, in Telegram:

```
/add 0xWALLET…        add a wallet — it is copied unconditionally from this moment on
/mode shadow          paper trading: same fees, same book, no exchange contact
/stake 10             $10 per signal
/slippage 15          15% ceiling on the miss versus the leader's price
/health               is the flow alive?
```

Run it for a few days. Read `/pnl`, `/matchup` and `/slippage`. Only then
consider `/mode live` — and when you do, start with money you are prepared to
lose entirely.

> [!WARNING]
> **History is not a signal.** A wallet is copied from the moment you add it,
> never retroactively. A quiet wallet's `/activity` returns weeks of history, and
> on the first tick all of it looks like news — that mistake once produced 69
> rejections and 28 copies of trades up to 3.4 days old, one of which filled at
> 0.001 against the leader's 0.260.

---

## ⚙️ Configuration

Two files, and the split matters:

| File | Holds | In git? |
|---|---|---|
| [`.env`](.env.example) | secrets: keys, tokens, RPC URLs | **never** |
| [`config.toml`](config.example.toml) | behaviour: limits, intervals, thresholds | **never** (example is) |

Both examples are heavily commented — each setting says not just what it does but
why it defaults to what it does. Highlights:

<details>
<summary><b>Settings whose defaults are deliberate</b></summary>

| Setting | Default | Why |
|---|---|---|
| `fire_limit` | `0` (off) | A threshold must come from a measurement, not from a guess. Counting runs anyway — `/firerate` shows what each limit *would have* blocked, so you can set it from your own data. |
| `daily_loss_limit_usd` | `0.0` (off) | `--preflight --live` **refuses to start** with this at zero. Off is the honest default for a fresh install; zero in production is a decision you make explicitly. |
| `per_market_cap_usd` | `0.0` (off) | A fresh install that rejects its very first signal is the most expensive failure this project has. |
| `max_hold_hours` | `0` (off) | A prediction-market position resolves to 0 or 1 anyway; dumping into a thin book is usually worse than waiting. Turn it on when capital is *tied up*, not when you want to lose less. |
| `slice_window_secs` | `300` | One taker order arrives as many frames with **different** `tx_hash`, so hash dedup cannot catch them. This can. |
| `polygon_ws_url` | `""` (off) | The third circuit needs your own node. If you set `ws` but leave `rpc` empty, the process refuses to start — block timestamps would be unavailable, and trades would get *our* clock instead of the leader's. |
| `allowed_chat_ids` | `[]` (nobody) | An empty allowlist means the bot answers **nobody**, which is the safe default. It stays silent rather than saying "access denied" — a reply confirms the bot exists and controls something. |

</details>

---

## 💬 Telegram

Full operator control from your phone — no `psql`, no SSH.

<details>
<summary><b>All commands</b></summary>

**Wallets** — `/wallets` (with 24h result) · `/add <address>` · `/wallet <address>` (inline buttons) · `/on` · `/off`

**Trading** — `/mode <live|shadow>` · `/stake <usd>` · `/slippage [<%>]` · `/positions [open|all]`

**Reporting** — `/pnl [day|week|all]` · `/signals [N]` · `/balance` (with 24h delta) · `/matchup [day|week|all]` · `/sources [day|week|all]` · `/firerate` · `/latency`

**Safety** — `/kill` · `/resume` · `/health` · `/flatten <graceful|hybrid|panic>`

**Other** — `/app` (opens the dashboard mini-app) · `/help`

`/slippage` with no argument prints the distribution of your fill miss, bucketed,
**with the result of closed positions per bucket** — the only thing that answers
"are expensive fills actually losing money". Move the threshold by that
instrument, not by feel.

</details>

The dashboard (`garnet-dash`) is a Telegram mini-app. It binds to `127.0.0.1`
only; put Caddy or nginx in front for TLS. There are **no sessions and no
cookies** — nothing to steal and nothing to rotate. Auth is `initData`, signed by
your bot token, checked against the same allowlist.

---

## 🧪 Testing

```bash
docker compose up -d
cargo test --workspace --no-fail-fast
```

**669 tests**, 59 test binaries, **zero compiler warnings**.

`--no-fail-fast` is not optional: every test gets its own Postgres schema, and a
suite that aborts halfway leaves them behind, so the next run fails on
`CREATE SCHEMA` instead of on anything real. If the whole suite dies at once,
look outside the code first — Docker, a taken port, leftover schemas.

Tests never touch the trading database. `TEST_DATABASE_URL` defaults to
`garnet_test`, because on 04.09.2026 the default pointed at the live one and a
test run added **eleven wallets to the production registry** — one of them in
live mode.

---

## 📚 Documentation

| | |
|---|---|
| [**docs/ARCHITECTURE.md**](docs/ARCHITECTURE.md) | crate layout, the numbered invariants, and what each one cost to learn |
| [**docs/DESIGN.md**](docs/DESIGN.md) | the original specification: what this system deliberately does *not* do |
| [**docs/PORT-AUDIT.md**](docs/PORT-AUDIT.md) | what was carried over from the earlier implementation, what wasn't, and why |
| [**deploy/README.md**](deploy/README.md) | systemd units, deployment, the watchdog timer |
| [**CONTRIBUTING.md**](CONTRIBUTING.md) | conventions, the review bar, contribution licensing |
| [**SECURITY.md**](SECURITY.md) | reporting vulnerabilities, and the operator checklist |

If you read only one thing, read the invariants. Every one of them is a defect
that was paid for once, with real money, so that it doesn't have to be paid for
again.

---

## 🛠 Services

I build and run this software. If you would rather not do it yourself, I can do
it for you. Payment in crypto; scope and price agreed up front, in writing.

<table>
<tr valign="top">
<td width="50%">

### 🖥 Server setup & deployment

- VPS provisioning, hardening, users, firewall
- Docker, Postgres, Redis, NATS, systemd units
- Caddy + TLS for the dashboard
- Garnet installed, configured, preflight green
- your wallets loaded, shadow mode running
- a walkthrough so you understand what you now own

*You hand over a bare server. You get back a working bot.*

</td>
<td width="50%">

### 🔧 Maintenance & monitoring

- OS and dependency updates, backups
- monitoring and alerting that reaches you
- incident response when Polymarket changes an API
- upgrades to new Garnet releases
- monthly health report on the bot and the box

*A trading bot is not a "set and forget" asset. This is the part most people
skip and then regret.*

</td>
</tr>
<tr valign="top">
<td>

### ⚙️ Customisation & new modules

- new exit strategies, sizing models, risk rules
- extra detection circuits and data sources
- custom reporting and dashboards
- integration with your own systems
- performance work on the hot path

*Built to the conventions in this repo, with tests, so it survives the next
upgrade.*

</td>
<td>

### 🤖 Trading bots, built to order

**Not only Polymarket.**

- other prediction markets — Kalshi, Limitless, and others
- centralised crypto exchanges — spot, futures, market making
- DEX and on-chain strategies
- copy-trading, arbitrage, market making, custom logic
- Rust or Python, your infrastructure or mine

*Fifteen crates and 669 tests of evidence that I finish what I start.*

</td>
</tr>
</table>

<div align="center">

**[✉️ andreyschurko@gmail.com](mailto:andreyschurko@gmail.com)**

</div>

---

## ❤️ Support the project

<div align="center">

### Garnet is free for individuals. Forever. No key, no trial, no phone-home.

**If it makes you money, consider sending some back.**

</div>

This is a real ask, so let me be specific about why. Garnet exists because
someone paid — in money and in nights — for every defect listed in the
invariants. A bot that books every win as a total loss costs you nothing to
avoid here, because it already cost someone else 3,659 false resolutions to find.
That work continues: Polymarket changes its API, contracts move to V2, a topic
goes down platform-wide for hours. Every one of those became a fix in this
repository that you get for free.

**If Garnet is trading your money right now, a donation is the cheapest part of
your stack.**

<div align="center">

| Asset | Network | Address |
|:---|:---|:---|
| **₿ Bitcoin** | Bitcoin mainnet | `bc1qjy2hk0t3dplj9z0rs8ym4jvqv05v6y8whtxsrg` |
| **Ξ Ethereum** / ERC-20 | Ethereum mainnet | `0x28D64da03A34823CD5C4A253C484905A4eb1994a` |
| **💵 USDT** | Tron — TRC-20 | `TPjsbd5YYLE3oeQfiytUJu5xzJ8XBqfvH7` |
| **💵 USDC** | Polygon mainnet | `0x28D64da03A34823CD5C4A253C484905A4eb1994a` |
| **◎ Solana** | Solana mainnet | `8KujLApQTyGpvS9aeE1QpibpU7T6DbpqPbKZdJ12DP7m` |
| **Ⓣ GRAM** / Toncoin | TON — The Open Network | `UQChevAIOfh3p1RceK1Pcr4RZxxznmmvIqlr8AdTXmMw_-KC` |

> **Please verify the network before sending funds. Sending tokens to an address on the wrong network may result in permanent loss.**

</div>

**Other ways to support nothing:**

⭐ Star the repository — it is how anyone finds this at all
☕ Ko-fi — one-time or recurring support https://ko-fi.com/andreyschurko
🧡 Boosty — support and donations https://boosty.to/andreyschurko/donate
🐛 Report a bug you hit, with the log line that showed it
📣 Tell someone who trades on Polymarket
🔗 Use the [referral links below](#-recommended-infrastructure) when you sign up for something anyway


Every contribution, whether financial or through code, testing, documentation or feedback, helps keep the project alive and actively maintained.
---

## 🌐 Recommended infrastructure

> [!NOTE]
> **The links in this section are referral links.** If you sign up through them,
> I receive a commission at no extra cost to you. I am telling you this plainly
> because a recommendation you can't audit is worth nothing — and because these
> are tools I actually run this bot on. If you would rather not use them, every
> provider below is one search away.

### Trade on Polymarket

**[→ Sign up for Polymarket](https://polymarket.com/?r=garnetbot)**

You need an account to use Garnet at all. Note that Polymarket restricts access
in some jurisdictions, including the US — check whether you are eligible before
you build a stack on it.

### A server to run it on

Latency matters for copy trading: you are racing to take liquidity that someone
else just revealed. Choose a datacentre close to the exchange, not close to you.

Two hosts are built for precisely this, and they are what I run Garnet on. Ryzen 9
9950X, DDR5, NVMe, Ubuntu, full root — and servers sitting **beside Polymarket's
own endpoints** in Amsterdam and Dublin, Ireland being the closest region
Polymarket does not geo-restrict.

| Provider | Pick this tier | What you get |
|---|---|---|
| **[TradingVPS](https://app.tradingvps.io/aff.php?aff=160)** | **$35/mo** — 2 cores, 8 GB DDR5 | Plans start at $19; $35 is the sweet spot for Garnet, and $59 buys 4 cores and 16 GB with room to build on the box |
| **[TradoxVPS](https://app.tradoxvps.com/aff.php?aff=67)** | **$69/mo** — 4 cores, 12 GB DDR5, 150 GB NVMe | Builds and runs the whole stack out of the box — no swap, no waiting on the linker |

**Take Amsterdam for Garnet** — **~10 ms to the live feed**, measured against
Polymarket's own endpoints:

| | Live feed (WS p50) | Order round-trip (warm p50) | Best for |
|---|---|---|---|
| **Amsterdam** | **~10 ms** | ~22 ms | copy trading and event-driven entries — **Garnet** |
| **Dublin** | ~14 ms | **~21 ms** | market making, rapid post/cancel |

Garnet lives or dies on the feed: every millisecond ahead of `DECIDE` is liquidity
someone else is taking, and the order path costs the same either way to within a
millisecond. Run a box in both and you have the cheapest geographic failover
money can buy.

Then watch what you paid for: `/latency` prints the same measurement from your own
server, continuously.

**Minimum that actually works:**

| | RAM | CPU | Disk |
|---|---|---|---|
| **Running the bot** | 2 GB | 2 cores | 20 GB |
| **Building it too** | 4 GB+ | 2 cores | 30 GB |
| **Comfortable** | 8 GB | 4 cores | 40 GB |

Below 4 GB the release build runs out of memory linking `alloy` and the SDK. Two
ways around it: build elsewhere and copy the binary, or add swap and be patient.

### A Polygon RPC node

The third detection circuit and every on-chain read need one. Free public
endpoints work (`https://polygon-bor-rpc.publicnode.com` answers anonymously and
also serves websockets), but they rate-limit and they go down.

| Provider | Free tier | Notes |
|---|---|---|
| **[QuickNode](https://quicknode.com/signup?via=andrey)** | limited | fastest in most benchmarks |
| **[Chainstack](https://chainstack.referral-factory.com/uCFimyIe)** | modest | good regional coverage |

### Also useful

| | For what |
|---|---|
| **[Binance](https://www.binance.com/activity/referral-entry/CPA?ref=CPA_00L18UEUYD)** / **[OKX](https://okx.com/join/77265196)** | funding the account, moving USDC to Polygon, cashing out |
| **[UptimeRobot](https://uptimerobot.com/?rid=975ac4efe6e80d)** | alerting when `garnet-watch` says something is wrong and you are asleep |

---

## ✉️ Contact

<div align="center">

| | |
|:---|:---|
| ✉️ **Email** — reports, services, commercial licensing, security | [andreyschurko@gmail.com](mailto:andreyschurko@gmail.com) |
| 🐛 **Issues** — bugs, and questions about running it | [GitHub Issues](https://github.com/AndreySchurko/garnet-polymarket/issues) |
| 💬 **Discussions** — anything that is not a bug | [GitHub Discussions](https://github.com/AndreySchurko/garnet-polymarket/discussions) |

</div>

Measurements get published with the releases — what a threshold actually did over
a month, which detection circuit earned its keep, what broke at Polymarket and
what the fix was. Watch the
[releases](https://github.com/AndreySchurko/garnet-polymarket/releases) if you run
this bot.

---

## ❓ FAQ

<details>
<summary><b>Is this profitable?</b></summary>

By itself? No. **Garnet has no opinion about which wallets are worth copying —
that is the entire design.** It is an execution engine: it decides how much of
someone else's edge reaches your account, not whether that edge exists.

Run it in shadow mode for a couple of weeks with wallets you believe in and read
`/matchup`. That number — your result against the leader's, fees included — is the
only honest answer, and it will be about *your* wallets, not about this software.

</details>

<details>
<summary><b>Can I run this on a company account / for a fund?</b></summary>

Not under the free grant. The [BUSL](LICENSE) reserves that for individuals
trading their own funds; anything involving a legal entity or third-party money
needs a [commercial license](LICENSE-COMMERCIAL.md). Evaluation is always free —
you may read the code, run the tests and use shadow mode before any conversation
about money.

</details>

<details>
<summary><b>How fast is it? Can it front-run the leader?</b></summary>

**No, and nothing here claims otherwise.** Garnet copies trades that have already
executed. The Polygon log circuit is more *reliable* than the websocket but not
faster — a log appears once the settlement transaction is in a block, and the
order was matched by the CLOB before that.

What the engine does is avoid losing seconds on top of that: nothing on the hot
path waits sequentially when it could wait in parallel, and `/latency` measures
the result end-to-end, per trade, out of the database — so the number survives a
restart and can be compared before and after a change.

</details>

<details>
<summary><b>Which markets does it support?</b></summary>

Polymarket, including neg-risk markets, binary markets, sports, crypto pairs and
totals. Outcome labels are never trusted — settlement goes by `token_id` and
`tokens[].winner` only, which is what makes Up/Down, Over/Under and team-name
markets work at all.

Other venues are not supported in this repository. They are available
[as custom work](#-services).

</details>

<details>
<summary><b>What happens on the Change Date?</b></summary>

On **2030-09-25** this version becomes available under **Apache-2.0** — for
everyone, companies included. Each later release carries its own four-year clock.
This is the guarantee that the code cannot be taken away from you: if this
project is abandoned, the version you hold today opens on a date already written
down.

</details>

<details>
<summary><b>Will you add feature X?</b></summary>

Open an issue and make the case. Two categories are settled, though: **wallet
scoring and screening** are not coming back (see above), and anything that makes
the bot second-guess the operator's wallet choice contradicts the one rule the
whole design rests on — a wallet you added is copied unconditionally.

Everything else is a conversation, and paid [custom work](#-services) is always
an option if you want it sooner than I would get to it.

</details>

---

## 📄 License

**Garnet is source-available under the [Business Source License 1.1](LICENSE).**

<table>
<tr><th>Free, forever</th><th>Requires a commercial license</th></tr>
<tr valign="top"><td>

✅ You are an individual, trading **your own funds**
✅ Reading, modifying, testing the code
✅ Non-production use by anyone — including companies evaluating it
✅ Any use at all, once the Change Date passes

*Making a profit on your own account does not make your use commercial.*

</td><td>

💼 Any company, fund, partnership or other legal entity
💼 Trading or managing **third-party** money
💼 Selling the output — signals, fills, derived data — to others
💼 Proprietary trading firms, market makers, brokers, asset managers

*→ [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md) · [andreyschurko@gmail.com](mailto:andreyschurko@gmail.com)*

</td></tr>
</table>

Every release becomes **Apache-2.0 four years after publication** — for this
version, on **2030-09-25**.

To be precise about a word that gets misused: the BUSL is **not** an
OSI-approved Open Source license, and this README will not call it one. It is
source-available, with a guaranteed open future. If that distinction matters to
you, it should — and now you know exactly where this project stands.

---

## ⚠️ Disclaimer

**This software places real orders with real money on a live market.**

- **No financial advice.** Nothing in this repository, in any report, or in any
  message from me is investment advice, a recommendation, or a solicitation.
- **No warranty.** The software is provided "as is", without warranty of any
  kind. See the [LICENSE](LICENSE).
- **You will lose money.** Prediction markets are risky; copy trading compounds
  someone else's risk with your own execution risk. Trade only what you can
  afford to lose entirely.
- **Past performance means nothing.** Not a copied wallet's, not a report's, not
  a shadow-mode backtest's. Part of what the reports measure is how often past
  performance fails to continue.
- **You are responsible for your own compliance.** Prediction markets are
  restricted or prohibited in some jurisdictions — the US among them. Check your
  local law, and Polymarket's terms, before you run this.
- **Bugs exist.** 669 tests and a list of invariants paid for in real defects do
  not add up to a guarantee. Start in shadow mode, keep the loss stop on, and
  never fund the trading account from your main wallet.

By using Garnet you accept that every trade it makes is your decision and your
risk.

---

<div align="center">

**⬡ Garnet** — built in Rust, for people who would rather copy a good decision
than invent a bad one.

If this saved you time or money, [say so](#-support-the-project). ⭐

</div>
