-- The leader's price, which the floor of the deferred exit was computed from.
--
-- A deferred exit waited for the leader's next sale, and only for that: on
-- 06.09.2026 a position sat with a deferred exit covering its entire size and
-- never repeated once — the leader had exited in full and was not going to sell
-- again. The retry is needed, but it has nothing to aim at: the leader's price
-- lived in the trade frame and was stored nowhere, and taking today's bid
-- instead would mean deciding afresh.
--
-- Zero means "nothing to retry": the column is cleared together with the shares.

ALTER TABLE positions
  ADD COLUMN pending_exit_price NUMERIC(18,6) NOT NULL DEFAULT 0;
