-- The fraction of an exit the exchange refused, which we deferred.
--
-- The leader trims a position by percentages, and our share comes out in cents:
-- at a $25 stake, sixty-three percent of exit attempts hit the $1 minimum order
-- size (invariant 15); at a $10 stake it would have been almost a hundred. A
-- refusal on every such sale means we copy the leader's entry and do not copy
-- their exit — that is, we trade a different strategy from the one we measure.
--
-- Accumulated in SHARES, not in fractions: a fraction is computed against the
-- current position size, that size changes with additional buys, and a deferred
-- "five percent" would mean a different number of shares an hour later.

ALTER TABLE positions
  ADD COLUMN pending_exit_shares NUMERIC(18,6) NOT NULL DEFAULT 0;
