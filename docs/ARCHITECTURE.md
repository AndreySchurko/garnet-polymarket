# Garnet — Architecture

Copy trading on Polymarket **without selection**. A human names the wallets; the
bot executes their decision quickly and without initiative of its own.

| | |
|---|---|
| [**DESIGN.md**](DESIGN.md) | the specification: what this system deliberately does not do |
| [**PORT-AUDIT.md**](PORT-AUDIT.md) | what was carried over from the predecessor, what was not, and why |

---

## The one rule

**A wallet added by the operator is copied unconditionally.** A signal may be
skipped for seven named reasons and no others:

`wallet_disabled` · `slippage_exceeded` · `insufficient_balance` ·
`market_not_tradable` · `duplicate` · `exposure_capped` · `rate_limited`

The first five are mechanical. The sixth, `exposure_capped`, appeared by an
operator's decision on 06.09.2026 — that is a change of strategy, not a refactor.
The seventh, `rate_limited`, arrived the same way on 19.09.2026, and an eighth
will arrive by that route or not at all. In the predecessor the list of reasons
grew on its own, and the bot rejected 35 signals out of 35.

What was deleted and is not coming back: scoring, screening, benches, buckets,
leaderboards, auto-promotion, micro-mode.

## Crate layout

```
garnet-types       the shared vocabulary: Mode, Side, OutcomeIndex, BookSnapshot
garnet-db          the schema (13 tables) and the repositories; knows SQL, not trading
garnet-core        pure logic: frame parsing, fees, the book, shadow, settlement
garnet-copy-engine the decision to copy, and exits; knows nothing about the network
garnet-risk        killswitch, health, metrics, rate limiting
garnet-clob        the CLOB client (carried over from the predecessor with one change: the order type)
garnet-blockchain  redemption, balances, order signing (carried over from the predecessor)
garnet-feed        the RTDS socket and the Polygon log circuit
garnet-bus         NATS: events
garnet-redis       Redis: state
garnet-config      process configuration
garnet-telegram    the operator's controls: the `garnet-tg` binary
garnet-dash        the dashboard mini-app: the `garnet-dash` binary
garnet-watch       the watcher: the `garnet-watch` binary
garnet-bin         the wiring and the `garnet-core` binary
```

Dependency direction: `types` <- `db` <- `core` <- `copy-engine` <- `bin`.
`clob`, `blockchain`, `feed`, `bus` and `redis` are leaves that `bin` stitches
together.

---

## The invariants, each one paid for by a defect

These are numbered because the numbers are referenced from the code. When you
change one, say which defect and when.

**1. Settlement goes by `token_id` only, and the winner only from
`tokens[].winner`.** Outcome labels are not Yes/No: crypto pairs use Up/Down,
totals use Over/Under, sports use team names. Settling by label books every win
as a total loss. A sale at 0.99 is not a resolution — that is how the
predecessor fabricated 3,659 false resolutions.

**2. A position's key is `(wallet, token_id, mode)`.** The mode is switched on
open positions; without it in the key, paper and real lots would add up into one
row.

**3. The dedup key is `(tx_hash, wallet, token_id, side)`.** A log index does not
exist in the RTDS frame, nor in `/trades`, nor in `/activity` (measured: 500
trades, 500 unique hashes). Price and size are not part of the key: a redelivery
with different rounding would pass as a new trade and double the stake.

**4. The fee is `rate × (p × (1 − p))^exponent`, taker only.** The `feeSchedule`
lives in Gamma (`{rate, exponent, takerOnly, rebateRate}`); a `takerFeeRate`
field does not exist, and `taker_base_fee` from the CLOB equals 1000 for every
market and is not a rate. `exponent` is 1 on every schedule seen. Applying the
rate to the price overstates by 2× at `p=0.5`; a flat rate overstates by 4×.

**5. Shadow pays the same fee.** Otherwise it is systematically better than live
by exactly that amount, and a comparison of the modes measures our own
undercount.

**6. Asks in `/book` come by descending price**, best last. Walking in array
order buys at the worst price in the book.

**7. Orders are `FAK` only.** `GTC` would leave us a maker — the opposite of
copying a taker entry.

**8. The submission response does not contain the filled size** — it is in
`get_order().size_matched`. The exchange does not return the fee at all.

**9. Sports timing comes from `game_start_time`, not `end_date`.**

**10. Nothing is ever sent to the RTDS socket** except the subscription frame and
a `Pong` on request. Measured: a keepalive cuts delivery by 2.5×.

**11. Health is measured by flow, not by whether the process is alive.** In the
predecessor both guards reported OK while the bot was blind for 6.5 hours.

**12. The killswitch stops live only.** Shadow is a measuring instrument; it does
not stop.

**13. A divergence in the ledger is never fixed silently** — only an alert.

**14. The settlement queue runs from old to new** with an attempt counter.

