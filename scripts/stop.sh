#!/usr/bin/env bash
# Gracefully stop a running desk (finishes the current step, releases the lease).
set -euo pipefail
cd "$(dirname "$0")/.."
SUB=public; [ "${1:-}" = "--demo" ] && SUB=demo
PIDF="${PANDABRAIN_DATA_DIR:-data}/$SUB/server.pid"
if [ -f "$PIDF" ]; then kill -TERM "$(cat "$PIDF")" && echo "stop signal sent ($SUB)"; else echo "no running $SUB desk found"; fi
