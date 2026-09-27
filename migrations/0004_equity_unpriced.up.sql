-- How many positions entered the snapshot without a price.
--
-- `snapshot_equity` counted them and returned the count to its caller, but the
-- write dropped it: the database kept an understated total with no indication
-- that it was incomplete. Measured 2026-09-04: the 24-hour equity delta showed
-- +12.55 against +12.28 realised — the difference came from a position that
-- fell out of the snapshot silently.
--
-- Valuing such a position at its entry price is not allowed: that is an
-- invention, not a price. But the reader must be able to see that the total is
-- incomplete — otherwise it looks more precise than it is.

ALTER TABLE equity_snapshots ADD COLUMN unpriced INTEGER NOT NULL DEFAULT 0;