**15. An order's size is not rounded; it is snapped to the grid.** The exchange
requires the notional `price × size` to have no more than two decimal places and
the size no more than five. That makes the permitted sizes a progression whose
step depends on the price: at 0.021 the step is ten shares, at 0.48 it is 0.0625.
The minimum order is **$1 in money**; `minimum_order_size` from the CLOB equals 5
for every market and is not a minimum. All three rules are known only from the
exchange's refusals. **On an exit the minimum is not raised but refused**: a buy
can be topped up to a dollar, a sale cannot — that would be selling more than the
leader sold. The leader trims a position by a percentage and our share comes out
in cents: on 05.09.2026 shadow booked such exits for $0.44, $0.06 and $0.02.

**16. "Rejected" and "unknown" are different outcomes.** An order the exchange
accepted must never be retried: `get_order` shows zero filled for several seconds
after a match, and a retry buys a second position (measured 2026-09-04: $2.00
instead of $1.00). Only what the exchange did not accept is retried; an unknown
outcome is written with status `unknown` and waits for reconciliation.

**17. A fill's price comes from the trade feed**, not from `OrderInfo.price`,
which is the order's price. Measured: a limit of 0.021 against a fill at 0.0182 —
the cost and the fee were overstated by 15%. The trade appears in the feed
seconds after the fill, so the polling lasts longer than it seems it should.

**18. A resolved market has no order book.** `GET /book?token_id=` answers 404
"No orderbook exists for the requested token id", and the condition was exactly
what we used to fetch from there — the token → condition path breaks precisely
where settlement needs the `winner`. The fallback: Gamma
`?clob_token_ids=<t>&closed=true` returns the condition and the fee in one row.
**`closed=true` is mandatory**: without the flag Gamma does not show a closed
market at all and answers `[]`. An empty response means "ask differently", not
"there is no fee".

**19. The chain runs ahead of the CLOB.** An auto-payout lands in the account
before the CLOB raises `tokens[].winner` (measured 04.09: the money at 21:25 UTC,
the `winner` about two minutes later). A resolution is still decided only by the
`winner` (invariant 1); the window is closed by reconciliation, which attributes
a position with no tokens on chain to `awaiting_settlement` rather than to a
divergence.

**20. The manual stop lives in the database.** `/kill` has to work when NATS is
unreachable, and the killswitch used to be a `Mutex` inside the process that did
not survive a restart. The `controls` table; the health loop synchronises with it
every tick, and the operator's decision **overrides** the automatic reason:
otherwise the sequence "feed stalled → operator pressed /kill → feed came back"
would resume live trading by itself.

**21. A chat that is not ours gets nothing.** Not "access denied" but silence: a
reply confirms that the bot exists and controls something. An empty allowlist
means "nobody", not "everybody".

**22. Shadow does not touch the exchange in any branch.** The buy path branched
on mode, the exit path did not, and twelve paper wallets sent 81 signed sell
orders to the live CLOB; they were refused only because the account did not hold
those tokens. The exchange stub in the tests filled at our own limit without
looking at the book — such a stub hides not a detail but a whole branch. The order
shape is shared across both modes (`exit_request`); the only difference is how the
fill is obtained.

**23. The killswitch stops all of live, and a sale is a live order.** The check
stood only on the buy path: `/kill` halted entries and left exits trading.

**24. State that must survive a restart lives in the database.** The second
occurrence after the manual stop (invariant 20): shadow's virtual account was a
`Mutex` inside `App` and was taken from the config at startup — a restart gifted
it $14,854 and broke the series by which drawdown is measured. The account is
derived from what is already recorded (`equity::shadow_cash`) rather than
accumulated; as a bonus it now sees settlement payouts, which the in-memory
account never received.

**25. History is not a signal.** What is copied is what the wallet does **from
the moment it was assigned** (`wallets.created_at`). `/activity?user=&limit=20` on
a quiet wallet returns weeks, and on the first tick all of that looks like news:
69 `market_not_tradable` refusals and 28 copies of trades up to 3.4 days old, one
of which filled at 0.001 against the leader's 0.260. This is not an eighth skip
reason — there is no signal at all. The trade is still recorded, otherwise the
dedup would forget it and the next tick would bring the same history again. The
comparison is by the second: RTDS timestamps to the second, `created_at` has
microseconds.

**26. Tests do not publish to the production bus.** `garnet-tg` listens on the
same subjects as production, and a fixture published by a test goes to the
operator's chat: on 05.09.2026 they received "Resolved · LIVE · 0xsettled", one
message per test run, while not a single live trade had happened. The same mistake
as tests against the production database, and it is cured the same way — with a
namespace, not with care: `Bus::connect_in(url, ns)` and `Events::connect_in`. An
empty namespace is production and leaves the subject unchanged. Publishing to
`alert.*` or `position.settled` without a namespace is forbidden even from a test
about isolation.

