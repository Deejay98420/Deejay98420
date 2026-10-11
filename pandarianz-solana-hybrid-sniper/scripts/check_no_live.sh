#!/usr/bin/env bash
# Static PAPER-safety check: fails if any signing / broadcasting / key-handling capability or
# dependency appears in the codebase. Patterns are split so this file does not match itself.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pats=(
  "send""Transaction" "send""RawTransaction" "send""Bundle" "sign""Transaction" "sign""_message"
  "Key""pair" "secret""_key" "private""_key" "swap-""instructions" "/ultra/v1/""execute" "/ultra/v1/""order"
  "ed25519""_dalek" "solana""_sdk" "solana-""sdk" "solana-""client" "@solana/""web3" "seed_""phrase" "mnem""onic"
)
fail=0
for p in "${pats[@]}"; do
  if grep -RIil --exclude-dir=target --exclude-dir=node_modules --exclude-dir=.next --exclude-dir=.git --exclude=package-lock.json \
       -e "$p" "$ROOT/core/src" "$ROOT/core/Cargo.toml" "$ROOT/dashboard" "$ROOT/scripts" "$ROOT/.env.example" "$ROOT/docker-compose.yml" 2>/dev/null; then
    echo "FORBIDDEN pattern '$p' found in the files above" >&2
    fail=1
  fi
done
if [ -f "$ROOT/core/Cargo.lock" ]; then
  for c in "solana""-sdk" "solana""-client" "solana""-program" "ed25519""-dalek" "solana-key""pair" "solana""-signer"; do
    if grep -q "name = \"$c\"" "$ROOT/core/Cargo.lock"; then echo "FORBIDDEN crate $c in Cargo.lock" >&2; fail=1; fi
  done
fi
if [ -f "$ROOT/dashboard/package-lock.json" ]; then
  if grep -q '"node_modules/@solana/' "$ROOT/dashboard/package-lock.json"; then echo "FORBIDDEN @solana/* package in dashboard" >&2; fail=1; fi
fi
# The engine API must be GET-only.
if grep -nE 'routing::(post|put|patch|delete)|\.(post|put|patch|delete)\(' "$ROOT/core/src/api.rs"; then
  echo "API exposes a mutating route" >&2; fail=1
fi
[ $fail -eq 0 ] && echo "check_no_live: OK — no signing, broadcasting, key handling or live-order endpoints found"
exit $fail
