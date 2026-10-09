#!/usr/bin/env bash
# Start the desk on http://127.0.0.1:8765/  (add --demo for the synthetic demo book)
# Stop with Ctrl+C, or from another terminal: ./scripts/stop.sh [--demo]
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f .env ] || ./scripts/setup.sh
exec "${PYTHON:-python3}" -m pandabrain serve "$@"