**27. The price we saw is recorded on every verdict.** `app.rs` used
`book.best_ask().unwrap_or(1.0)`, and an empty book arrived in the statistics as a
slippage refusal: the "expensive" bucket filled with observations where there was
no price at all. A slippage threshold cannot be tuned from such a log at any
sample size. `Quote.best_ask` is now an `Option`, the absence of asks is
`market_not_tradable` (not a new reason but a correctly named existing one), and
`signals` stores `best_ask`, `best_bid` **and the ceiling on the refusal itself**:
the miss is a difference, and without both halves it does not exist. `NULL` in
`best_ask` means "that side of the book did not exist", not "the price was zero" —
substituting any number would merge two different outcomes again.

**28. Market metadata lives in the database, not only in the process cache.** The
`HttpMarkets` cache lives ten minutes and does not survive a restart, while a
resolved market loses its book (invariant 18) — that is, the token → condition
path breaks exactly where settlement needs the `condition_id`. `Detector::ingest`
writes a `markets` row on every encounter with a market. A resolution is **not
erased** from the row by a fresher response lacking it: the winner can be learned
once and forgotten any number of times.

**29. One leader order, one order of ours.** A taker order consumes as much of the
book as is standing there and arrives as that many frames — with **different**
`tx_hash` values, so dedup by hash (invariant 3) does not catch them and cannot.
Measured 06.09.2026: 43% of our buys were slices of an already-copied order (75%
for one wallet; 85% on production data with a 300 s window). The cost was measured
by a paired comparison within a position: the first order in a wave returns
**+7.4%**, the second and later **−15%**, a difference of **+23.6 pp** with an
interval of [3.1; 44.3] and a sign that held for six wallets out of eight. The wave
closes **by the leader's clock** (`leader_trades.ts_trade`): our clock would
measure delivery latency rather than their behaviour. A refusal from the exchange
also closes the wave — the decision has been acted upon. The skip reason is the
existing `duplicate`, which had never once been issued before 06.09.2026, so the
label is unambiguous. **On buys only**: on a sale each slice takes out its own
fraction of our position, and collapsing them would leave us short by exactly the
discarded slices. The window is `[copy] slice_window_secs`; zero disables it.

**30. The loss stop latches for the day and lives in the database.** Five of the
killswitch's trip reasons were mechanical — the feed, a run of exchange failures,
the balance floor, a failed write, the manual stop — and not one was about money: a
bot that merely loses did not stop on its own. Two details are not obvious and both
were paid for by earlier defects. The stop **does not lift on a day that
recovered**: otherwise the first winning position resumes trading on the day it was
decided to stop. And the latch lives **in `controls`** rather than in memory
(invariants 20, 24): a restart after a bad day is the most likely event of that
day. It is computed from **closed** live positions: revaluing open ones depends on
the mid of the book, which some positions do not have at all, and a stop triggered
by a missing price is the worst kind of false alarm. `[risk]
daily_loss_limit_usd`; zero disables it, and `--preflight --live` calls zero a
refusal.

**31. Health distinguishes the circuits.** `FeedClock` counted only socket frames
and was right, while the operator's `/health` read `max(ts_seen)` across the whole
table — and on 06.09.2026, on a cold start, it printed "ok" while the socket
brought nothing for six minutes: a fresh safety-net poll counted as flow in
general. The poll runs every few seconds and always looks alive, so it must not
close the question about the socket, which is the reason health is measured by flow
at all (invariant 11). Two counters, two lines.

**32. The exposure ceiling is computed per event and does not trim the stake.**
The outcomes of one condition are correlated, and a per-token ceiling is bypassed
by buying the neighbouring outcome — so open value is summed by `condition_id`
through `markets`. A remainder smaller than the stake is a **refusal**, not a
reduced order: the part that fits is half of the leader's decision, not a decision.
Zero disables it: a fresh installation that refuses its very first signal is the
most expensive failure in this project. It is set **against concentration, not for
returns**: the counterfactual of 06.09 showed that as a way to raise ROI a ceiling
works only insofar as it accidentally trims slices of one order — and that is what
the slice window does (invariant 29), deliberately. `[risk] per_market_cap_usd`.

**33. A slippage threshold is moved by an instrument, not by guesswork.**
`/slippage` with no arguments shows the distribution of the miss
(`best_ask / leader's price − 1`) with the number taken, the number skipped and
**the result of closed positions per bucket** — the only thing that answers "are
expensive fills losing money". The tail is truncated by the active threshold: no
observations more expensive than it exist, because they were refused, which is why
the skips are shown alongside.

**34. An order can exist with no leader decision behind it.** A time-based exit is
our own, and `orders.signal_id` for it is `NULL` rather than an invented reference:
a synthetic signal would record, in the registry of the leader's decisions,
something they never did. The price is anchored to the **best bid** rather than to
the price of an old leader trade — aiming at a price that left the market long ago
means not exiting at all. The killswitch stops this exit too. The rule is **about
capital, not about returns**: on a prediction market the position will reach zero
or one anyway, and dumping into a thin book is usually worse than waiting. `[copy]
max_hold_hours`; zero does not start the loop at all.

