#!/usr/bin/env bash
# Run the full offline test suite (fixtures only; no network, no messages sent).
set -euo pipefail
cd "$(dirname "$0")/.."
PYTHONPATH=.:tests PANDABRAIN_NO_OUTBOUND=1 "${PYTHON:-python3}" -m unittest discover -s tests -p "test_*.py" -v "$@"
