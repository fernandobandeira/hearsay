#!/usr/bin/env bash
#
# Regenerate the typed TypeScript client from the Rust server's own OpenAPI
# document, then check it against the contract the React reader already uses.
#
#   openapi.json  the checked-in contract. `narrator --openapi` prints it; the
#                 binary needs no work dir and no running server.
#   client/       the generated fetch client: types + one function per operation.
#   client/DRIFT.md  what the reader would have to change to adopt it.
#
# Nothing outside this repo is written. ~/git/narrator is read for its
# hand-written types and never touched.
#
# Usage: scripts/gen-client.sh
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO="$PWD"
BIN="$REPO/target/release/narrator"
SPEC="$REPO/openapi.json"
OUT="$REPO/client"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
step() { printf '\n\033[1m==>\033[0m %s\n' "$*"; }

# ---------------------------------------------------------------------------
step "Server binary"
# Building unconditionally costs a couple of minutes on a cold target dir and
# seconds on a warm one, but a stale binary means a stale contract — which is
# the one failure this script exists to prevent. cargo decides if there is work.
# ---------------------------------------------------------------------------
cargo build --release --bin narrator
[[ -x "$BIN" ]] || { echo "no binary at $BIN after build" >&2; exit 1; }

# ---------------------------------------------------------------------------
step "OpenAPI document"
# ---------------------------------------------------------------------------
"$BIN" --openapi > "$SPEC.tmp"
mv -f "$SPEC.tmp" "$SPEC"

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
echo "  ${FILES} TypeScript files under client/"
echo "  ${TYPES} exported types (client/types.gen.ts)"
echo "  ${SDK_OPS} exported operations (client/sdk.gen.ts)"
echo "  fetch runtime vendored into client/client/ and client/core/ — no npm dependency at integration time"

# ---------------------------------------------------------------------------
step "Drift check against the reader's hand-written contract"
# ---------------------------------------------------------------------------
set +e
node "$REPO/scripts/drift-check.mjs"
DRIFT_STATUS=$?
set -e

echo
if [[ $DRIFT_STATUS -eq 0 ]]; then
  bold "OK — the generated client is a drop-in for the reader's current calls."
  echo "Any 'additive' or 'missing-optional' lines above are safe by construction:"
  echo "extra fields the reader ignores, or optional fields it already guards."
else
  bold "FAILED — the drift check found MISMATCHES."
  echo
  echo "A MISMATCH means a value from the generated client is not assignable to the"
  echo "type the reader declares: a required field is gone, a type changed, a required"
  echo "field became optional, or nullability moved. Those break the port."
  echo
  echo "Not counted as mismatches, and never a reason for this to fail:"
  echo "  additive        — fields only the generated type has; the reader ignores them"
  echo "  missing-optional — an optional hand-written field the server no longer sends"
  echo "  now-always-sent  — a field the reader treated as optional that is now guaranteed"
  echo
  echo "Full report: client/DRIFT.md"
fi

exit $DRIFT_STATUS