**35. An exit fraction that does not reach the minimum accumulates rather than
disappearing.** The leader trims a position by percentages and our share comes out
in cents: at a $25 stake **63% of exit attempts** hit the $1 minimum order size
(invariant 15); at $10 it would have been almost a hundred. A refusal on each means
we copy the leader's entry and **do not copy their exit** — that is, we trade a
different strategy from the one we measure. It accumulates **in shares**
(`positions.pending_exit_shares`) rather than in fractions: a fraction is computed
against the current size, that size changes with additional buys, and a deferred
"five percent" would mean a different number of shares an hour later. More than we
hold can never accumulate.

The holding-time threshold was measured and is **bimodal**: of 571 positions in the
archive, 272 resolved with a median of 2.8 h, while 299 were still alive at the time
of the cleanup with a median age of 26.7 h. The first mode is visible in the closed
ones, the second was cut off by the observation window — 37 hours, beyond which
there is no data. Hence `max_hold_hours = 72`: safely above the fast ones (whose
observed maximum is 29.7 h), with the slow ones to be recomputed a week later.

> Numbers 36–39 were reserved for mechanisms that are not implemented. They are
> deliberately left as a gap: existing comments reference the numbers, and
> renumbering silently would break those references.

**40. A leader exits by more than selling.** Merging a pair is an exit at $1 on
both legs, and it arrives in no trade frame at all: `MERGE`, `SPLIT` and `REDEEM`
come through the same feed in the `type` field and were discarded by
`parse_activity` from 03.09.2026. That is not a filter, it is a blind spot: a
position whose leader merged out was held by us until resolution with a
`leader_observed_size` that no longer meant anything.

**The two circuits label a row with different fields.** `/activity` puts the kind
of event in `type` and leaves `side` empty on non-trades; an RTDS frame has no
`type` field on a row at all — there the kind sits directly in `side`, next to `BUY`
and `SELL` (`tests/fixtures/rtds_batch.json`: `"side": "MERGE"`). Parsing by `type`
alone would bring down the whole socket. Hence also the answer to a question the
plan left open: **the socket does carry merges.**

A leader's merge sells **the same fraction on each leg** through the shared
`exit_at_bid` path — at the best bid, with the order's `signal_id` as `NULL`
(invariant 34). A split is **not copied** by default: it buys a pair for $1, that
is, it is not a bet on an outcome but a placement of capital, and copying it with a
`stake_usd` stake would mean betting twice; whether to copy it is a decision of the
operator, not a refactor. A leader's redemption does not touch our position: our own
settlement closes it, with its own source of truth.

It is stored in a **separate** table, `leader_actions`, rather than in
`leader_trades.kind` with `side IS NULL`: in Postgres NULL in a UNIQUE constraint is
not equal to itself, and the key `(tx_hash, wallet, token_id, side)` would stop
catching repeats — one merge delivered by two circuits would sell the position
twice. `handled_at` is kept separate from `ts_seen`: seeing and acting are different
events, and a merge seen with an empty book or under a stop has to stay visible as
an alarm in `/health` rather than look done.

**41. The third circuit is not a third source of truth.** Polygon logs do not
depend on Polymarket's infrastructure at all: on 31.08.2026 the `activity/trades`
topic went down platform-wide for hours, from every IP at once, and the only backup
was the slow poll. The log carries the same `transactionHash` as the socket frame,
so it collapses on the **existing** key `(tx_hash, wallet, token_id, side)`;
`source` gains a third value, `chain`, and the circuit is a third delivery of one
truth rather than a third truth.

**More reliable than the socket, but not faster.** A log appears once the settlement
transaction is in a block, and the order was matched by the CLOB before that. No
claim about "getting ahead of the leader" follows from this. Whether the circuit
pays for itself is answered by `/sources` (invariant 43): the share of unique trades
is precisely its value.

**The decoding was written from the verified V2 ABI, not carried over.** The topic
of the V1 signature (five data words, `makerAssetId`/`takerAssetId`, the side
inferred from a zero `assetId`) yields **not one** log on the V2 exchange, while the
exchange emits 45,911 over 500 blocks. V2 has seven words and the side is an
**explicit field**, `side: uint8`.

**The filter goes on `topics[2]` (the maker), and a second filter is not needed.**
The aggressor has an `OrderFilled` of its own where it is the maker and `taker` is
the exchange's own address: of the 1877 addresses encountered in `topics[3]`, 1876
also appear in `topics[2]`. The filtering happens **on the node's side** — that is
the point of the circuit; without it we would have to accept the platform's entire
feed (7255 fills over 120 blocks).

