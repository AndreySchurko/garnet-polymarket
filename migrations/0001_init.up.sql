-- Garnet, the only initial migration: 10 tables.
-- Everything the predecessor had beyond this existed for the sake of wallet
-- selection, and for disk growth.

CREATE TYPE mode AS ENUM ('live', 'shadow');

CREATE TABLE wallets (
  address           TEXT PRIMARY KEY,
  nickname          TEXT,
  mode              mode NOT NULL DEFAULT 'shadow',
  stake_usd         NUMERIC(18,6) NOT NULL DEFAULT 0,
  max_slippage_pct  NUMERIC(6,4) NOT NULL DEFAULT 0.15,
  enabled           BOOLEAN NOT NULL DEFAULT FALSE,
  created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE wallet_events (
  id         BIGSERIAL PRIMARY KEY,
  wallet     TEXT NOT NULL REFERENCES wallets(address) ON DELETE CASCADE,
  field      TEXT NOT NULL,
  old_value  TEXT NOT NULL,
  new_value  TEXT NOT NULL,
  actor      TEXT NOT NULL,
  ts         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE markets (
  token_id         TEXT PRIMARY KEY,
  condition_id     TEXT NOT NULL,
  question         TEXT NOT NULL,
  outcome_label    TEXT NOT NULL,          -- Up / Over 2.5 / Lakers, NOT Yes/No
  category         TEXT,
  game_start_time  TIMESTAMPTZ,            -- sports timing comes from here, not from end_date
  end_date         TIMESTAMPTZ,
  neg_risk         BOOLEAN NOT NULL DEFAULT FALSE,
  fee_rate         NUMERIC(8,6) NOT NULL DEFAULT 0,  -- feeSchedule.rate, from Gamma
  fee_exponent     NUMERIC(4,2) NOT NULL DEFAULT 1,  -- feeSchedule.exponent
  fee_taker_only   BOOLEAN NOT NULL DEFAULT TRUE,    -- feeSchedule.takerOnly
  resolved_outcome TEXT,                   -- only ever from tokens[].winner
  closed           BOOLEAN NOT NULL DEFAULT FALSE,
  updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE leader_trades (
  id            BIGSERIAL PRIMARY KEY,
  wallet        TEXT NOT NULL REFERENCES wallets(address) ON DELETE CASCADE,
  tx_hash       TEXT NOT NULL,
  token_id      TEXT NOT NULL,
  side          TEXT NOT NULL CHECK (side IN ('buy','sell')),
  price         NUMERIC(8,6) NOT NULL,
  size          NUMERIC(18,6) NOT NULL,
  ts_trade      TIMESTAMPTZ NOT NULL,
  ts_seen       TIMESTAMPTZ NOT NULL DEFAULT now(),
  source        TEXT NOT NULL CHECK (source IN ('rtds','poll')),
  market_text   TEXT NOT NULL,             -- a snapshot taken at the moment of the event
  outcome_text  TEXT NOT NULL,
  -- The one and only dedup: a single on-chain trade seen by two circuits.
  -- A log index does not exist in the RTDS frame, nor in /trades, nor in
  -- /activity; a measurement over 500 live trades gave 500 unique
  -- transactionHash values and zero collisions on this key. Price and size are
  -- NOT part of the key: otherwise a redelivery of the same trade with
  -- different rounding would pass as a new one and double the stake.
  UNIQUE (tx_hash, wallet, token_id, side)
);

CREATE TABLE positions (
  id                   BIGSERIAL PRIMARY KEY,
  wallet               TEXT NOT NULL REFERENCES wallets(address) ON DELETE CASCADE,
  token_id             TEXT NOT NULL,
  mode                 mode NOT NULL,
  size_bought          NUMERIC(18,6) NOT NULL DEFAULT 0,
  size_sold            NUMERIC(18,6) NOT NULL DEFAULT 0,
  cost_usd             NUMERIC(18,6) NOT NULL DEFAULT 0,
  proceeds_usd         NUMERIC(18,6) NOT NULL DEFAULT 0,
  fees_usd             NUMERIC(18,6) NOT NULL DEFAULT 0,
  leader_observed_size NUMERIC(18,6) NOT NULL DEFAULT 0,
  attempts             INTEGER NOT NULL DEFAULT 0,
  last_attempt_at      TIMESTAMPTZ,
  opened_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
  closed_at            TIMESTAMPTZ,
  UNIQUE (wallet, token_id, mode)          -- mode IS in the key: paper and real never mix
);

CREATE TABLE signals (
  id              BIGSERIAL PRIMARY KEY,
  leader_trade_id BIGINT NOT NULL REFERENCES leader_trades(id) ON DELETE CASCADE,
  wallet          TEXT NOT NULL REFERENCES wallets(address) ON DELETE CASCADE,
  mode            mode NOT NULL,
  verdict         TEXT NOT NULL,            -- 'copy' | 'skip:<one of the named reasons>'
  target_size_usd NUMERIC(18,6) NOT NULL,
  limit_price     NUMERIC(8,6),
  ts_signal       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE orders (
  id           BIGSERIAL PRIMARY KEY,
  signal_id    BIGINT NOT NULL REFERENCES signals(id) ON DELETE CASCADE,
  token_id     TEXT NOT NULL,
  mode         mode NOT NULL,
  side         TEXT NOT NULL CHECK (side IN ('buy','sell')),
  limit_price  NUMERIC(8,6) NOT NULL,
  size_usd     NUMERIC(18,6) NOT NULL,
  status       TEXT NOT NULL CHECK (status IN ('submitted','filled','partial','rejected')),
  tx_hash      TEXT,
  error        TEXT,
  attempts     INTEGER NOT NULL DEFAULT 1,
  ts_submitted TIMESTAMPTZ NOT NULL DEFAULT now(),
  ts_filled    TIMESTAMPTZ
);

CREATE TABLE fills (
  id         BIGSERIAL PRIMARY KEY,
  order_id   BIGINT NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
  mode       mode NOT NULL,
  source     TEXT NOT NULL CHECK (source IN ('clob','book_walk')),
  size       NUMERIC(18,6) NOT NULL,
  avg_price  NUMERIC(8,6) NOT NULL,
  notional   NUMERIC(18,6) NOT NULL,
  fee_usd    NUMERIC(18,6) NOT NULL,       -- the same formula in both modes
  ts         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE settlements (
  id               BIGSERIAL PRIMARY KEY,
  position_id      BIGINT NOT NULL REFERENCES positions(id) ON DELETE CASCADE,
  token_id         TEXT NOT NULL,
  resolved_outcome TEXT NOT NULL,          -- the label from tokens[].winner
  won              BOOLEAN NOT NULL,
  payout_usd       NUMERIC(18,6) NOT NULL,
  tx_hash          TEXT,                   -- NULL in shadow
  ts               TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE equity_snapshots (
  id              BIGSERIAL PRIMARY KEY,
  mode            mode NOT NULL,
  cash_usd        NUMERIC(18,6) NOT NULL,
  positions_value NUMERIC(18,6) NOT NULL,
  total_usd       NUMERIC(18,6) NOT NULL,
  ts              TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX ON positions (opened_at) WHERE closed_at IS NULL;
CREATE INDEX ON leader_trades (wallet, ts_trade DESC);
CREATE INDEX ON equity_snapshots (mode, ts DESC);
CREATE INDEX ON signals (ts_signal DESC);
