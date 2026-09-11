# scripts/ — the client contract

The Rust server describes itself. `narrator --openapi` prints an OpenAPI 3.1
document and exits — no work dir, no running server, no side effects. Everything
here hangs off that one fact.

## Regenerate

```bash
scripts/gen-client.sh
```

That does four things, in order:

1. `cargo build --release --bin narrator` (cargo no-ops when it is already current)
2. `narrator --openapi > openapi.json` — **this file is committed.** It is the
   contract: the thing to read in a diff when someone asks what changed about
   the API.
3. `@hey-api/openapi-ts` → `client/` — a fetch-based client: `types.gen.ts`
   (every schema), `sdk.gen.ts` (one exported function per operation), and the
   fetch runtime vendored into `client/client/` and `client/core/`, so the
   generated client has no npm dependency of its own at integration time.
4. the drift check below. **Non-zero exit if it finds mismatches.**

Versions are pinned in `scripts/client/package.json` — regenerating in six
months produces the same client, not whatever npm shipped that week. To bump the
generator, change the pin there, run `npm install` in `scripts/client/`, and
regenerate. `scripts/client/node_modules/` is gitignored.

To run only the comparison, without rebuilding or regenerating:

```bash
node scripts/drift-check.mjs
```

It reads `$NARRATOR_WEB` if set, otherwise `~/git/narrator/web/src/lib`.

## What the drift check means

The React reader wrote this server's API down by hand, before this server
existed: `narrator/web/src/lib/types.ts` (the shapes) and `api.ts` (the URLs).
That is the contract the Rust rewrite has to satisfy. The check asks one narrow
question:

> Is a value from the generated client assignable to the type the reader already
> declares?

It answers it through the TypeScript compiler — the real type checker, over both
files — so nested `$ref`s, inline object literals and arrays are compared
structurally rather than by name or by string. Three verdicts:

| verdict | meaning | fails the build |
|---|---|---|
| **MISMATCH** | breaks the assignment: a required field is gone, a type changed, a required field became optional, or nullability moved | yes |
| **NOTE** | safe: an *optional* hand-written field the server no longer sends, or a field the reader treated as optional that is now always present | no |
| **ADDITIVE** | fields only the generated type has — the server grew, the reader ignores them | no |

It also walks `api.ts` for every URL the reader builds and checks each one
exists in `openapi.json` with the right method. A missing endpoint fails the
build too; those are extracted from the AST, so a new call site in the reader
shows up here without anyone maintaining a list.

The latest run is written to `client/DRIFT.md`.

A MISMATCH is a decision, not automatically a bug. It says the two descriptions
of the API disagree, and someone has to say which one is right. The fix belongs
on the Rust side or in the reader's types — never in this script.

## narrator/web is never modified from here

`~/git/narrator` is read-only to this repo. These scripts open its files to
learn the contract and write nothing back, ever — not a formatting pass, not an
import fix, nothing.

The port runs the other way and by hand: when the Rust server is ready, the
React app gets moved onto the generated client deliberately, in its own repo,
in its own commit. `client/DRIFT.md` is the list of what that move has to
reconcile. It is a worklist, not a patch.