**A log carries no time, and taking it from our clock is not allowed.** `ts_trade`
is the leader's time, by which the slice window closes (invariant 29). The timestamp
comes from the block and is cached; a node failure is **not** substituted with our
clock — a trade with an invented time would pass the assignment threshold by the
wrong clock and would look real.

A log removed by a reorganisation (`removed: true`) is not a trade: copying it means
buying something that no longer exists on chain. The silence watchdog here is five
minutes rather than forty-five seconds as with RTDS: there silence means a dead
subscription, here it means **the leaders were not trading**. `[feed]
polygon_ws_url`; an empty string disables the circuit entirely.

**42. On the hot path there are no sequential waits that could run in parallel.**
Every such wait is a network round trip added to the latency by which the whole point
of copying is measured.

`market` and `book` waited for each other for no reason: `book` does not use `meta`.
The four database calls in `handle_buy` — the leader observation, reading the wave,
open exposure and the balance — also ran in turn, though none uses another's result.
Both now go through `try_join!`. The order of **writes** does not change: `signals`
is written after all the reads, `orders` after `signals`, and an order with no
decision behind it remains impossible.

**There was no instrument for this.** Of the four stages declared in
`metrics::STAGES`, two were written and the fourth was written as a **constant
zero**: the histogram counted observations and carried not one number, and a
distribution of zeros is indistinguishable from an instantaneous path. Speeding up a
path there is nothing to measure is exactly what "building an improvement on faith"
means.

Latency is computed **from the database, not from process metrics**: those live in
its memory, neither the bot nor the operator sees them after a restart, and "before
and after" has to be compared precisely across a restart. The end-to-end figure
`trade_to_submitted` is measured **per trade** rather than by adding per-stage
medians: the median of a sum does not equal the sum of medians, and the added-up
number would resemble the truth without being it. Execution is not included —
submission is where it ends. Negative differences are clamped to zero: the clocks of
the database and the exchange drift apart, and a negative latency is a clock mismatch
rather than "faster than instantaneous". `/latency`, `--latency`.

**What this does not give.** None of these steps makes us faster than the leader. We
copy what has already executed; the point is not to lose seconds on top of that.

**43. A losing delivery is an observation, not waste.** The dedup discards the second
copy of a trade correctly, but the only evidence about a circuit's speed was
discarded along with it: `insert_new` did `ON CONFLICT DO NOTHING`, returned `None`,
and the second copy vanished without trace. A circuit whose value cannot be measured
can neither be switched off nor defended — and the safety-net poll costs us backfill
duplicates and 69 false `market_not_tradable` refusals.

`leader_trades.source` holds the **winner**; `trade_sightings` holds all of them, the
winner included: a circuit's lag is computed as the difference between its `ts_seen`
and the minimum for that trade, and without the winner's row there is no minimum. The
key is `(leader_trade_id, source)`: a redelivery by the same circuit adds no sighting
— the poll brings the same trade back twenty times in a row, and counting that as
twenty sightings would declare it twenty times more useful than it is.

This is answered by `/sources` and `--sources [day|week|all]`. The main column is
"brought by it alone": zero means the circuit catches nothing that would not be caught
without it. But **zero for every circuit at once** means something else — they
duplicate each other, and any one of them can be switched off, not all of them; naming
the first on the list in that case would blame a circuit for its place in the alphabet.

**44. The entry queue is limited by time, not by market.** The slice window (29)
removes slices of one order, the ceiling (32) removes concentration within one event. A
leader who fired twelve decisions within a minute across twelve markets passes **both**
and brings 94% of the loss: the bucket "11+ entries per wallet at once" — 24 positions,
half the capital deployed, ROI −15.74% against +3.48% for the "1 entry" bucket (244
closed, 06.09.2026).

The window closes **by our clock**, not by the leader's. That is the difference from
invariant 29: there we interpret their behaviour — one order or separate decisions — and
measuring that by our clock would mean measuring our own delivery latency. Here what is
limited is **our own rate of spending capital**, and that is measured in our time. The
window's boundary is **exclusive**: an inclusive one would make the window a second
longer than advertised, and a threshold set from a measurement would mean something
other than what was measured.

It is asked **last** of all the refusal reasons, and only of what would otherwise
become an entry. The reason is the same as for judging the ceiling last: an order that
would not have been taken on price or on funds anyway must neither report itself as
refused by the rate limit nor take a place in the queue — otherwise the queue would
consist of refusals. A refused entry does not enter the queue: otherwise one burst
would lock a wallet for a whole window, counting its own refusals as expenditure.

`skip:rate_limited` is the **seventh** skip reason, and it appeared by an operator's
decision rather than by a refactor: the same route the sixth took on 06.09.2026.

