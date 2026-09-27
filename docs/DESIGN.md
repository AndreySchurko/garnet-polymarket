# Garnet — Design

Written 2026-09-03, approved section by section before implementation began. This
is the specification the code was built against, kept as the record of **what this
system deliberately does not do** and why.

The numbered invariants have since moved to [ARCHITECTURE.md](ARCHITECTURE.md) and
grown there; this document keeps the reasoning that produced them.

---

## 1. Why Garnet exists

The predecessor searched for profitable wallets on its own, scored them and selected a
portfolio. That was measured, and it does not work.

**A leader's skill does not replicate outside the sample it was measured in.** The
top decile of 5,269 wallets returned +0.1383 in-sample and **+0.0008**
out-of-sample (Pearson r = 0.0363). The `closed_positions` table the selection
stood on showed 86.4% winning legs against 6.9% losing ones — a 12.5× skew, with a
correlation to real money of r = 0.116.

The only selector that survived an honest out-of-sample test was the t-stat of ROI
by settlement: +1.55% net, with monotonicity against the middle of −5.79% and
against the `t ≤ −2` group of −19.61%, t = 5.23 with clustering by market. It
remains **a tool for the operator**, not automation.

**Garnet does not select wallets. A human does.** The bot's job is to execute that
human's decision quickly, accurately, and without initiative of its own.

### Goals

1. A manually added wallet is copied unconditionally — identically before an event
   and in-play.
2. Each wallet has its own mode (shadow/live) and its own stake.
3. Full manual control through Telegram and a dashboard, with human-readable
   events.
4. The market scanner is a separate module, off by default, started by hand.
5. A schema that physically cannot swell to the 220 GB the predecessor reached.

### Non-goals, deliberately out of scope

- Any filtering of signals beyond the mechanical refusal reasons.
- Scoring, screening, benches, buckets, leaderboards, auto-promotion.
- Maker strategies, chasing the unfilled remainder, averaging by logic of our own.
- Micro-mode — deleted; the size is set by hand.

---

## 2. Architecture

### Bus boundaries

Both buses are kept, with non-overlapping responsibilities — in the predecessor
their roles blurred:

- **NATS** carries events: `signal.detected`, `order.submitted`, `order.filled`,
  `position.settled`, `alert.*`. The only producer is `garnet-core`.
- **Redis** holds state: the dedup of leader trades, API rate limits, the market
  metadata cache, feed cursors.
- The rule: one event is never written to both buses. A duplicated write is a
  defect.

Why NATS: Redis pub/sub loses a message if the subscriber was down. For alerts
about real trades, and for an audit trail, that is unacceptable.

### What was carried over, and the warning attached to it

- **Carried over unchanged**: `garnet-clob`, `garnet-blockchain`, `garnet-feed`,
  `garnet-redis`, `garnet-bus`, `garnet-types`.
- **Written from scratch**: `garnet-db` (a schema of its own), `garnet-config`.
- **Rewritten**: `garnet-copy-engine` — only sizing and the signal → order path
  remain; `filters.rs` was not carried over at all.
- **Cut down**: `garnet-risk` to the killswitch, the feed watchdog and feed lag.
  `exposure`, `circuit_breaker` and `triggers` are gone — those are indirect
  filters.
- **Not carried over**: scoring, screening, benches, buckets, `metrics.rs`,
  `portfolio.rs`.

> **A warning about "proven code".** The predecessor's execution path never placed
> a single real order in its entire life. EIP-712 signing, the neg-risk adapter
> and the handling of USDC.e all exist in the code and had never once been
> exercised in production. Carrying code over is not validating it.

See [PORT-AUDIT.md](PORT-AUDIT.md) for what actually happened to each crate.

---

## 3. The data model

Ten tables at design time (thirteen today). Everything the predecessor had
beyond this existed for the sake of selection, and of disk growth.

**What the schema deliberately does not have**: `wallet_scores`, `metrics`,
`bench`, buckets, `closed_positions`, `trades_raw`, `market_price_points`,
`wallet_snapshots`, `screening_reasons`. That is roughly 90% of the
predecessor's disk and 100% of its selection machinery.

---

## 4. The signal

A leader's buy becomes a signal. There are **no** checks on price, category, the
leader's size, time to the event, or pre-game versus in-play.

A skip is possible only for named mechanical reasons, each written into
`signals.verdict` as explicit text. At design time there were five;
`exposure_capped` and `rate_limited` were added later, each by an explicit
decision of the operator.

`insufficient_balance` applies to live only: the shadow ledger is allowed to go
negative, and for that reason a shadow signal is never skipped.

