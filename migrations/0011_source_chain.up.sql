-- The third detection circuit: Polygon logs.
--
-- `leader_trades.source` holds the winner of the delivery race, and until now
-- there could be only two contenders. The chain-log circuit does not depend on
-- Polymarket's infrastructure at all: on 31.08.2026 the `activity/trades` topic
-- went down platform-wide for hours, from every IP at once, and the only backup
-- was the slow `/activity` poll.
--
-- In `trade_sightings` the value `chain` has been permitted since migration
-- 0010: it was allowed there in advance, so that the arrival of the circuit
-- would not require a migration at exactly the moment it was being debugged.

ALTER TABLE leader_trades DROP CONSTRAINT leader_trades_source_check;
ALTER TABLE leader_trades ADD CONSTRAINT leader_trades_source_check
  CHECK (source IN ('rtds', 'poll', 'chain'));
