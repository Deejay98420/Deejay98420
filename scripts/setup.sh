#!/usr/bin/env bash
# One-time setup: checks Python, creates .env from the example, migrates both books.
# No third-party Python packages are needed at runtime (stdlib only).
set -euo pipefail
cd "$(dirname "$0")/.."
PY="${PYTHON:-python3}"
"$PY" - <<'PYCHK'
import sys, sqlite3
assert sys.version_info >= (3, 10), "Python 3.10+ required (found %s)" % sys.version.split()[0]
assert sqlite3.sqlite_version_info >= (3, 30, 0), "SQLite 3.30+ required"
print("python", sys.version.split()[0], "| sqlite", sqlite3.sqlite_version)
PYCHK
[ -f .env ] || { cp .env.example .env; chmod 600 .env; echo "created .env (edit it to add optional keys)"; }
"$PY" -m pandabrain migrate
"$PY" -m pandabrain migrate --demo
echo "setup ok"
