-- Baseline measurements: the numbers that decide whether a mechanism is worth
-- building, and where a threshold should be set.
--
-- Verified 18.09.2026 against docs/fixtures/baseline-seed.sql.
-- Read-only: not a single INSERT / UPDATE / DELETE.
--
-- Run it wherever the dump currently lives:
--
--   psql "<url>" -f baseline-queries.sql
--
-- or, having restored the dump into a local container:
--   docker compose exec -T postgres psql -U garnet -d garnet -f /dev/stdin < baseline-queries.sql
--
-- An empty database returns zeros, indistinguishable from an honest "that never
-- happens". The expected output against the fixture is in the header of
-- docs/fixtures/baseline-seed.sql.

\echo
\echo === 1a  pairs right now: how much capital is locked at this instant ===
-- Only a binary condition can be merged: a [1,2] partition over three outcomes
-- is wrong. A condition with anything other than exactly two open legs is
-- counted separately.
WITH open_legs AS (
  SELECT m.condition_id, p.mode, p.token_id,
         p.size_bought - p.size_sold AS open_size
    FROM positions p JOIN markets m USING (token_id)
   WHERE p.closed_at IS NULL AND p.size_bought - p.size_sold > 0
), by_cond AS (
  SELECT condition_id, mode, count(*) AS legs,
         min(open_size) AS smaller_leg
    FROM open_legs GROUP BY condition_id, mode
)
SELECT mode,
       count(*) FILTER (WHERE legs = 2)                        AS binary_pairs,
       round(coalesce(sum(smaller_leg) FILTER (WHERE legs = 2), 0), 2) AS pairs_mergeable,
       round(coalesce(sum(smaller_leg) FILTER (WHERE legs = 2), 0), 2) AS usd_unlockable,
       count(*) FILTER (WHERE legs > 2)                        AS non_binary_skipped
  FROM by_cond GROUP BY mode ORDER BY mode;

\echo
\echo === 1b  pairs over history: did they ever form, and for how long ===
-- The same over history rather than at an instant: an instant can catch zero by
-- accident. Non-binary conditions are excluded for the same reason as in 1a.
WITH legs AS (
  SELECT m.condition_id, p.mode, p.token_id, p.opened_at,
         coalesce(p.closed_at, now()) AS ended_at
    FROM positions p JOIN markets m USING (token_id)
), binary_conds AS (
  SELECT condition_id, mode FROM legs
   GROUP BY condition_id, mode HAVING count(DISTINCT token_id) = 2
)
SELECT a.mode,
       count(*) AS overlapping_pairs_ever,
       round(avg(extract(epoch FROM
             least(a.ended_at, b.ended_at) - greatest(a.opened_at, b.opened_at)
       ) / 3600.0)::numeric, 1) AS avg_overlap_hours,
       round(max(extract(epoch FROM
             least(a.ended_at, b.ended_at) - greatest(a.opened_at, b.opened_at)
       ) / 3600.0)::numeric, 1) AS max_overlap_hours
  FROM legs a
  JOIN legs b ON a.condition_id = b.condition_id AND a.mode = b.mode AND a.token_id < b.token_id
  JOIN binary_conds c ON c.condition_id = a.condition_id AND c.mode = a.mode
 WHERE least(a.ended_at, b.ended_at) > greatest(a.opened_at, b.opened_at)
 GROUP BY a.mode ORDER BY a.mode;

\echo
\echo === 2   entry queue depth: the threshold for fire_limit ===
-- Queue depth is measured in COPIES: a refusal spends no capital, and its
-- presence in the distribution would inflate every bucket.
WITH depth AS (
  SELECT wallet, mode, verdict, ts_signal,
         count(*) FILTER (WHERE verdict = 'copy') OVER (
           PARTITION BY wallet, mode ORDER BY ts_signal
           RANGE BETWEEN INTERVAL '30 seconds' PRECEDING AND CURRENT ROW) AS bucket
    FROM signals
)
SELECT bucket, count(*) AS copies_at_this_depth
  FROM depth WHERE verdict = 'copy'
 GROUP BY bucket ORDER BY bucket;
