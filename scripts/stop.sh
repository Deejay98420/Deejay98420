#!/usr/bin/env bash
# Gracefully stop a running desk (finishes the current step, releases the lease) and wait for it
# to exit, so the book is safe to copy or back up afterwards.
set -euo pipefail
cd "$(dirname "$0")/.."
SUB=public; [ "${1:-}" = "--demo" ] && SUB=demo
PIDF="${PANDABRAIN_DATA_DIR:-data}/$SUB/server.pid"
if [ ! -f "$PIDF" ]; then echo "no running $SUB desk found"; exit 0; fi
PID="$(cat "$PIDF")"
if ! kill -0 "$PID" 2>/dev/null; then echo "no running $SUB desk found (stale pid file)"; rm -f "$PIDF"; exit 0; fi
kill -TERM "$PID"
for _ in $(seq 1 90); do
  kill -0 "$PID" 2>/dev/null || { echo "$SUB desk stopped"; exit 0; }
  sleep 1
done
echo "$SUB desk is still finishing a step after 90s; check it before copying data" >&2
exit 1
