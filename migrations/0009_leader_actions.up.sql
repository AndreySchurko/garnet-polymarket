-- Leader actions that are not trades but do change the position.
--
-- Invariant 40: a leader exits by more than selling. Merging a pair is an exit
-- at $1 on both legs, and it arrives in no trade frame at all: neither RTDS nor
-- `/activity` reports it as a TRADE. A position whose leader merged out, without
-- us seeing it, was held by us until resolution with a `leader_observed_size`
-- that no longer meant anything.
--
-- A SEPARATE table, rather than `leader_trades.kind` with `side IS NULL`. The
-- reason is mechanical: in Postgres, NULL in a UNIQUE constraint is not equal to
-- itself, so the key `(tx_hash, wallet, token_id, side)` with a NULL side would
-- stop catching repeats — the same merge, delivered twice, would be inserted
-- twice and would sell our position twice. The dedup key has to stay functional;
-- here it stays functional for both tables.
--
-- The key is the condition, not the token: a merge burns both legs at once, and
-- it does not have one token.

CREATE TABLE leader_actions (
  id           BIGSERIAL PRIMARY KEY,
  wallet       TEXT NOT NULL REFERENCES wallets(address) ON DELETE CASCADE,
  tx_hash      TEXT NOT NULL,
  condition_id TEXT NOT NULL,
  kind         TEXT NOT NULL CHECK (kind IN ('merge','split','redeem')),
  size         NUMERIC(18,6) NOT NULL,
  ts_action    TIMESTAMPTZ NOT NULL,
  ts_seen      TIMESTAMPTZ NOT NULL DEFAULT now(),
  source       TEXT NOT NULL CHECK (source IN ('rtds','poll')),
  -- When we acted on it. For a merge, the moment we exited the legs; for a
  -- split and a redeem it stays NULL forever, because there is nothing to act
  -- on. Kept separate from `ts_seen`: seeing and acting are different events,
  -- and merging them loses the merge that was seen and not acted upon.
  handled_at   TIMESTAMPTZ,
  UNIQUE (tx_hash, wallet, condition_id, kind)
);

CREATE INDEX ON leader_actions (wallet, ts_action DESC);
