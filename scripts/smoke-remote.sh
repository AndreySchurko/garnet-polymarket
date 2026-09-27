#!/usr/bin/env bash
# Remote smoke test after a deployment. Health is measured by flow, not by
# whether the process is alive.
set -euo pipefail
BOX=${1:?give a host}

ssh "$BOX" 'systemctl is-active garnet-core || true'
ssh "$BOX" 'journalctl -u garnet-core -n 50 --no-pager'
echo "--- check with your own eyes: an RTDS frame and an equity snapshot, not just 'started' ---"
