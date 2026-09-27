# Port audit: what came from the predecessor

Written 2026-09-03 against the predecessor at commit `03e5ada`. This is the
record of what was carried over from the previous generation, what was not, and
why — kept because "we already have that code" is the most expensive assumption
in this project.

## The state at the time of the audit

**Nothing had been carried over from the predecessor.** Everything that existed
here at that point (`garnet-db`, `garnet-core`, `garnet-copy-engine`, 53 tests) had
been written from scratch. The porting was deliberately scheduled **after** the
core passed its tests against stubs, and each port was closed by a test against the
real shape of an API response.

## What happened

- ✅ **`garnet-bus`** carried over whole, with a `subjects` module added for the
  event names. The roundtrip was verified against a live NATS.
- ✅ **`garnet-redis`** carried over trimmed: `egress_bucket` in full, `dedup_key`
  rewritten for the new key. `active_set` and `bankroll` were discarded as the
  predecessor's scoring concepts.
- ✅ **`garnet-clob`** carried over without `orderbook_ws` and `heartbeat` (2,071
  lines of the maker track) and **with one change, for the order type**: the SDK
  supports `FAK` ("fill what you can immediately, cancel the rest"), and the type
  now travels through `traits` → `client` → `sdk` → the request body. The crate's
  96 tests are green, and the assertions about `GTC` were rewritten for `FAK`. On
  the way it turned out that the dependency on `garnet-types` amounted to two
  order-book structs, and the one on `garnet-config` to three fields: both now
  live inside the crate itself.
- ✅ **The `ClobExec` adapter** — `crates/garnet-bin/src/clob_adapter.rs`.
- ✅ `garnet-blockchain` — redemption, USDC.e, balances.
- ✅ `garnet-feed/ws.rs` — the live socket.

### What the porting exposed

**The order submission response does not contain the filled size.**
`OrderResponse` carries the requested size; the filled one exists only in
`get_order().size_matched`, which is why the adapter reads the order back after
submitting. Had we trusted the response, a position would have been recorded at
full size on a partial fill.

**The exchange does not return the fee on a live fill.** The caller has to compute
it from the market's schedule — otherwise live would look better than shadow by
exactly that amount, and the comparison between the modes would be measuring our
own undercount rather than the quality of execution. Fixed in `App::priced`.

## The decisions

| Predecessor crate | Lines | Decision | Why |
|---|---|---|---|
| `garnet-clob` | 4,700 | **Carry over with a change** | The only implementation of the REST CLOB v2, its signatures and order types. The change was mandatory: the `place_limit_order` trait takes no order type, meaning it can only do GTC — a maker leg. IOC/FAK was needed. **Not carried over**: `orderbook_ws.rs` (1,393) and `heartbeat.rs` (678), which served a maker track Garnet does not have |
| `garnet-blockchain` | 2,547 | **Carry over almost whole** | Redemption, contract addresses (USDC.e, the neg-risk adapter, CTF), EIP-712 signing in `order_signer.rs`, gas, RPC failover. Without it a winning position does not turn into money |
| `garnet-feed` → `ws.rs` | 802 | **Carry over** | The RTDS socket loop: subscription, reconnection, silence detection. This is where the rule "send nothing to the socket" lives |
| `garnet-feed` → `parse.rs` | 286 | **Do not carry over** | Already rewritten in `garnet-core/src/detect.rs` (about 80 lines): the predecessor's version drags in its own types and parses fields that cannot be trusted in 7.8% of frames |
| `garnet-feed` → `active_set.rs` | 242 | **Do not carry over** | The set of active markets was for the maker |
| `garnet-bus` | 129 | **Carry over as is** | A thin wrapper over `async-nats`, exactly what is needed |
| `garnet-redis` | 747 | **Carry over trimmed** | `egress_bucket.rs` is a distributed limiter against the API; Garnet has no second consumer, but the limit is needed, especially when a scanner competes with trading |
| `garnet-types` → `fees.rs` | — | **Do not carry over; verify against it** | Our `market_meta::taker_fee` already matches in form. What we take from the predecessor is a measurement across 153 markets for the documentation: `exponent` equals 1 on every schedule |
| `garnet-types` → `decimal.rs`, `primitives.rs`, `subjects.rs` | — | **Take in pieces** | Decimal utilities, addresses and tokens, NATS subject names |
| `garnet-types` → `copy.rs`, `risk.rs`, `events.rs`, `market.rs` | — | **Do not carry over** | the predecessor's event and risk types; Garnet has its own model |
| `garnet-executor` → `live.rs`, `booking.rs`, `supervisor.rs` | 1,636 | **Do not carry over** | This is the predecessor's accounting and supervision, which Garnet had already replaced with its own `execute.rs`, `positions` and `settle.rs`. Signing lives in `garnet-clob`, not here |
| `garnet-executor` → `reconcile.rs` | 203 | **Take as a model** | Reconciling positions on chain against the database is a function worth having, but it is rewritten for the new schema |
| `garnet-executor` → `simulator.rs` | 185 | **Do not carry over** | the predecessor's simulator is replaced by the book walk in `shadow.rs` |
| `garnet-config` | 2,496 | **Write from scratch** | the predecessor's config consists of knobs for scoring, screening, buckets and benches |
| `garnet-risk` | 3,132 | **Write from scratch** | Keep the killswitch, the feed watchdog and feed lag; `exposure`, `circuit_breaker` and `triggers` are indirect filters |
| `garnet-db` | 3,607 | **Written from scratch** | the predecessor's schema encoded the selection |

## Never carried over

Scoring, screening, benches, buckets, `metrics.rs`, `portfolio.rs`,
`closed_positions`, `wallet_scores`, `market_price_points`, `trades_raw`.

That is 90% of the predecessor's disk and 100% of the selection machinery Garnet
gave up.
