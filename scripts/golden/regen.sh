#!/usr/bin/env bash
# Regenerate the golden fixtures in tests/fixtures/ from narrator's Python
# implementation. Reads /home/fernando/git/narrator (read-only) and Fernando's
# vault (read-only, copied to a temp dir before parsing).
#
#   ./regen.sh            regenerate the JSON fixtures only (epub untouched)
#   ./regen.sh --epub     ALSO rebuild fixture.epub (changes its bytes!)
set -euo pipefail
cd "$(dirname "$0")"

UV=(uv run --no-project --python 3.12
    --with ebooklib --with beautifulsoup4 --with lxml python)

if [[ "${1:-}" == "--epub" ]]; then
  "${UV[@]}" make_fixture_epub.py --force
else
  "${UV[@]}" make_fixture_epub.py
fi

"${UV[@]}" gen_golden.py
