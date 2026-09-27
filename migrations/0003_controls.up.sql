-- The operator's manual stop, and other switches that outlive the process.
--
-- Until now the killswitch was a `Mutex` inside `App`: a restart silently
-- lifted the operator's stop. And `/kill` from Telegram is a separate process,
-- so its channel must work precisely when nothing else does. The bus is unfit
-- for this: an unreachable NATS must not mean that the stop did not take
-- effect. The database is the one dependency without which trading is already
-- halted anyway.

CREATE TABLE controls (
  key        text PRIMARY KEY,
  value      text        NOT NULL,
  actor      text        NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now()
);
