-- Baseline fixture with a known-in-advance answer.
--
-- A measurement run only against the production database cannot be checked: a
-- plausible number is indistinguishable from a correct one. Here the answer is
-- known before the query runs — and the first revision of both query 1 and
-- query 2 failed against it.
--
-- Expected output for 1a:  live 1 pair / 4.00 / 1 non-binary · shadow 1 / 5.00 / 0
-- Expected output for 1b:  live 2 pairs / avg 2.0 h / max 3.0 h · shadow 1 / 8.0 / 8.0
-- Expected output for 2:   bucket 1 -> 5, buckets 2..12 -> 1 each
--
-- Run: docker compose exec -T postgres psql -U garnet -d garnet -f /dev/stdin < this file
-- WARNING: it begins with TRUNCATE. Local databases only, never production.
TRUNCATE wallets, markets, leader_trades, positions, signals RESTART IDENTITY CASCADE;

INSERT INTO wallets(address, mode, stake_usd, enabled) VALUES
 ('0xw1','live',10,true), ('0xw2','live',10,true);

-- C1: binary, both legs open (live)            -> a pair, 4 mergeable
-- C2: a single leg                              -> no pair
-- C3: binary, both legs open (shadow)           -> a pair, 5 mergeable
-- C4: one leg open, the other closed (live)     -> no pair
-- C5: THREE open outcomes of one condition      -> NOT binary, cannot be merged
INSERT INTO markets(token_id, condition_id, question, outcome_label) VALUES
 ('t_c1_y','C1','q1','Yes'), ('t_c1_n','C1','q1','No'),
 ('t_c2_a','C2','q2','Up'),
 ('t_c3_p','C3','q3','Over'), ('t_c3_q','C3','q3','Under'),
 ('t_c4_a','C4','q4','A'),   ('t_c4_b','C4','q4','B'),
 ('t_c5_x','C5','q5','X'),   ('t_c5_y','C5','q5','Y'), ('t_c5_z','C5','q5','Z');

INSERT INTO positions(wallet, token_id, mode, size_bought, size_sold, closed_at, opened_at) VALUES
 ('0xw1','t_c1_y','live',  10, 0, NULL, now() - interval '5 hours'),
 ('0xw1','t_c1_n','live',   4, 0, NULL, now() - interval '3 hours'),
 ('0xw1','t_c2_a','live',   7, 0, NULL, now() - interval '2 hours'),
 ('0xw1','t_c3_p','shadow', 5, 0, NULL, now() - interval '9 hours'),
 ('0xw1','t_c3_q','shadow', 6, 1, NULL, now() - interval '8 hours'),
 ('0xw1','t_c4_a','live',   3, 0, NULL, now() - interval '1 hours'),
 ('0xw1','t_c4_b','live',   3, 3, now(), now() - interval '6 hours'),
 ('0xw1','t_c5_x','live',   3, 0, NULL, now() - interval '4 hours'),
 ('0xw1','t_c5_y','live',   3, 0, NULL, now() - interval '4 hours'),
 ('0xw1','t_c5_z','live',   3, 0, NULL, now() - interval '4 hours');

-- Signals: w1 produces a queue of 12 copies one second apart, plus refusals
-- interleaved, plus one late copy. w2 produces three copies a minute apart.
INSERT INTO leader_trades(wallet, tx_hash, token_id, side, price, size, ts_trade, source, market_text, outcome_text)
SELECT '0xw1', 'tx'||g, 't_c1_y', 'buy', 0.5, 1, now(), 'rtds', 'm', 'o' FROM generate_series(1,40) g;

INSERT INTO signals(leader_trade_id, wallet, mode, verdict, target_size_usd, ts_signal)
SELECT g, '0xw1', 'live', 'copy', 10, timestamptz '2026-09-18 10:00:00Z' + (g || ' seconds')::interval
  FROM generate_series(1,12) g;
INSERT INTO signals(leader_trade_id, wallet, mode, verdict, target_size_usd, ts_signal)
SELECT 12+g, '0xw1', 'live', 'skip:slippage_exceeded', 0, timestamptz '2026-09-18 10:00:00Z' + (g || ' seconds')::interval
  FROM generate_series(1,12) g;
INSERT INTO signals(leader_trade_id, wallet, mode, verdict, target_size_usd, ts_signal)
VALUES (25,'0xw1','live','copy',10, timestamptz '2026-09-18 11:00:00Z');
INSERT INTO signals(leader_trade_id, wallet, mode, verdict, target_size_usd, ts_signal)
SELECT 25+g, '0xw2', 'live', 'copy', 10, timestamptz '2026-09-18 10:00:00Z' + (g*60 || ' seconds')::interval
  FROM generate_series(1,3) g;
