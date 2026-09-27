#!/usr/bin/env bash
# Restore a production database dump into a SEPARATE local database and run the
# measurement queries against it.
#
# Three things here are not cosmetic:
#
#   * the restore goes into `garnet_snapshot`, NOT into `garnet` and not into
#     `garnet_test`. On 04.09.2026 the test default pointed at the trading
#     database, and a test run added 11 wallets to the production registry plus
#     1880 test schemas. The same mistake is possible in the other direction
#     here: a snapshot landing on the working database wipes it silently;
#   * the database name is hardcoded and verified rather than taken from an
#     argument: a database that can be called anything will eventually be called
#     `garnet`;
#   * row counts per table are printed after the restore. An empty restored
#     snapshot looks exactly like a successfully applied one, and a measurement
#     against it returns zero — indistinguishable from an honest "that never
#     happens".
#
# Usage:
#   scripts/restore-snapshot.sh ~/garnet-prod.sql        # plain / .gz / -Fc
#   scripts/restore-snapshot.sh ~/garnet-prod.sql --measure
set -euo pipefail

DUMP="${1:-}"
MEASURE="${2:-}"
DB=garnet_snapshot
USER_=garnet
COMPOSE_SVC=postgres
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -z "$DUMP" ]]; then
  echo "give a dump file: scripts/restore-snapshot.sh <file> [--measure]" >&2
  exit 2
fi
if [[ ! -f "$DUMP" ]]; then
  echo "no such file: $DUMP" >&2
  exit 2
fi

cd "$REPO"

psql_() { docker compose exec -T "$COMPOSE_SVC" psql -U "$USER_" "$@"; }

if ! psql_ -d postgres -c 'SELECT 1' >/dev/null 2>&1; then
  echo "postgres is not answering. First: docker compose up -d" >&2
  exit 1
fi

# The format is detected from the signature, not from the extension: a dump
# renamed while being copied around would otherwise silently fail to apply.
#
# The signature is read through `od` into hexadecimal text rather than by
# comparing the bytes themselves: BSD `tr` on macOS dies with
# `Illegal byte sequence` when it meets binary data in a UTF-8 locale, and under
# `set -e` that kills the whole script — found by running it against a .gz on
# 18.09.2026.
magic=$(LC_ALL=C od -An -tx1 -N4 "$DUMP" | tr -d ' \n')
case "$magic" in
  1f8b*)     FORMAT=gz ;;      # gzip
  5047444d)  FORMAT=custom ;;  # "PGDM" — pg_dump -Fc
  *)         FORMAT=plain ;;
esac
echo "dump format: $FORMAT  ($(du -h "$DUMP" | cut -f1))"

echo "recreating database $DB (garnet and garnet_test are left alone)"
psql_ -d postgres -c "DROP DATABASE IF EXISTS $DB;" >/dev/null
psql_ -d postgres -c "CREATE DATABASE $DB;" >/dev/null

echo "restoring…"
case "$FORMAT" in
  gz)     gunzip -c "$DUMP" | psql_ -d "$DB" -q -v ON_ERROR_STOP=0 >/dev/null 2>"$REPO/.restore.err" ;;
  plain)  psql_ -d "$DB" -q -v ON_ERROR_STOP=0 -f /dev/stdin < "$DUMP" >/dev/null 2>"$REPO/.restore.err" ;;
  custom) docker compose exec -T "$COMPOSE_SVC" pg_restore -U "$USER_" -d "$DB" --no-owner --no-privileges < "$DUMP" 2>"$REPO/.restore.err" || true ;;
esac

# A dump taken from production references the `garnet_app` role, which does not
# exist here, and owners that do not exist here either. That is expected and is
# not a failure: the failure is an absence of rows, which is what gets checked
# below.
if [[ -s "$REPO/.restore.err" ]]; then
  echo "restore warnings (first 5):"
  grep -v -E "role .* does not exist|no privileges|must be owner" "$REPO/.restore.err" | head -5 || true
fi
rm -f "$REPO/.restore.err"

echo
echo "=== what arrived ==="
psql_ -d "$DB" -c "
SELECT relname AS table, n_live_tup AS rows
  FROM pg_stat_user_tables
 WHERE n_live_tup > 0
 ORDER BY n_live_tup DESC;"

total=$(psql_ -d "$DB" -tAc "SELECT coalesce(sum(n_live_tup),0) FROM pg_stat_user_tables;" | tr -d '[:space:]')
if [[ "$total" == "0" ]]; then
  echo "REFUSED: the snapshot contains no rows at all. A measurement against" >&2
  echo "it would return zeros, and zeros read as an answer. Check that the dump" >&2
  echo "holds data and not just the schema (pg_dump without --schema-only)." >&2
  exit 1
fi
echo "rows in total: $total"

if [[ "$MEASURE" == "--measure" ]]; then
  echo
  echo "=== baseline queries ==="
  psql_ -d "$DB" -f /dev/stdin < "$REPO/docs/fixtures/baseline-queries.sql"
fi

echo
echo "snapshot ready: database $DB"
echo "measurements:  docker compose exec -T postgres psql -U garnet -d $DB -f /dev/stdin < docs/fixtures/baseline-queries.sql"