**`fire_limit = 0` is the default, and that is not a stub.** A threshold comes from a
measurement, not from a guess (invariant 33), and no measurement on Garnet's own data
exists. So **the counting happens even while the refusal is disabled**, and `/firerate`
and `--firerate` show the distribution of depths and the **counterfactual**: how much
each limit would have refused over history that has already happened. The
counterfactual is computed by replaying the history through the limiter itself rather
than by the formula "depth greater than the limit": a refused entry does not enter the
queue and does not deepen the ones that follow, so counting by depths systematically
overstates the refusals. A disabled limiter that counts nothing would leave the
operator exactly where they were — with a threshold there is nowhere to get.

**45. The watcher does not live inside the process it watches.** Five background loops
live inside `garnet-core`: a runtime hang, a deadlock on a `Mutex` or an exhausted
connection pool take out **both the trading and the observation of trading** at once —
and the first thing to disappear is the one that was supposed to say so. Invariant 11
says health is measured by flow rather than by whether the process is alive; but it is
the process itself doing the measuring.

`garnet-watch` is a separate binary, a separate unit, a separate exit code. It does
**not restart or stop** the trading process and does not write to the trading tables at
all: a watcher that can repair what it watches will sooner or later repair it wrongly,
and by then there will be nobody left to explain the divergence.

**Three exit codes, not two.** `0` agreed, `1` a divergence, `2` could not be checked.
The third exists because "could not" is not "agreed" (invariant 47). A divergence is
louder than an unknown: both are printed, but the unknown is raised only when there are
no divergences at all. In the unit, `SuccessExitStatus=1 2` — otherwise systemd would
mark every problem it **found** in red as a breakage of the watcher itself, and there
would be nothing left to distinguish "it broke" from "it found something".

**Silence in the trades is not a divergence.** The leaders do not trade around the
clock, and a watcher that shouts at a quiet night is the first to stop being read —
before it says anything important. It raises an alarm only while wallets are enabled,
and "there were no trades at all" is `Unknown` rather than a divergence: a fresh
installation is not broken. That question is answered by the **heartbeat**
(`controls.heartbeat`), and only by it: the age of the trades speaks about the leaders,
the age of the snapshots about the loop, the heartbeat about the process.

**46. An irreversible action requires a phrase, not a button.** A button is pressed by
accident, a phrase is not. `/kill` halts trading and **leaves** the positions; a "close
everything now" path did not exist at all, and at the moment it was needed it would have
been done by hand through the exchange — under pressure, without a trace and without
checks.

The three modes differ not in force but in what each one gives up. `graceful` sells
nothing. `hybrid` sells only what is in profit — that is, it **deliberately gives up the
expected payout** on winning positions, and that is confirmed by a separate field rather
than inferred from the choice of mode: inferring consent from the mode means not asking
for it at all. `panic` sells everything and is rejected while trading is running —
closing everything while continuing to buy is not a halt but a swap.

The intent lives for a minute and sits in `controls` (invariant 20): between "decided"
and "confirmed" the process may restart. A timestamp **from the future** is checked
before expiry — its age is negative, and the expiry check would pass it as fresh, so an
intent marked with tomorrow's date would never expire. Any refusal clears the intent: a
second attempt is a second decision.

The installation's name in the second half of the phrase (`[flatten] name`) is not
decoration: a phrase copied from documentation or from someone else's chat log must not
fire on this host.

**The only path that does not defer to the stop.** `panic` is permitted only while
trading is halted, and a killswitch forbidding it to sell would make the mode impossible
by construction: the operator would halt trading, type the phrase and sell nothing. The
phrase is stronger than the automatic stop here, because it is spoken after it and about
it.

**No mode touches paper positions.** There is nothing to save in them, and closing them
would destroy the only comparison shadow exists for. `hybrid` also leaves alone what it
cannot value: selling a position whose profitability is unknown means going beyond what
the operator agreed to (invariant 27).

**47. An incomplete read is not "agreed".** A reconciler that failed to read a balance
has to say "the check could not be performed" rather than stay silent: that is the same
mistake as `unwrap_or(1.0)` on an empty book (invariant 27) — a substitution merges two
different outcomes. Until 19.09.2026 a `?` on a chain read brought down the **whole
pass**: one unreadable token left all the others unchecked, and the report said nothing
about it. Now an unreadable token costs only itself, lands in `unreadable` and goes out
on its own subject, `alert.reconcile_unreadable` — deliberately separate from a
divergence: "the chain says something other than the ledger" and "the chain could not be
looked at" are different news.

A related defect was fixed in the same place: `push_text` matched known subjects and the
`_ => None` branch **silently swallowed** any new alert. It reached the bus, reached the
`alert.*` subscription — and died before being sent. An alert the operator never learned
about is worse than a missing one: the first creates confidence that all is quiet.

**48. A divergence within an order in flight is not a divergence.** Between submission
and fill the chain and the ledger diverge by construction, and a reconciler that shouts
about that shouts often; a reconciler that shouts often stops being read — and it is the
only mechanism that notices a real divergence. Four outcomes per position (`Agreed`,
`Explained`, `Unattributed`, `Unknown`), and exactly one of them is an alarm.

