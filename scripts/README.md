# scripts/ — the client contract

The Rust server describes itself. `narrator --openapi` prints an OpenAPI 3.1
document and exits — no work dir, no running server, no side effects. Everything
here hangs off that one fact.

## Regenerate

```bash
scripts/gen-client.sh                  # build, dump the spec, generate the client
scripts/gen-client.sh --from-spec      # skip cargo: generate from the committed spec
scripts/gen-client.sh --check          # ... and fail if the result differs from git
NARRATOR_BIN=target/debug/narrator scripts/gen-client.sh   # use a binary you have
```

The full run does three things, in order:

1. `cargo build --release --bin narrator` (cargo no-ops when it is current)
2. `narrator --openapi > openapi.json` — **this file is committed.** It is the
   contract: the thing to read in a diff when someone asks what changed about
   the API.
3. `@hey-api/openapi-ts` → `web/src/client/` — a fetch-based client:
   `types.gen.ts` (every schema), `sdk.gen.ts` (one exported function per
   operation), and the fetch runtime vendored into `client/` and `core/`, so the
   generated client has no npm dependency of its own at integration time.

Versions are pinned in `scripts/client/package.json` — regenerating in six
months produces the same client, not whatever npm shipped that week. To bump the
generator, change the pin there, run `npm install` in `scripts/client/`, and
regenerate. `scripts/client/node_modules/` is gitignored.

## Why the client lives inside the reader

`web/src/client/` is generated, committed, and imported by `web/src/lib/api.ts`
and `web/src/lib/types.ts` — it is the reader's **only** description of this API.
It sits under `web/` because that is what the reader's own image is built from
(`web/Dockerfile`, context `./web`), and because a generated client nobody
imports is just a file.

## The gate

There is no compatibility check any more, because there are no longer two
descriptions of the API to compare. There is one, generated, and the only
question worth asking is whether what is committed is what this server actually
generates:

```bash
scripts/gen-client.sh --check              # regenerate and diff against git
scripts/gen-client.sh --from-spec --check  # the client only, from the committed spec
```

Both run in CI, and the split is about what each workflow has available:

| workflow | runs | catches |
|---|---|---|
| `release.yml` (server) | `--check`, with the debug binary | a response shape that changed in Rust and was not carried into `openapi.json` and the client |
| `web.yml` (reader) | `--from-spec --check` | a hand-edited generated client, on a push that never touches Rust |

A failure is not a mystery: run `./narrator client` and commit the result in the
same change. That is also the rule — **a change to any response shape means
regenerating in the same commit** — and now it is enforced rather than
remembered.

## ~/git/narrator is never modified from here

The python repo is read-only to this one. It was the reference the contract was
transcribed from (`tests/fixtures/api_contract.json` still is that
transcription); nothing here writes to it, ever — not a formatting pass, not an
import fix, nothing.