**`duplicate` is defined strictly and narrowly**: it is one and the same on-chain
trade seen by both detection circuits. The dedup key is
`(tx_hash, wallet, token_id, side)` and **never a coincidence of parameters**. A
log index does not exist in nature: it is absent from the RTDS frame, from
`/trades` and from `/activity`. Measured over 500 live trades: 500 unique
`transactionHash` values, zero collisions on this key. Price and size are not part
of the key — otherwise a redelivery of the same trade with different rounding would
pass as new and double the stake. A leader's second buy into the same market at
the same price is a new signal and a new buy of ours, even if it arrives a second
after the first. Dedup by parameters would bring filtering back through the side
door and would kill the leader's averaging, which we undertook to reproduce.

Extending this list is a change of strategy, not a refactor: only by an explicit
decision of the operator. In the predecessor the growth of that list silently
killed 35 signals out of 35.

## 5. Size, execution and shadow

- The size is the wallet's `stake_usd` on every signal. The leader's fraction,
  their bankroll and their conviction are not taken into account.
- The order is a limit IOC at `min(leader's price × (1 + max_slippage_pct),
  0.999)`.
- A partial fill is accepted; the remainder is **not chased**.
- Neg-risk: the collateral is strictly USDC.e; the adapter required live
  verification.

Shadow takes the same path and branches at one point: instead of submitting an
order, the real book is fetched and the ladder of asks is walked for `stake_usd`;
the hypothetical fill is written to `fills` with `mode='shadow'`.

- Accounting, positions, P&L and settlement are shared with live.
- **The fee is charged by the same formula as in live.** A free shadow would be
  systematically better than live by exactly the fee, and the comparison between
  the two modes would lose its meaning.
- The virtual capital is global, from the config, $1000 by default. A stake is
  debited, a payout credited.
- The ledger **may go negative and never blocks copying**. Shadow is a measuring
  instrument; blocking it would bias the sample exactly where it is most
  interesting. The negative balance is logged and visible in `/balance`.

---

## 9. Failures and reliability

### The failure matrix

| Failure | Response |
|---|---|
| RTDS silent (on 31.08 the `activity/trades` topic was down platform-wide) | a three-state probe: a NotFound frame / an empty ack / silence; the control is the `crypto_prices` topic. Fall back to polling, raise an alert |
| An order rejected | one retry against a fresh book, then a skip with a reason; a run of them raises an alert |
| RPC/DNS | the resolvers are pinned explicitly: the past "geoblock" and "RPC failure" turned out to be a flapping first nameserver |
| A divergence in the ledger | positions on chain are reconciled against the database every N minutes; a divergence raises an alert, **never a silent auto-correction** |
| A failed database write | the killswitch stops live |

### Validating the executor

The carried-over executor code counted as **unverified** until it had passed:

1. one manual live $1 order on a binary market — reconciled on chain and in the
   database;
2. one manual live $1 order on a neg-risk market — the predecessor used an
   outdated V1 neg-risk adapter that had never been exercised for real;
3. one redemption of each type (binary and neg-risk).

Until all three had passed, no wallet was to be moved to live. See the settlement
section of [ARCHITECTURE.md](ARCHITECTURE.md) for what happened when they were.

---

## 10. The scanner

The scanner is the module that searches for wallets worth copying. It is
**deliberately not part of this repository**: it is a separate binary with its own
database, started by hand, and nothing in the trading path depends on it.

Its design constraints were: complete isolation from the trading database, no
authority to add or enable a wallet, and no automatic promotion of anything it
finds. The output is a report a human reads.

---

## 13. The inheritance from the predecessor

The data was archived and verified before the server was decommissioned on
2026-09-03: 2.4 GB, 19/19 gzip PASS — `pnl_pos` (9,087,078 settled positions),
`markets` (495,584), `trades_raw_eligible` (10,363,873 rows across 3,703 wallets).

The predecessor's code is used as a reference for API traps, not as a foundation.

## 14. Open questions

Decided during self-review; change only deliberately:

- `equity_snapshots` — every 5 minutes plus after every fill and settlement. At
  288 snapshots a day across two modes that is about 210,000 rows a year, which is
  nothing.
- Reconciling positions on chain against the database — every 10 minutes. More
  often is pointless: redemption and transaction confirmation are slower anyway.

Left to the operator:

1. The alert threshold on the collateral balance.
2. The VPS tier: the disk size depends on whether the scanner runs on the same
   host. The trading path is happy with 40 GB; a scanner run needs headroom for
   its own database while it works.