**The sign of the delta is not symmetric.** The chain holds more than the ledger — a buy
may have been taken on whose fill we failed to book; less — a sale may have gone out.
Explaining a shortfall by a buy in flight is not allowed: a buy brings tokens in, it does
not take them away.

**The tolerance scales with the position's size** (`0.1%`, but no less than `0.01`
shares): an absolute tolerance on a position of a hundred thousand shares is zero; on a
position of three shares it is everything.

"In flight" for us means status **`unknown`**: execution is IOC, the order does not rest
in the book, and `submitted` is written nowhere. An `unknown` row is created precisely so
that the reconciler will see it (migration 0002). It has an **expiry**, `[reconcile]
in_flight_window_secs`: nobody ever moves an `unknown` row to another status, and without
an expiry such a row would explain a genuine loss of tokens forever. Zero disables the
explanation entirely.

A premise that was missing: three places recorded a sale in flight as **zeros**
(`limit_price = 0`, `size_usd = 0`), that is, the order did not carry its own size and
explained nothing. A row that exists for the sake of future diagnostics is obliged to
carry the number those diagnostics are made from.

**49. The quality of copying and the quality of the leader are different quantities, and
a report has to separate them.** Absolute P&L answers "did we make money" and does not
answer "because of execution or because of the choice of wallet". The controls we have —
the slippage threshold, the slice window, the stake, the exposure ceiling — turn only the
former, and moving them by the absolute result means looking at the wrong instrument. The
gap is computed against the leader's average price **from the moment of assignment**
(invariant 25) and includes the fee (invariant 4); without the fee it measures our own
undercount rather than execution. An open position has no result: `our_pct` there is
`None`, not zero (invariant 27). `/matchup`, `--matchup [day|week|all]`.

**50. A refusal is also an observation about the quality of copying; somebody else's mode
is not.** A position the leader opened and we skipped on slippage does not enter the "our
percent against theirs" comparison, and the average gap over the positions taken flatters
us by exactly the discarded tail — the same truncated tail as in `/slippage` (invariant
33). The skips are shown on a separate line and do not enter the average. But **a skip is
our own refusal**: a leader trade on which no decision was taken in this mode at all (the
wallet was in another mode, was disabled, the engine was down) contains no decision of
ours, and counting it as a skip would inflate the tail by somebody else's mode. It is
counted separately (`n_unevaluated`). A `copy` verdict with no position behind it is a
third outcome: that is a refusal from the exchange or a fill that never arrived, and
merging it with a slippage refusal would make us turn a threshold where the threshold is
not the issue.

---

## The three detection circuits

All three write to one table and collapse on one dedup key. Which copy arrives
first does not matter.

| | Speed | Depends on | Silence means |
|---|---|---|---|
| **RTDS socket** | fastest | Polymarket's websocket | a dead subscription (watchdog: 45 s) |
| **`/activity` poll** | slow | Polymarket's REST | nothing; it is the safety net |
| **Polygon logs** | not faster than RTDS | your own node | the leaders were not trading (watchdog: 5 min) |

A third delivery of one truth, not a third truth. `/sources` says which of them
earns its keep: the column "brought by it alone" is the answer.

## The background loops

Five of them live inside `garnet-core`, each with its own timer and a shared stop
signal:

| Loop | Period | What it does |
|---|---|---|
| health | 30 s | computes the verdict, arms and clears the killswitch, applies the manual stop and the loss stop |
| settlement | 600 s / 3600 s | walks the queue oldest-first, records resolutions, announces them on the bus |
| equity | 300 s | a snapshot of both modes; live cash from the chain, shadow's from the ledger |
| reconciliation | 600 s | the ledger against the chain; only live positions |
| the `/activity` poll | 3 s | the safety-net circuit, and it re-reads the wallet list |

Plus, conditionally: the time-based exit (only when `max_hold_hours > 0`), the
retry of deferred exits (always, every two minutes), the emergency-close executor
(always) and the heartbeat (always, and first).

`garnet-watch` is deliberately **not** among them — see invariant 45.

## The database

Thirteen tables. Everything the predecessor had beyond this existed for the sake
of wallet selection, and for disk growth.

| Table | Holds |
|---|---|
| `wallets` | the registry: mode, stake, slippage, enabled |
| `wallet_events` | the audit trail of every change, with the actor |
| `markets` | what we learned about a token, outliving the process (invariant 28) |
| `leader_trades` | the trades we saw, with the winning circuit in `source` |
| `trade_sightings` | every delivery, the winner included (invariant 43) |
| `leader_actions` | merges, splits, redemptions (invariant 40) |
| `positions` | keyed `(wallet, token_id, mode)`, plus the deferred exit |
| `signals` | every decision, refusals included, with the prices we saw |
| `orders` | what was submitted and what came of it |
| `fills` | the fills, with the fee |
| `settlements` | resolutions and payouts |
| `equity_snapshots` | the series drawdown is measured from |
| `controls` | the manual stop, the loss latch, the flatten intent, the heartbeat |

