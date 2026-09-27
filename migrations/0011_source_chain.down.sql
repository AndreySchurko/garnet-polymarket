DELETE FROM leader_trades WHERE source = 'chain';
ALTER TABLE leader_trades DROP CONSTRAINT leader_trades_source_check;
ALTER TABLE leader_trades ADD CONSTRAINT leader_trades_source_check
  CHECK (source IN ('rtds', 'poll'));
