#!/usr/bin/env bash
# Build and lay out the binaries. Run as `deploy` on the host.
#
# Restarting the units is deliberately not done here: it requires sudo, which
# `deploy` does not have, and a silent failure at the end of the script would
# look exactly like a successful deployment of the old binary.
set -euo pipefail

REPO=${REPO:-$HOME/garnet-polymarket}
cd "$REPO"

# Debug information is disabled: it triples both the binary size and the build
# time, and nothing on the host reads it.
CARGO_PROFILE_RELEASE_DEBUG=0 cargo build --release --bin garnet-core --bin garnet-tg --bin garnet-dash

# Migrations are a separate step and run **after** the build: `sqlx::migrate!`
# compiles them into the binary, so "deploy migrations only" without a rebuild
# applies nothing while still reporting success.
set -a && . ./.env && set +a
./target/release/garnet-core --config config.toml --migrate-only

cat <<'MSG'
done. the restart is yours to make:
  sudo systemctl restart garnet-core garnet-tg garnet-dash
MSG
