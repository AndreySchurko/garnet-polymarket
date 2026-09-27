-- An order with no leader decision behind it.
--
-- A time-based exit is our own decision: the leader plays no part in it, and a
-- reference to one of their trades would be a fabrication. Until now `signal_id`
-- was mandatory, because every order was born from someone else's trade;
-- inventing a signal for the sake of the reference would mean recording, in the
-- registry of the leader's decisions, something they never did.

ALTER TABLE orders ALTER COLUMN signal_id DROP NOT NULL;
