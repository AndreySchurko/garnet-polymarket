-- The outcome of an order may be unknown.
--
-- The exchange accepted the order but we never received confirmation of the
-- fill: retrying is forbidden (measured 2026-09-04: a retry bought a second
-- position, $2.00 instead of $1.00), and there is nothing to book. Such a row
-- waits for reconciliation.
--
-- The absence of this status from the schema cost more than the defect itself:
-- the write failed AFTER the money was spent, and a $1.23 position never
-- entered the ledger at all.

ALTER TABLE orders DROP CONSTRAINT orders_status_check;
ALTER TABLE orders ADD CONSTRAINT orders_status_check
  CHECK (status IN ('submitted', 'filled', 'partial', 'rejected', 'unknown'));
