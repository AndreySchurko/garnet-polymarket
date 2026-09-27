-- Circuit sightings: who saw the same on-chain trade, and when.
--
-- Invariant 43: a losing delivery is an observation, not waste. Dedup discards
-- the second copy of a trade correctly, but the only evidence about a circuit's
-- speed was discarded along with it: `insert_new` did `ON CONFLICT DO NOTHING`
-- and returned `None`, after which the second copy vanished without trace. A
-- circuit whose value cannot be measured can neither be switched off nor
-- defended: the `/activity` poll costs us duplicates from backfill and 69 false
-- `market_not_tradable` refusals, and whether it pays for itself is unknown.
--
-- `leader_trades.source` holds the WINNER. Here are all of them, the winner
-- included: a circuit's lag is computed as the difference between its `ts_seen`
-- and the minimum for that trade, and without the winner's row there is no
-- minimum.

CREATE TABLE trade_sightings (
  id              BIGSERIAL PRIMARY KEY,
  leader_trade_id BIGINT NOT NULL REFERENCES leader_trades(id) ON DELETE CASCADE,
  -- Nobody writes `chain` yet: the third circuit (Polygon logs) comes later.
  -- The value is permitted in advance so that its arrival does not require a
  -- migration at exactly the moment the circuit is being debugged.
  source          TEXT NOT NULL CHECK (source IN ('rtds','poll','chain')),
  ts_seen         TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- A redelivery by the same circuit is not an observation: the poll runs
  -- every few seconds and brings the same trade back twenty times in a row.
  -- Counting that as twenty observations would declare the poll twenty times
  -- more useful than it is.
  UNIQUE (leader_trade_id, source)
);

CREATE INDEX ON trade_sightings (leader_trade_id);
