#!/usr/bin/env bash
# Copy the reader's build into web/dist, where the image picks it up.
#
# The reader still lives in ~/git/narrator/web and is built there; this repo
# carries the *result* so the image can be built on the VPS without node
# touching the source tree. Re-run after every reader build:
#
#   (cd ~/git/narrator/web && npm run build) && ./scripts/sync-web.sh
set -euo pipefail
src="${NARRATOR_WEB_SRC:-$HOME/git/narrator/web/dist}"
dst="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/web/dist"
if [ ! -f "$src/index.html" ]; then
  echo "no build at $src - run 'npm run build' in the reader first" >&2
  exit 1
fi
rm -rf "$dst"; mkdir -p "$dst"
cp -a "$src/." "$dst/"
echo "synced $(find "$dst" -type f | wc -l) files, $(du -sh "$dst" | cut -f1) from $src"
