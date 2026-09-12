#!/usr/bin/env bash
#
# Regenerate the reader's API client from the Rust server's own OpenAPI document.
#
#   openapi.json        the checked-in contract. `narrator --openapi` prints it;
#                       the binary needs no work dir and no running server.
#   web/src/client/     the generated fetch client: types + one function per
#                       operation. It lives inside the reader because the reader
#                       is what imports it — and because the reader's image is
#                       built from `web/` alone.
#
# Both are committed, and `--check` is the gate that keeps them honest: it
# regenerates and fails if the result differs from what is in git. A response
# shape changed in Rust and not carried through here is then a red build rather
# than a reader that compiles and misreads a field.
#
# Usage:
#   scripts/gen-client.sh                 build the server, dump the spec, generate
#   scripts/gen-client.sh --from-spec     skip cargo; generate from the committed spec
#   scripts/gen-client.sh --check         ... and fail if anything differs from git
#   NARRATOR_BIN=path scripts/gen-client.sh   use an existing binary (a debug one
#                                             prints the same document)
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO="$PWD"
BIN="${NARRATOR_BIN:-$REPO/target/release/narrator}"
SPEC="$REPO/openapi.json"
OUT="$REPO/web/src/client"

FROM_SPEC=0
CHECK=0
for a in "$@"; do
  case "$a" in
    --from-spec) FROM_SPEC=1 ;;
    --check)     CHECK=1 ;;
    *) echo "unknown argument: $a" >&2; exit 2 ;;
  esac
done

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
step() { printf '\n\033[1m==>\033[0m %s\n' "$*"; }

# ---------------------------------------------------------------------------
if [[ $FROM_SPEC -eq 1 ]]; then
  step "OpenAPI document (using the committed openapi.json)"
  [[ -f "$SPEC" ]] || { echo "no $SPEC" >&2; exit 1; }
else
  step "Server binary"
  # A stale binary means a stale contract, which is the one failure this script
  # exists to prevent. cargo decides whether there is any work to do.
  if [[ -n "${NARRATOR_BIN:-}" ]]; then
    echo "  using $BIN"
  else
    cargo build --release --bin narrator
  fi
  [[ -x "$BIN" ]] || { echo "no binary at $BIN" >&2; exit 1; }

  step "OpenAPI document"
  "$BIN" --openapi > "$SPEC.tmp"
  mv -f "$SPEC.tmp" "$SPEC"
fi

read -r SPEC_PATHS SPEC_OPS SPEC_SCHEMAS <<<"$(node -e '
  const s = JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"));
  const verbs = ["get","post","put","patch","delete","head","options"];
  const paths = Object.values(s.paths ?? {});
  const ops = paths.reduce((n, p) =>
    n + Object.keys(p).filter((k) => verbs.includes(k)).length, 0);
  console.log(paths.length, ops, Object.keys(s.components?.schemas ?? {}).length);
' "$SPEC")"

echo "  openapi.json — ${SPEC_PATHS} paths, ${SPEC_OPS} operations, ${SPEC_SCHEMAS} schemas"

# ---------------------------------------------------------------------------
step "Client codegen"
# Versions are pinned in scripts/client/package.json so a regeneration months
# from now produces the same client, not whatever npm ships that week.
# ---------------------------------------------------------------------------
cd "$REPO/scripts/client"
if [[ ! -d node_modules ]]; then
  echo "  installing pinned toolchain..."
  npm ci --no-audit --no-fund --silent 2>/dev/null || npm install --no-audit --no-fund --silent
fi
npx --no-install openapi-ts
cd "$REPO"

FILES=$(find "$OUT" -name '*.ts' | wc -l | tr -d ' ')
TYPES=$(grep -c '^export type ' "$OUT/types.gen.ts" || true)
SDK_OPS=$(grep -c '^export const ' "$OUT/sdk.gen.ts" || true)

echo
echo "  ${FILES} TypeScript files under web/src/client/"
echo "  ${TYPES} exported types (types.gen.ts)"
echo "  ${SDK_OPS} exported operations (sdk.gen.ts)"
echo "  fetch runtime vendored in — no npm dependency at integration time"

# ---------------------------------------------------------------------------
if [[ $CHECK -eq 1 ]]; then
  step "Drift gate"
  # The generated client is the reader's only description of the API, so the
  # question is not "is it compatible" any more — it is "is what is committed
  # what this server actually generates". Anything else is a lie the compiler
  # cannot see.
  if git diff --quiet -- "$SPEC" "$OUT"; then
    bold "OK — openapi.json and web/src/client match the server."
    exit 0
  fi
  git --no-pager diff --stat -- "$SPEC" "$OUT"
  echo
  bold "FAILED — the committed contract is not what this server generates."
  echo "Run ./narrator client and commit the result in the same change."
  exit 1
fi

bold "OK — regenerated. Commit openapi.json and web/src/client together."
