#!/usr/bin/env bash
# Build dist/pandarianz-pandabrain-desk-v<version>.zip from tracked files (no data, no secrets).
set -euo pipefail
cd "$(dirname "$0")/.."
V=$(python3 -c "import pandabrain; print(pandabrain.__version__)")
NAME="pandarianz-pandabrain-desk-v$V"
mkdir -p dist
rm -f "dist/$NAME.zip"
git ls-files | grep -v '^dist/' | python3 -c "
import sys, zipfile, os
name = sys.argv[1]
with zipfile.ZipFile(f'dist/{name}.zip', 'w', zipfile.ZIP_DEFLATED) as z:
    for f in (l.strip() for l in sys.stdin if l.strip()):
        assert not f.startswith(('.env', 'data/')) or f == '.env.example', f
        zi = zipfile.ZipInfo.from_file(f, f'{name}/{f}')
        zi.compress_type = zipfile.ZIP_DEFLATED
        with open(f, 'rb') as fh:
            z.writestr(zi, fh.read())
print(f'dist/{name}.zip')
" "$NAME"