## Development

```bash
docker compose up -d                 # postgres 5433, redis 6380, nats 4223
cargo test --workspace --no-fail-fast
```

**`--no-fail-fast` is not optional.** Every database test gets its own Postgres
schema (`garnet_db::testing`), because settlement, reconciliation and equity
snapshots are global in meaning and in a shared schema would change each other's
rows. A suite that aborts halfway leaves those schemas behind, and the next run
fails on `CREATE SCHEMA` instead of on anything real. `cargo test` also **stops at
the first failing binary**, so one flake hides the rest: the first run of 18.09
reported 41 tests out of 535.

**Schema cleanup removes no more than twenty per call.** Without a limit it tried
to drop everything accumulated on every call to `isolated()`: by 19.09.2026 that
was 2411 schemas, that is, over two thousand `DROP SCHEMA CASCADE` statements per
test, with a dozen tests in parallel. Under that load the run started failing in
whole suites — and failing not where it was broken but where it timed out.
Schemas accumulate more slowly than they are removed: one run creates fewer than
two hundred.

**Tests go to a separate database.** `TEST_DATABASE_URL` defaults to
`garnet_test`, because on 2026-09-04 the default pointed at the trading database
and a test run added eleven wallets to the production registry — one of them in
live mode — plus 1880 test schemas, enough that `pg_dump` failed on a shortage of
locks. A forgotten environment variable must drop the tests somewhere there is
nothing to break.

**Fixtures must contain** Up/Down, Over/Under, team names, and an RTDS frame with
empty metadata (7.8% of the real stream).

### Known flakes

Two, both against live infrastructure, both **undiagnosed**:

- a rare failure when schemas are created in parallel (probably a race with
  sqlx's migration advisory lock): `schema "t_…" already exists`, and
  `--test-threads=1` gives a clean run;
- `garnet-bus` `roundtrip.rs::an_empty_namespace_leaves_the_subject_alone` — the
  message fails to reach the subscription in roughly one run out of five.

Measured 06.09.2026: waiting does not cure it (twenty seconds help exactly as much
as three), `--test-threads=1` gives 6 clean runs out of 6, and a shared `Mutex`
over the file's tests still gives 1 failure out of 8. The timeout and lock changes
were **reverted**: both treated the symptom. On a failure, just re-run. A run where
only these fail counts as clean; anything else failing is not a flake.

### Conventions

Comments are in English and explain **why**, not what. Money is
`rust_decimal::Decimal`, never `f64`. Dates in comments are `DD.MM.YYYY`. See
[CONTRIBUTING.md](../CONTRIBUTING.md).

## Commands

```
--preflight [--live]   thirteen probes, one line per dependency
--inject …  --yes      a hand-made leader trade through the whole pipeline
--settle               a manual settlement pass
--reconcile            the ledger against the chain
--trades               our own trade feed (diagnostics)
--matchup [period]     copy quality: our half against the leader's
--sources [period]     the race between delivery circuits
--firerate [--window]  entry queue depth and the threshold's counterfactual
--latency [period]     latency per stage of the signal path
--flatten <mode>       the emergency close (requires the phrase)
--redeem …  --yes      a manual redemption
--migrate-only         apply the migrations (requires a rebuild!)
```

## The account

The reference deployment is a Gnosis Safe (`POLY_GNOSIS_SAFE`) with auto-payout
enabled: the outcome tokens sit on the proxy, Polymarket's relay pays the gas, and
winnings are credited without us. That is why `ChainRedeemer` does not run and the
default redeemer is `AutoPayout` — what gets verified is not our redemption but the
agreement of the credited payout with our ledger.

This matters for preflight: on such an account the gas and allowance checks speak
about the proxy, not about our EOA, and zero MATIC there is normal rather than
broken.

## Settlement, verified with money

Two smoke positions resolved on 04.09.2026 and both won. The platform credited the
payout itself and the ledger agreed with the balance to the cent:

| Position | Stake + fee | Payout | P&L |
|---|---|---|---|
| Ipswich No 3.037030 (neg-risk) | 2.483448 | 3.037030 | +0.55 |
| Betis Yes 14.285712 (neg-risk) | 2.085978 | 14.285712 | +12.20 |

pUSD 6.351095 → 9.388125 → **23.673837**; both steps equal the payout exactly.
`settlements` holds both rows, the positions are closed, and reconciliation against
the chain found zero divergences.

The first pass reported "resolved 0" and found the seventh execution defect: the
book of a resolved market disappears (invariant 18) while `settle_once` swallowed
the error silently — nine attempts of silence with the payout already credited. The
cause now reaches the report (`SettleReport::failed`), per invariant 13.

All three smoke positions resolved and closed for a total of **+$12.28**: Ipswich No
+0.55, Betis Yes +12.20, SPX Up −0.47 (Down won).
