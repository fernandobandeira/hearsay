# narrator-rs — repo instructions

This file is `AGENTS.md`; `CLAUDE.md` is a symlink to it (Fernando's convention — edit this file, never break the symlink).

The **Rust rewrite of narrator** (`~/git/narrator`, Python/FastAPI). Same HTTP contract, same `work/` cache, same writes into the Obsidian vault — so the two are interchangeable in front of the same machine state. Kokoro-82M on CPU through ONNX Runtime, whisper.cpp for voice memos, axum + tokio, one binary.

**Why it exists.** Fernando's motivation is stability, not speed: the reader must never crash and must heal itself. So the type system is doing real work here — every failure path is a typed error with a caller that logs it and degrades, there is no `unwrap()`/`expect()` outside tests and startup, and the render worker derives its bookkeeping from the filesystem rather than from a counter it keeps in its head (see [the disk-truth invariant](#the-disk-truth-invariant), which is a bug the Python version still has).

> The Python repo is the **reference**. Read `~/git/narrator/AGENTS.md` before changing anything here; its "The HTTP API — treat as a frozen contract" section is the spec this crate implements. Never edit that repo from this one.

## Layout

| Path | What it is |
|---|---|
| `src/book.rs` | EPUB → render plan. A **bug-for-bug** port of `app/book.py`: spine order, sentence-aware chunking at `max_chars` 300, `is_speakable()` silent beats. See [chunking parity](#chunking-parity-the-one-that-cannot-move). |
| `src/tts/g2p.rs` | Text → Kokoro phonemes: espeak-ng as a **subprocess**, then misaki's `EspeakFallback` character mapping verbatim. |
| `src/tts/kokoro.rs` | Kokoro-82M v1.0 fp32 ONNX through `ort`. The 178-symbol vocabulary is inlined as model contract; the voice pack is 510 style vectors indexed by phoneme count. |
| `src/tts/mod.rs` | `Engine`: lazy load, never fatal, plus the deterministic fake (`NARRATOR_FAKE_TTS=1`) the whole test suite renders with. |
| `src/stt.rs` | whisper.cpp via `whisper-rs`: `large-v3-turbo-q5_0`, CPU, one transcription at a time, ffmpeg decoding the webm, silero VAD when its model is present. |
| `src/cache.rs` | `work/audio/<key>/chNNN/IIIII.wav` and `gc_audio`. |
| `src/chapters.rs` | Chapter packing (AAC m4a + chunk→second manifest), lazy HLS, the chapter gc. |
| `src/plancache.rs` | The parse cache: `plan.json` plus a `parse.json` stamp, so `/api/load` does not re-parse a book that has not changed. |
| `src/text.rs` | The two-tier offline text bundle: `index.json` + byte-budgeted shards. |
| `src/vault.rs` | `.narrator-positions.json`, `Reading Log.md`, fleeting notes — all byte-identical to the Python output. |
| `src/render.rs` | The render worker and the packer, one OS thread each. |
| `src/export.rs` | `narrator export`: the streaming cache packed into one `.m4b`, chapter marks and cover art included. A port of `app/export.py`. |
| `src/events.rs`, `src/api/stream.rs` | The SSE bus and `/api/events`. |
| `src/state.rs` | One global session, exactly like Python's process-wide `S`. |
| `src/api/` | Every endpoint, with typed request/response structs that **generate** the OpenAPI document. |
| `src/watch.rs` | The library watcher (`notify`), which turns "the vault's git sync pulled an epub onto the server" into a `books` event. |
| `src/wishlist.rs` | The chapters someone asked for, kept across restarts: `work/audio/<key>/queue.json`, intent only. See [downloading a chapter](#downloading-a-chapter-end-to-end). |
| `tests/` | Five parity suites, the reader-requirement suite, and a harness that runs a whole server in a temp dir. |
| `scripts/golden/` | Generates the Python golden fixtures (uv + ebooklib + bs4) the parity tests assert against. |
| `scripts/gen-client.sh` | OpenAPI → the reader's typed TS client (`web/src/client/`), and the `--check` gate CI runs. |
| `listen-test/` | GATE 0: the ONNX engine rendered against the PyTorch render Fernando accepted. |
| `web/src/client/` | **Generated**, committed, and the reader's only description of the API. Never hand-edited; see [the gate](#openapi-is-the-contract). |
| `web/` | **The reader itself** — the Vite/React/Tailwind PWA, source and all. It moved in from the python repo; the build is no longer vendored (`web/.gitignore` ignores `dist/`), because two images are built from this tree now. See [the two images](#two-images-server-and-reader). `web/placeholder/` is the fallback page when there is no build at all. |
| `deploy/` | systemd templates for the Oracle A1 — **applied by hand, never by a playbook**, like the python repo's. |

## Running it

```bash
./narrator models        # fetch Kokoro + whisper weights into ./models (~900 MB)
./narrator web           # npm ci && npm run build in web/ (dev serves web/dist)
./narrator dev           # cargo run, serving ./web and ./work
./narrator build && ./narrator up     # docker, port 7870 on localhost only
./narrator test          # the whole suite: 139 tests, no model, no network
./narrator lint          # rustfmt --check + clippy -D warnings
./narrator client        # regenerate openapi.json + web/src/client
./narrator export --book books/Title.epub [--partial]   # the cache → a .m4b
./narrator listen-test   # re-render the GATE 0 passage and report RTF
```

`cargo test` needs neither weights nor network: `NARRATOR_FAKE_TTS` swaps in a deterministic tone as long as the real thing would be, which is how a chapter renders and packs in milliseconds.

## Parity guarantees

Everything below is asserted by a test. "Golden" means the fixture was produced by running the Python implementation, not by blessing this one's output.

### Chunking: the one that cannot move

A chunk index *is* a reading position. It names a wav on disk, an entry in a chapter manifest, a line in the vault's `Reading Log.md`, and the passage a voice note points back to. Move one boundary and Fernando's place in a 1433-chapter book silently relocates and every packed chapter in the cache becomes a lie. So `src/book.rs` reproduces the Python exactly, **including two bugs**:

- **The abbreviation guards are inert.** `_ABBR` puts lookbehinds like `(?<!\bDr)` *before* `(?<=[.!?])`, so they test the two characters ending at the split point — which are `r.`, never `Dr`. All ten guards do nothing and "Dr. Smith" splits in two.
- **The split eats a closing quote.** `["'”’)\]]*` sits inside the separator, so `He said "go." Then` loses the `"`.

Fixing either is a migration, not an edit: it invalidates every rendered chunk, every packed chapter and every stored position. Three things Rust does *not* get for free are also handled: `len()` counts characters, Python's `\s` is wider than `char::is_whitespace`, and `str.isalnum()` includes the `Nl`/`No` categories — that last one decides whether a footnote marker like `²` is a silent beat.

Verified against `7 Powers (2016).epub` copied out of the vault: 22 chapters, 1577 chunks, 237 400 characters, identical chapter ids, titles, counts and first/last chunk text, with chapters 0–2 identical chunk for chunk.

And, the check that actually settles it — `tests/parity_chunking.rs`'s opt-in
`a_python_written_plan_matches_chunk_for_chunk` re-chunks whatever books a real
narrator working tree already has a `plan.json` for and compares against the
file **the Python server itself wrote**. Run against `~/git/narrator/work`:

```
01 - Lord of Mysteries: 1433 chapters, 118831 chunks, identical
```

Every chapter id, every title, every chunk's text, paragraph index and silent
flag. That is the whole cache and every stored position in Fernando's largest
book, proven to survive the swap. It is opt-in because it needs the epub still
sitting next to the plan; `NARRATOR_REF_WORK` and `NARRATOR_REF_BOOKS` point it
somewhere else, and it only ever reads (the epub is copied out before parsing).

### Cache layout

`work/audio/<stem[:50]>/chNNN/IIIII.wav`, 24 kHz mono s16le; `plan.json` beside the chapter directories; `chapters/<key>/chNNN.{m4a,json}`; `hls/<key>/chNNN/`; `text/<key>/{index,NNN}.json`, each with a `.json.gz` beside it. **A Rust deploy adopts the existing cache in place** — a test seeds a Python-shaped cache and asserts nothing is re-rendered.

**The text bundle is pre-gzipped at build time**, exactly as the python reference does it: `write_json_gz` writes the `.json` and its `.gz` sibling in one call (level 9, `mtime=0`, so a rebuild from the same words produces the same bytes), and `/api/book.json` and `/api/text/{s}.json` serve the `.gz` with `Content-Encoding: gzip` when the request's `Accept-Encoding` takes it, `Vary: Accept-Encoding` either way. The decoded body is byte-identical to the plain file — content negotiation only, the frozen contract is untouched. JSON this repetitive compresses to ~33 %: *Lord of Mysteries* goes 17.7 MB → 5.8 MB over the wire, once per device rather than once per request. A missing `.gz` (a bundle written before this existed) falls back silently to the plain file, and the next `/api/load` rewrites the bundle to earn one even when the plan itself is reused. The negotiation is hand-rolled rather than a `tower-http` compression layer for two reasons: a layer would re-compress those megabytes on every request instead of serving the file that is already on disk, and it must never touch the audio endpoints, which serve byte ranges — a range of a compressed body is not the range iOS asked for.

`gc_audio` trims to 90 % of `MAX_AUDIO_GB`, oldest first, and never touches the current chapter, its prerender span, or any chapter the manager is rendering, flagged for packing, queued to pack, or packing right now — chunk wavs are the *input* to a pack, and evicting them leaves a chapter permanently one hole short of packable.

One deliberate improvement: chunks are written to a `.part` and renamed. The Python server writes in place, so a killed container leaves a truncated wav that looks rendered.

### Vault writes

`.narrator-positions.json` is written the way `json.dumps(pos, indent=1)` writes it — one-space indent, `": "` after keys, and **non-ASCII escaped**, which is `ensure_ascii=True`'s doing and the part `serde_json` would otherwise get wrong. `Reading Log.md` is byte-identical, unescaped table pipes included: a `|` in a book stem genuinely breaks the row, and matching the Python matters more than a prettier table, because anything else is churn in every `vault backup:` commit. Fleeting notes match byte for byte, deep link and all. Positions are throttled to 15 s and forced on open/pause.

### The disk-truth invariant

**The bug this rewrite was asked to fix.** The Python worker advances `render_idx` and treats it as the record of what has been rendered. It is not: `gc_audio` can delete a chunk behind the frontier, a forward `/api/playhead` jump drags `render_idx` past holes, and a container restart resets it. In each case the chunk the reader is *waiting on* never gets rendered, because the bookkeeping has already moved past it, and the reader sits on a 404 forever with a renderer that believes it is up to date.

Here `render_idx` is a **hint** and the filesystem is the truth:

1. If the chunk under the playhead is missing, it is rendered first. Always.
2. Otherwise the worker scans forward from the hint for a chunk that is genuinely absent.
3. `/api/open` sets the playhead, so rule 1 guarantees the opened chunk renders, whatever the cache looks like.

Tested three ways: a cache with holes opened at a missing chunk, a hole punched *behind* a completed frontier, and a forward jump past the frontier.

**And the other half of rule 1, which is that it has no exit.** "Render the chunk
under the playhead, always" is exactly right while the render can succeed and
exactly wrong when it cannot — a wedged espeak-ng, a work directory gone
read-only, a model that never loaded — because then it is a full-speed retry
loop: a subprocess spawn and a `warn!` line per iteration, a core burnt for
nothing and the log buried. Measured at **3611 attempts in 2.5 s** with the
backoff disabled. So a run of *consecutive* failed renders now pauses the worker,
250 ms doubling to a 30 s cap, and any success puts it straight back to zero. The
invariant is untouched: the chunk is still retried, still first, just not
thousands of times a second. It is deliberately not per-chunk — a render that
fails is almost always systemic, so in that state every chunk fails and a
per-chunk counter would only ping-pong between two hot targets.

The worker also parks entirely while a voice memo is being transcribed — see
[the STT priority gate](#the-stt-priority-gate). That is not a failure and does
not touch the backoff.

### Surviving a restart

The other half of the same incident. Playback is one global in-memory session, so
a container restart emptied it: `book` was `None` until something called
`/api/load`, and a reader that was already mid-chapter never does — loading is
what *picking* a book does. In that state `/api/chunk/{ci}/{i}.wav` (which is
session-scoped) 404s, `/api/open`, `/api/playhead` and `/api/chapters?book=`
answer 409, and the worker has no plan to render from. The reader sat on a 404
that would never become a 200, silently, showing a healthy connection.

Everything needed was already on disk — the plan in `plan.json` with its
`parse.json` stamp, the position in the vault. The only missing thing was the
name of the book, which is now `work/session.json`, written by `/api/load` and
read back by `narrator::boot` at startup. Restoring costs one plan read (0.30 s
on the 1433-chapter book) and **never a parse**: a stamp that no longer matches
means the book changed, and re-chunking it here would move every stored position
in it. It also does not start the renderer — a process that comes up rendering a
book nobody is reading is a worse failure than the one being fixed. What starts
it is the reader's next `/api/playhead`, which now wakes the worker if nothing
has started it in this process (gated so an explicit `/api/renderer {"on":
false}` is not undone by a chunk advance).

The reader has a backstop for the cases the server cannot restore: `hello` fires
on every reconnect and names the book, so a `hello` naming *no* book while this
device has one open is an unambiguous "the session is gone", and the reader
re-issues `/api/load` and tells it where it is (`lostSession` in
`web/src/lib/live.ts`, rate-limited to once per ten seconds so a flapping tunnel
is not a load per flap). Deliberately narrow: a `hello` naming a *different* book
is another device having loaded one, which is the existing one-session-at-a-time
behaviour and not this reader's to undo — two readers healing a mismatch would
take turns kicking each other's book out.

### The API

Every path, method, field name, type and nullability of the Python contract, asserted against `tests/fixtures/api_contract.json` — a transcription of `AGENTS.md` + `app/server.py` + `web/src/lib/types.ts`, written down rather than generated here, so it can catch *this* server drifting.

25 paths. `/api/events` (SSE) is implemented over a tokio broadcast channel with the same wire format, the same coalescing of `progress` and the same refusal to replay. `/healthz` returns 503 with `problems[]` for the failures that actually happen: a dead render thread, a "rendering" status with no chunk in `HEALTH_STALL_S`, an unwritable work dir.

## What this server does that the Python one does not

The reader kept a list of nine requirements written against the Python server,
each with the client-side mitigation standing in for it meanwhile. All nine are
implemented here, all nine are **additive** — a client that sends none of the new
parameters gets exactly the Python behaviour, which is what keeps the Obsidian
plugin working untouched — and each has a test in `tests/reader_requirements.rs`.
The list is gone; this is what it said, and what answered it.

1. **`/api/load` does not re-parse an unchanged book.** Measured at 12.4 s per
   call on the 1433-chapter *Lord of Mysteries*, and the reading position comes
   back in that response, so first paint on a new device waited for all of it.
   The plan was already being written to disk for `narrator export`; what was
   missing was a way to know it is still valid, which is the `parse.json` stamp
   beside it — source path, size, mtime, `max_chars`, format version. All match,
   and the plan is read back instead of rebuilt; anything differs and the book is
   parsed and the text bundle rebuilt exactly as before. Because the stamp is on
   disk, a container restart is fast too, not just a second call.
2. **`/api/chapter/{ci}` honours `?book=`.** The Python one answers for whichever
   book the process last loaded and ignores the query — the one asymmetry that
   coupled the reader's first paint to `/api/load` at all. Here a key that is not
   the loaded book is served straight out of that book's text bundle, one shard,
   no session.
3. **A returned position carries `updated_ms`.** `updated` stays the naive local
   stamp, byte-identical, because that is what goes in the vault; the instant
   rides along beside it, resolved here where the server's zone is actually
   known.
4. **`/api/open` and `/api/playhead` take an optional `book`.** Supplied and
   mismatched, they answer **409** instead of dragging another book's render
   frontier around and filing a position under the wrong name.
5. **An unwritable positions directory is a health problem.** `/healthz` reports
   `vault` and `positions_dir` and probes the latter. The failure this prevents
   is the nastiest one in the list: with `NARRATOR_VAULT` pointing at a path that
   does not exist, every write fails, every read returns `{}`, and every book
   quietly opens at chapter one.
6. **`/api/chapters` takes `?from=&to=`.** Inclusive, clamped, with `total`
   alongside so a windowed response still says how big the book is.
7. **Every chapter endpoint takes the book.** `?book=` on `/api/chapters`, a
   `"book"` field on `/api/chapters/{render,build,cancel}`, and the same **409**
   on a mismatch as `/api/open`. This is the endpoint where the race is
   expensive rather than merely wrong: one tap can queue 74 chapters, and if the
   server swapped books between the reader's poll and the reader's tap — the
   watcher picked up an epub, the plugin opened something, a second device
   loaded another book — those renders occupy the worker for hours on the wrong
   novel. The reader had no mitigation that closed it; this does.
8. **The packed bitrate is reported.** `/api/status` carries `bitrate` (`"64k"`)
   and `bitrate_bytes_per_min` (480000), and every `/api/chapters` row carries
   `est_bytes` — the server doing the arithmetic with the constants it actually
   has. The reader used to hard-code the 64 kbit/s default, so changing
   `CHAPTER_BITRATE` on the box made every size in the UI wrong by that ratio,
   silently.
9. **`/api/chapters/build` says what it refused, and why.** A fourth list,
   `refused`, with one entry per chapter the packer did not take:
   `not_rendered` (carrying `rendered`/`n`, and the chapter is queued to render
   instead), `no_chunks`, `out_of_range`. Before, a refused chapter appeared in
   none of `built`/`building`/`rendering` and was indistinguishable from one
   nobody asked about, so the reader ignored the response entirely and re-asked
   every twenty seconds. It now reads the answer and keeps the repeat for a row
   that has not moved in ninety seconds, which is what a lost call actually
   looks like.

## OpenAPI is the contract

Handlers carry `#[utoipa::path]` and every request/response is a typed struct, so the document is **generated from the code**, not maintained beside it.

```bash
./target/release/narrator --openapi > openapi.json   # no server, no work dir
./scripts/gen-client.sh                              # spec → web/src/client
./scripts/gen-client.sh --check                      # ... and fail on any difference
```

**The reader is on that client.** `web/src/lib/api.ts` calls the generated
functions and `web/src/lib/types.ts` is a handful of aliases over the generated
types (`ChapRow` = `ChapterRow`, `SavedPosition` = `StampedPosition`), so there is
no longer a hand-written description of this API anywhere in the tree: a field
that moves in Rust is a TypeScript error in the same change. What stayed
hand-written in the reader is policy no generator knows — the offline-first
retry rules, and the URL builders for `<audio src>`, HLS and Cache Storage keys,
which are strings rather than calls (the generated client encodes query values
with `encodeURIComponent` too, so a builder URL and an SDK URL for the same
resource are byte for byte the same request).

That leaves one thing worth checking, and CI checks it: **is what is committed
what this server generates?** `--check` regenerates and diffs against git.
`release.yml` runs it with the debug binary (a spec change in Rust that was not
carried through is a red build); `web.yml` runs `--from-spec --check`, which
regenerates the client from the committed spec and catches a hand-edited
generated file on a push that never touches Rust.

The old field-by-field drift check — which compared the generated types against
the reader's hand-written ones through the TypeScript compiler — is gone with the
hand-written types it compared against. It earned its keep first: 29 mismatches
on its first run, all real. utoipa renders `Option<T>` as both nullable *and*
absent from `required`, while serde without `skip_serializing_if` always sends
the key — so the spec was lying about eleven `Status` fields. Fixed with
`#[schema(required = true)]` and `value_type` overrides on the fields the server
genuinely always sends. **That rule stands: if serde will always serialize it,
say so in the schema** — the reader's types are generated from those annotations
now, so getting one wrong is no longer a report, it is a bug in the reader.

Five paths carry a file extension after their parameter (`/api/chunk/{ci}/{i}.wav` and friends). matchit matches whole segments only, so those are routed by hand against `{file}` and split their own suffix; the *document* still carries the real contract URL, because that is what the client is generated from.

## Live updates: `/api/events`

One long-lived `text/event-stream` per client, fed by a tokio broadcast channel
(`src/events.rs`) and served by `src/api/stream.rs`. It is how anything learns that anything
changed — the heartbeat poll dropped to a fifth of its rate behind it, and the
chapter drawer's two-second poll survives only as a backstop while it is open —
and the design rule is one sentence:
**an event is not state.** Every event means "this changed, go and look", which
is what makes a dropped one — a phone the server lagged, a reconnect — a
non-event rather than a lost update. Nothing is replayed, and `Last-Event-ID` is
accepted and ignored on purpose.

| event | when | payload |
|---|---|---|
| `hello` | on connect, and on every reconnect | `{heartbeat_s, book, key, chapter}` |
| `position` | every time a position is *written* (so at the 15 s throttle, and forced on open/pause) | the vault record plus `book` and `source`: `session` for the loaded book's playhead, `api` for a named `/api/position` write |
| `render` | the worker moved | `{kind, key, chapter, render_idx, playhead, n, status}`; `kind` is `progress` (throttled to `SSE_RENDER_MIN_S`, default 1/s), `complete`, `chapter`, or `packed` (which also carries `ok` and an `error`) |
| `books` | an epub appeared in or left either book root, debounced 500 ms | `{changed: [names], count}` |
| `note` | a voice memo was transcribed and filed | `{file, book, chapter, chunk, language}` |

Plus what is not an event: `: narrator live` on open, `retry: <SSE_RETRY_MS>`,
`: ping` every `SSE_HEARTBEAT_S`, and `: lagged N` for a subscriber that stopped
reading. Those comments are the point of the heartbeat — bytes on the wire prove
a tunnel that is still there, which `navigator.onLine` does not.

**A slow client never backpressures the renderer.** `broadcast::Sender::send`
does not block and does not fail on a full queue; a receiver that falls behind
gets `Lagged(n)`, is told so as a comment, and carries on with what is left.
`SSE_QUEUE` (64) is the depth. Tested both ways in `tests/parity_events.rs`: at
the bus (twenty events into a four-deep queue, emit never fails, the reader sees
one lag and then the last four) and over the wire (a stream nobody is polling
gets `: lagged`, and still delivers the next event).

The `books` event is why `src/watch.rs` exists: both book roots are watched
(`./books` and the vault's `03 - Resources/Books`), so an epub the vault's git
sync pulls onto the server appears in the library with nothing to call and no git
coupling. A root that does not exist is skipped rather than fatal.

### The reader's half

`web/src/lib/live.ts` is the client: a typed `EventSource` wrapper that validates
each payload (this is the one part of the API that is *not* generated — its body
is a stream, so there is nothing for openapi-ts to type) and turns each event into
a TanStack Query invalidation. `hello` refetches everything, because a reconnect
by definition missed whatever happened while it was down. Reconnection is the
browser's own: `EventSource` retries on the server's `retry:` interval, and the
connection indicator in the top bar is fed from the stream's state rather than
from a failing poll.

`position` is the one event that is not a refetch, because the reading position
is not a query — it is the page in front of someone. The arbitration
(`arbitrate`, unit-tested in `live.test.ts`):

- another book, or within two chunks of where this device already is (its own
  echo, a chunk or two stale by the time it arrives) → **ignore**
- somewhere else, and nothing is playing here → **follow**: this is the
  phone-down, laptop-up case, and it is the whole feature
- somewhere else, and this device *is* playing → **offer**: a quiet line above
  the player bar ("moved on another device · <chapter>", follow / dismiss).
  Jumping the page mid-sentence because a phone in another room saved a position
  would be the worst thing this feature could do.

The heartbeat query (`/api/status`) survives at a fifth of its old rate — 5 s
instead of 1 s — as the fallback for a browser with no live stream and the source
of the few numbers no event carries.

## Downloading a chapter, end to end

A download is two stages — render every chunk, then pack the chunks into one
m4a — and three parties: the server's render worker, the server's packer, and
the device that has to end up holding the file. The design rule for the whole
pipeline is that **no stage may depend on the app still being open**, because on
the A1 a 74-chapter download is an overnight job and iOS suspends a PWA within
seconds of the screen going off.

### The order: `pack: true`, and `work/audio/<key>/queue.json`

`POST /api/chapters/render {"chapters": [...], "pack": true}` is the whole
request. Additive in both halves — a client that sends neither the flag nor
`book` gets the python behaviour — and it means two things:

- the chapters go in the render queue, as before;
- each is marked `build_want`, so the worker hands it to the packer the moment
  its last chunk lands (`next_queued` in `src/render.rs`). **Nothing else has to
  be called.** The reader used to send one `/api/chapters/build` per chapter when
  it saw the row reach "fully rendered", which is why closing the app used to
  leave a night of rendered chapters and not one file.

`/api/chapters/build` takes the same flag's meaning implicitly: a chapter it has
to render first is queued with `pack: true`.

The order survives the process in `src/wishlist.rs` — one `queue.json` per book,
beside its `plan.json`, written `.part` → fsync → rename → fsync-dir on every
mutation, resumed by `narrator::boot` after the session restore and re-adopted by
`/api/load`. **Intent only, never progress**: which chapters, in what order, and
whether each wants packing. Completion stays disk truth, so a chapter finished
while the process was down simply leaves the queue the first time the worker
looks at it. A chapter picked back up by five restarts with no chunk landing is
*parked* — kept, logged, reported as `ChapterRow.parked`, retried by asking
again. The details, including why the file is per-book, are in that module's doc.

**What is deliberately not persisted: `autopack`'s own speculative orders.** The
packer also packs the chapter being read and the one after it, from the worker's
idle branches — organic listening rather than anything anybody asked for. Those
are re-derived from the current chapter on every boot and are *not* written to
the wishlist. Only a user-requested download is durable, which is the whole
distinction: an order somebody placed outlives the process, a guess about what
might be useful next does not.

### The CPU policy: pack when the renderer is ahead or idle

Two ARM cores, Kokoro at a quarter of realtime, and now packs that arrive with no
client watching — a resumed wishlist can fill the pack queue seconds after boot.
So the packer has a second hold-back below [the STT gate](#the-stt-priority-gate),
with the same shape and the same reason one rank down:

- the renderer sets `render_stalled` when it takes **rule 1** — the chunk under
  the playhead is missing, so somebody is waiting on that exact chunk *now* — and
  clears it on any other branch;
- the packer holds a **new** encode back while that flag is set. One already
  running is left to finish, for the same reason a memo does not kill it: an
  encode abandoned mid-chapter costs the box everything it has spent.

Only rule 1 counts as "behind". The lookahead is 80 chunks and also reports
`status: "rendering"`, and a packer that waited for *that* would never run on
this box at all. The hold is bounded at `PACK_HOLD_MAX_S` (30 s): past it the
renderer is not slow, it is wedged, and a download that waits forever on a wedged
renderer would be a worse bug than the one being prevented.

### The device's half: the foreground reconciliation sweep

The one step that can only happen on the phone is copying the m4a into Cache
Storage, and the phone is exactly what is not running. **Backgrounded work is not
attempted** — that is the accepted platform limit, not something to fight — so
instead every way back into the app asks one question, and
`web/src/lib/reconcile.ts` is that question:

> of the chapters still pending, which are packed and not here yet?

Three terms, each from the only place that knows it: the pending selection from
IndexedDB (`downloads`, beside the outbox, so it survives the app being killed),
`m4a` from the server's rows, and Cache Storage — *asked*, never remembered, so a
quota eviction reads as missing. The triggers are `visibilitychange`, `focus`,
the network returning, a cold launch, and `hello` off the live stream, which is
the "we were away" signal for a tab that never went hidden. One chapter that will
not come down does not cancel the rest, and what is still wanted is recomputed
from Cache Storage afterwards rather than from what the sweep thinks it stored.

The selection is written down *before* the first request, or a download confirmed
as the screen locks would be one nobody remembers. It is removed per chapter as
each lands, and the record is deleted when the last one does.

**Polling stays.** The drawer's ladder (`web/src/lib/download.ts`) is unchanged
and still runs rung by rung while the app is open — it is the fast path and the
fallback for a session with no live stream. The sweep is the catch-up pass, not a
replacement for it.

## The `.m4b` export

`narrator export --book <file.epub> [--partial]` packs the rendered chunks in the
work directory into one AAC `.m4b` under `work/export/<Title>.m4b`: chapter marks
from the *real* wav durations (so a seek in a player lands where the chapter
does), the EPUB's title, author and cover art embedded, the same inter-chunk
(0.30 s) and paragraph (0.60 s) gaps the chapter packer uses, and 1 s between
chapters.

It is a **CLI path, not a server one**, exactly as `app/export.py` was: a
minutes-long ffmpeg run producing a file for a different device entirely (Apple
Books, a car), with nobody waiting on a response, and it has to work against a
work directory whose server is not running. What it needs is what the server
already leaves behind — `plan.json`, read raw rather than through the parse cache,
because an export of audio on disk has no business refusing to run because the
epub's mtime moved.

On the box, where the cache actually is, it runs inside the container the server
is already using:

```bash
docker exec narrator-rs narrator export --book "/vault/03 - Resources/Books/…/Title (2016).epub" --partial
# → /work/export/Title.m4b, i.e. /home/ubuntu/narrator/work/export/ on the host
```

A chapter with one missing chunk is not packable: the gap would swallow the hole
and every chapter mark after it would be wrong. So it is left out and named, and
without `--partial` the whole run refuses rather than quietly shipping a book
with holes. `--dir`, `--out`, `--bitrate`, `--title`, `--author` and the three
gap widths are the python script's flags, spelled the same way.

## Engine notes

**Kokoro through ONNX.** `onnx-community/Kokoro-82M-v1.0-ONNX`, fp32 `model.onnx`, driven by `ort`. Three inputs: phoneme token ids wrapped in the boundary token `$`, a 256-float style vector picked out of the voice pack **by phoneme count** (that is how Kokoro gets its pacing right), and a speed scalar. Output is f32 mono at 24 kHz — the same rate and layout `app/tts.py` produces, so nothing downstream knows which engine rendered a chunk.

**espeak-ng is a subprocess, not a library.** libespeak-ng keeps process-global state, is not thread-safe, and a wedge or a segfault inside it would take the server with it. A fork costs a few milliseconds against a chunk that takes a second or two to synthesize, and it **is** killed on a timeout — `ESPEAK_TIMEOUT`'s 15 s, spent polling rather than in `Command::output()`, which waits forever and for a while did. The pipes are drained on their own threads, because a child that fills the 64 KB pipe buffer while the caller polls for its exit is the same hang by another route. `probe()` is bounded too: it runs inside `Engine::load`, on the render thread. It also deletes the Python image's entire `espeakng_loader` symlink surgery: the apt binary and its data are simply what `espeak-ng` on PATH resolves to and can never be a mismatched pair.

**G2P is the fallback half of misaki, not all of it.** Kokoro's real front end looks English words up in a lexicon first and only falls back to espeak-ng; this implements the fallback for every word, with misaki's `EspeakFallback.E2M` mapping table verbatim. Measured cost on the GATE 0 passage: 269.25 s against the PyTorch render's 271.57 s (0.9 %), with per-chunk boundaries lining up.

**The lexicon half: what a port would take, and why it has not been done.** It is
not a lookup table. misaki's `en.G2P` is a ~1200-line front end around two gold
lexicons (`us_gold.json` + `us_silver.json`, ~4 MB of JSON) plus:
a spaCy `en_core_web_sm` POS tag per token — a 12 MB statistical model, and the
only reason `read`, `lead`, `live`, `bow`, `close`, `record` come out right;
`num2words` for every number, ordinal, year, currency and decimal; a stress
system that re-marks a word by its part of speech and its position in the
sentence; and a stack of special cases (contractions, possessive `'s`, currency
symbols, `Mr`/`Mrs`, acronyms, hyphenation). The lexicon lookup alone, without
the tagger, would be worse than the fallback — it would give `read` one
pronunciation and be confidently wrong half the time, where espeak-ng at least
guesses from context. So a faithful port is a POS tagger in Rust or an ONNX
export of one, plus number expansion, plus the stress rules: days of work, a
~16 MB asset addition to the image, and a new class of divergence from the
Python render to test against. **Not attempted**, deliberately. The measured
difference today is 0.9 % of duration with boundaries lining up, and no word has
actually been reported as wrong. If one ever is, the cheap fix is a small
override table (a JSON map of word → phonemes consulted before espeak-ng),
which is an afternoon and carries none of the above.

**`KOKORO_GAIN`, and the delta it exists for.** The ONNX render is uniformly
~1.4× louder (≈ +3 dB) than `work/kokoro-test/kokoro_af_heart.wav` — same timing,
same prosody, a flat gain; one sample of 6.5 million clipped. Whether that is
wrong is Fernando's ear to decide, so what exists is the knob rather than a
decision: `KOKORO_GAIN` (default **1.0**, the identity — it changes not one byte
of a rendered chunk) multiplies every synthesized chunk post-synthesis, with a
soft knee at 0.95 so it cannot clip. Everything below the knee is a plain
multiply; above it, the remaining headroom is a `tanh`, which is smooth,
monotonic and can never leave ±1.0. Multiplying a waveform that already touches
±1.0 and then clamping is how a flat gain becomes audible distortion on exactly
the loudest words, which is the failure this avoids. `KOKORO_GAIN=0.71` would
undo the measured delta.

**It is not set on the box.** The verdict has not been given, and a gain applied
to chunks already rendered at another gain would make a book that changes
loudness at the frontier.

**Whisper.** `large-v3-turbo-q5_0` ggml, CPU, serialized behind one mutex like the Python's lock. Language auto-detect, `WHISPER_PROMPT` + book title + chapter title as the initial prompt, silero VAD when `models/whisper/ggml-silero-v5.1.2.bin` is present. `WHISPER_MODEL` takes either a bare name (resolved to `<models>/whisper/ggml-<name>.bin`) or a path. `WHISPER_THREADS` sets the thread count; it defaults to every core, which is what it was before it was configurable — see [the measurement](#whisper-on-the-a1).

## Performance

Measured on this machine (16 x86 cores), chapter 1 of *Lord of Mysteries*, 33 chunks, af_heart:

| | RTF |
|---|---|
| Python + PyTorch Kokoro | ~4.5× realtime (AGENTS.md) |
| Rust + ONNX Kokoro, `listen_test` | **4.31×** |
| Rust + ONNX Kokoro, in the container, 7 Powers ch. 2 | **4.87×** |
| Rust + ONNX Kokoro, in the container, *Lord of Mysteries* ch. 1 | **4.67×** |

So: parity on synthesis, within noise — on a desktop. The box is a different
machine and a different conclusion; see [the A1](#the-a1-measured).

Everything *around* the synthesis is a different story, and it is where the
reader's waiting actually was. On the 1433-chapter *Lord of Mysteries*
(7.8 MB epub, 16.8 MB of text, 12 shards), in the container:

| | Python | Rust |
|---|---|---|
| `POST /api/load`, cold | 12.4 s (measured by the reader) | **0.73 s** |
| `POST /api/load`, unchanged book | 12.4 s again | **0.36 s** (no parse at all) |
| `GET /api/chapters`, 1433 rows | a few thousand `stat`s per poll | **9 ms**, 244 kB |
| `GET /api/chapters?from=0&to=30` | not available | **12 ms**, 5.3 kB |

The cold parse being seventeen times faster is just Rust against ebooklib + BeautifulSoup + lxml; the second one is [the parse cache](#what-this-server-does-that-the-python-one-does-not).

### The A1, measured

The Oracle A1 — 2 cores, 11 GB — is the machine that matters, and none of the
numbers above came from it. These do, taken on the running box while it was
rendering *Lord of Mysteries* (which is the normal condition, not a spoiled
measurement: this box is always rendering something):

| | A1 | this 16-core x86 |
|---|---|---|
| Kokoro synthesis (`/api/status`'s `rtf`, cumulative over hours) | **0.26–0.30×** realtime | 4.3–4.9× |
| `GET /api/chapters`, 1433 rows | 25 ms cold, 7 ms memoised, 273 kB | 9 ms, 244 kB |
| `GET /api/chapters?from=0&to=30` | 2–26 ms, 6.0 kB | 12 ms, 5.3 kB |
| `GET /api/chapters?from=700&to=760` | 1.5–4.7 ms, 11.8 kB | — |
| `GET /api/status` | 20 ms, 631 B | — |
| `POST /api/load`, cached plan | 0.30 s | 0.36 s |

**Synthesis runs at about a quarter of realtime here** — roughly four seconds of
compute per second of audio, against 4.5× *faster* than realtime on the desktop.
That is the most consequential number in this file, and it is not a regression:
it is 2 ARM cores against 16 x86 ones. What it means is that the box can never
render a book as fast as anyone listens to it, so the prerender span, the
download-ahead drawer and the chapter queue are not optimisations — they are the
only reason the reader works at all. A 12-minute chapter is the better part of an
hour of rendering, and a 74-chapter download is an overnight job.

**`/api/chapters` pagination: measured, and not needed.** The full 1433-row
response is 273 kB and 25 ms cold on the A1 — 7 ms while the 1.5 s memo holds,
which is most polls — and the drawer polls it every two seconds only while it is
open. A cursor would add API surface, a second code path through the row scan and
a new way for the reader's list to be half-built, to save a quarter of a
megabyte over a tailnet on the one screen that is *about* those rows. The window
that already exists (`?from=&to=`, requirement 6) covers the case that actually
wanted paging — a reader that only shows a screenful — at 6 kB and 2 ms. So: no
cursor, deliberately, until a number says otherwise.

### Whisper on the A1

**Measured at last, and it is the bad news of this round.** A 10.0 s memo
(synthetic speech, webm/opus, exactly the payload the reader posts), against a
throwaway container on the box with a temp work dir and no vault mounted:

| `large-v3-turbo-q5_0` | wall | × realtime |
|---|---|---|
| 2 threads (the default: every core), first call — pays the 574 MB model load | 311 s | 31× |
| 2 threads, second call, model resident | 473 s | 47× |
| 1 thread (`WHISPER_THREADS=1`), model resident | 534 s | 53× |

The transcript was word-perfect — `large-v3-turbo` is a good model and the prompt
biasing works — and that is the only good part. **Thirty to fifty times realtime
means a one-minute memo is half an hour of CPU.** Two threads beat one, so the
default (every core) stays; but the second call being *slower* than the first,
with the model already resident, says what the real constraint is. The renderer
had the other core. Whisper took about one core's worth (≈ 95–99 % of one CPU)
either way, because on a 2-core box the other core is always busy synthesizing,
and every one of these numbers was taken with it busy — which is the honest
condition to measure in, since that is when a memo actually gets recorded. The
thread count is not the lever.

The lever is the model, and the options are:

1. **A smaller ggml model.** `small` is ~6× less compute than `large-v3-turbo`
   and `base` ~20×; either would bring a 10 s memo under a minute. The cost is
   accuracy on proper nouns — which `WHISPER_PROMPT` plus the book and chapter
   titles already exist to patch. This is the obvious first move: change
   `WHISPER_MODEL` in `/etc/narrator-rs.env`, fetch the ggml file into
   `models/whisper/`, restart. **Not done here** — it is a quality trade only
   Fernando can judge, and it wants his ear on a real memo.
2. **Transcribe somewhere else.** The memo is already a queued blob in the
   reader's outbox; nothing says the transcriber has to be the reader's server.
   A bigger machine (his desktop) with the same endpoint would do, at the cost
   of a second deployment target.
3. **Accept the latency.** Nothing breaks today: the outbox holds the recording
   until a 2xx comes back, the note is written minutes later, and Fernando is
   not waiting on the screen for it. The failure mode is not data loss, it is a
   note that appears in the vault five minutes after the thought.

What is *not* an option is leaving this undocumented, which is why it is here:
the voice-memo path works, and it works at a tenth of the speed anyone would
guess from the desktop numbers.

### The STT priority gate

**Whisper outranks the renderer and the packer, and they stand down for it.**
The measurement above is the argument: two ARM cores, Kokoro already at 0.26×
realtime, whisper at 30–50×, and with twenty chapters queued a ~45 s memo took
**over seven minutes** while the render RTF fell to 0.26. Neither job got a
machine.

The tie-break is not speed, it is what is recoverable. A memo exists in exactly
one place — IndexedDB on the phone that recorded it — until `/api/note` answers
2xx. A rendered chunk is a file the server can make again from a book it still
has. So:

* `Whisper::transcribe` takes an RAII claim on `SttGate` (`src/stt.rs`) at its
  very first line, and holds it across the ffmpeg decode, the **wait** for the
  model mutex, and the transcription. Covering the wait is the point: two memos
  arriving together park the renderer once, for both, instead of letting it wake
  up in the gap between them.
* The render worker parks between chunks — a bounded wait on the gate's condvar,
  so it restarts the instant the last transcription ends. Parking is **not** a
  render failure and does not touch the failure backoff.
* The packer holds back a *new* encode. One already running is left to finish:
  killing ffmpeg mid-chapter throws away every second it has spent and the
  chapter has to be packed again from nothing, which costs the box more than the
  transcription gains — and the packer is one chapter at a time, so the wait is
  bounded by one encode either way.
* `/healthz` does not call a parked renderer a stall. A one-minute memo is half
  an hour of not rendering on the A1, comfortably past `HEALTH_STALL_S`, and the
  watchdog restarting the container over it would kill the transcription every
  time it was retried. A stall that is explained is not a stall.
* Both threads log the park and the resume with its duration. A renderer that has
  quietly stopped is the exact shape of the bug this round was about; "it is
  parked for a memo" is only reassuring if it is written down.

Release is `Drop` and nothing else — no manual path to forget. That is also what
makes it safe under cancellation: `/api/note` transcribes inside
`spawn_blocking`, and a phone that locks mid-memo drops the *request future*
while the blocking job runs on. The claim belongs to the closure, not the future,
so it goes when the work does. `tests/parity_render.rs` asserts exactly that,
along with the renderer parking and resuming and the packer holding back.

`WHISPER_THREADS` still defaults to every core, and on the A1 that finally means
something: with the renderer parked, both cores are actually free for the
transcription rather than one of them being Kokoro's.

## Config surface

Every name the Python `AGENTS.md` documents, with the same default: `NARRATOR_PORT` (7870), `NARRATOR_VAULT`, `NARRATOR_WORK`, `NARRATOR_BOOKS`, `NARRATOR_WEB`, `BOOKS_SUBDIR`, `POSITIONS_SUBDIR` (`02 - Studies`), `NOTES_SUBDIR` (`05 - Fleeting`), `KOKORO_VOICE` (`af_heart`), `KOKORO_SPEED`, `KOKORO_GAIN` (1.0), `LOOKAHEAD` (80), `PRERENDER_CHAPTERS` (2), `PREFETCH_WHILE_PAUSED`, `MAX_AUDIO_GB` (5), `MAX_CHAPTER_GB` (20), `SILENCE_S` (0.5), `WHISPER_MODEL`, `WHISPER_PROMPT`, `WHISPER_THREADS` (every core), `CHAPTER_BITRATE` (`64k`), `CHAPTER_GAP_S` (0.30), `CHAPTER_PARA_GAP_S` (0.60), `HLS_SEGMENT_S` (6), `TEXT_SHARD_BYTES`, `TEXT_SHARD_CHAPTERS`, `HEALTH_STALL_S` (300), `AUTOPACK`, `AUTOPACK_EVERY_S`, `NARRATOR_WATCH_BOOKS`, `NARRATOR_FAKE_TTS`, `SSE_HEARTBEAT_S`, `SSE_QUEUE`, `SSE_RENDER_MIN_S`, `SSE_RETRY_MS`.

New here, because the weights are not downloaded by a Python package on first use: `NARRATOR_MODELS` (`/models`), `KOKORO_MODEL`, `KOKORO_VOICES`, `WHISPER_VAD_MODEL`, `ESPEAK_BIN`, `ESPEAK_VOICE`, `ESPEAK_TIMEOUT` (15 s, seconds, after which the subprocess is killed), `QUEUE_RESUME_DELAY_S` (10 s, how long after startup a wishlist left by the last process may start rendering — the note queue's own startup sweep may be claiming one of the box's two cores). `HF_HOME` and `KOKORO_REPO` are gone — nothing here talks to Hugging Face at runtime.

A value that will not parse logs a warning and falls back. A typo in an env var is not a reason to refuse to start a reader.

## Docker

### Two images, server and reader

The repo publishes **two** images to GHCR, and the split exists for one reason:
the server takes the better part of an hour to build (whisper.cpp, twice, for
amd64 and arm64) and the reader takes eight seconds. Fernando edits the reader
far more often than the server, and a CSS fix should not wait on a Rust
compiler.

| Image | Built by | What it is |
|---|---|---|
| `ghcr.io/fernandobandeira/hearsay` | `Dockerfile`, `.github/workflows/release.yml` | The server. Multi-arch, built natively on both runners. Its node stage still builds `web/`, so the image carries a reader at `/web` and `docker run`ing it alone is a whole working thing. |
| `ghcr.io/fernandobandeira/hearsay-web` | `web/Dockerfile`, `.github/workflows/web.yml` | The reader's `dist/`, and nothing else: `FROM scratch`, contents at `/dist`, ~600 KB. Not runnable — it is a file delivery mechanism with a registry in front of it. |

`release.yml` carries `paths-ignore: web/**` and `web.yml` carries
`paths: web/**`, so a push touches one pipeline or the other. Both are
test-gated identically: the image only builds on green (the reader's gate is
`vitest` + `tsc -b`, and `release.yml` runs those too, because a reader that
does not typecheck is a server image that does not build).

`hearsay-web` is built for both architectures even though its payload is
architecture-free — the node stage is pinned to `$BUILDPLATFORM` so it runs once
natively, and the second manifest is pure metadata. The cost is nil and the box
(arm64) pulls without a `--platform` flag or a platform-mismatch warning.

### The server image

Three stages: node builds the reader, cargo builds the server behind a manifest-only dependency layer (so a source edit does not recompile whisper.cpp, which is most of the build), and a `debian:bookworm-slim` runtime carrying one binary plus ffmpeg, espeak-ng and `libgomp1` (ONNX Runtime's CPU provider is OpenMP-threaded). 583 MB.

`HEALTHCHECK` is `narrator --healthcheck`: one loopback GET of `/healthz` written against a `TcpStream`, rather than putting curl in the runtime image for a single request.

**amd64 is built and run end to end**, including the real reader: the production
build of the React app, unmodified, boots against this server and goes all the
way through — library, `/api/load`, the 1433-chapter list, the text bundle's
twelve shards, `/api/open`, per-chunk wavs, auto-packing, and finally HLS
playback with 206s on the segments. Zero failed requests, zero console errors.
Notably it already sends `?book=` on `/api/chapter/{ci}`, which the python server
ignores — requirement 2 below paid for itself on the first run.

Memory while rendering *Lord of Mysteries* settles: 599 MB idle with the model
loaded, climbing to ~1.24 GB over five minutes of continuous rendering and then
flat to within 4 MB while still working. That is ONNX Runtime's arena reaching
its working set, not a leak.

### Deploying it

`deploy/` carries the templates. The `docker run` the unit performs, spelled
out:

```
docker run --rm --name narrator-rs \
  -p 127.0.0.1:7870:7870 \
  -v /home/ubuntu/narrator/work:/work \
  -v /home/ubuntu/narrator/books:/books:ro \
  -v /home/ubuntu/narrator/models:/models:ro \
  -v /home/ubuntu/vault:/vault \
  --env-file /etc/narrator-rs.env \
  narrator-rs:latest
```

with `/etc/narrator-rs.env` holding `NARRATOR_VAULT=/vault`,
`MAX_AUDIO_GB=50`, `KOKORO_VOICE=af_heart`,
`WHISPER_MODEL=large-v3-turbo-q5_0` and the rest (`deploy/narrator-rs.env`).
Port 7870 is bound to **loopback only** — there is no auth and no TLS, the box
is reached over Tailscale, and publishing that port is the one change that turns
a private reader into a public one. `deploy/narrator-rs-watchdog.{sh,service,timer}`
mirror the python repo's: curl `/healthz` every two minutes, restart after three
consecutive failures, with a fifteen-minute floor so a crash loop cannot hide
the problem.

The work dir and the vault are **adopted in place**: same cache paths, same
`plan.json`, same `.narrator-positions.json` and `Reading Log.md`. Nothing is
re-rendered and no position moves — that is what the parity suites above are
for.

### Deploying the reader

The unit mounts `-v /home/ubuntu/web:/web:ro` over the reader baked into the
server image, so what the box actually serves is a directory of files on disk.
`deploy/hearsay-web-update.{sh,service,timer}` keep that directory current:

    pull hearsay-web:latest → compare the image id to /var/lib/hearsay/web.digest
    → unchanged? exit
    → changed? docker create (never started) · docker cp /dist → a staging dir
      · chown ubuntu:ubuntu · rsync into /home/ubuntu/web · stamp the digest

Every ten minutes, plus two minutes after boot. **So a UI deploy is `git push`**
— web.yml builds and pushes the image, the timer lays it down within ten
minutes, and `sudo systemctl start hearsay-web-update` on the box is the same
thing now.

Three details that are load-bearing, not taste:

- **The contents are synced, the directory is not replaced.** `/home/ubuntu/web`
  is a bind-mount *source*: the running container holds the inode it started
  with, so a `mv` of a freshly built directory into place would leave the server
  serving files nobody can reach. rsync writes each file to a temp name and
  renames it, so an update is atomic per file with the container none the wiser
  — and narrator-rs is never restarted for a UI change.
- **An empty directory is an empty reader.** The mount shadows the image's copy
  unconditionally; the server does not fall back. The updater refuses to install
  anything without an `index.html`.
- **Offline is not a failure.** A pull it cannot do logs a line and exits 0. The
  box keeps serving what it has, and picks the change up on a later tick.

Install (by hand, like everything else in `deploy/`):

```bash
sudo install -m 0755 hearsay-web-update.sh /usr/local/bin/
sudo cp hearsay-web-update.service hearsay-web-update.timer /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now hearsay-web-update.timer
```

## What has and has not been proven

This server *is* production: the box runs the published arm64 image, the reader
runs against it, and the cache and the vault it adopted are the same ones the
python server left. What that sentence does not cover:

- **The listening verdict is Fernando's and has not been given.** GATE 0 is
  rendered and waiting (`./narrator listen-test`), and the ≈ +3 dB delta above is
  still undecided — which is why `KOKORO_GAIN` exists and is **not** set on the
  box. Setting it mid-book would make a novel that changes loudness at the render
  frontier, so it wants a decision and a re-render, not a quiet flip.
- **The soak is hours, not days.** The box has now rendered *Lord of Mysteries*
  for hours at a stretch under real use, with a 1433-chapter cache and a
  20-chapter download queue, and the endpoint numbers above were taken in the
  middle of it. Resident memory on the box settles around 1.9–2.3 GB while
  rendering (the desktop reaches ~1.24 GB in five minutes; the box has been at it
  far longer, and 11 GB of RAM makes the difference academic) — consistent with
  ONNX Runtime's arena rather than a leak, but *consistent with* is not *proven
  not to be*. What still has no evidence either way: gc churn at the
  `MAX_AUDIO_GB` cap over days, and whether anything drifts across thousands of
  sessions.
- **The A1 renders at a quarter of realtime, and whisper there is 30–50× it.**
  Both are measured now ([Performance](#the-a1-measured)), and neither is a bug —
  they are 2 ARM cores. The consequence is a design constraint rather than a
  to-do: nothing about this reader can assume the server keeps up with a
  listener, and the voice-memo path is minutes, not seconds. A smaller whisper
  model is the obvious lever and is Fernando's call to pull.

## Rules

- **Never edit `~/git/narrator`.** It is the reference and it is someone else's working tree — including its `web/`, which is now a historical copy of this repo's reader. The reader is edited **here**.
- **`web/src/client/` is generated.** Never hand-edit a `.gen.ts`; change the Rust and regenerate.
- **This repo is public.** Anything committed is on the internet: no tailnet addresses, no hostnames, no tokens, no vault contents. The reader talks to the API by relative path and has nothing to leak; keep it that way.
- **Never touch production.** The VPS is deployed by hand.
- No `unwrap()` or `expect()` outside tests and the startup path. Errors are typed (`thiserror`), the edges use `anyhow`, and every failure path logs and degrades.
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` are clean, and stay clean.
- A change to any response shape means regenerating in the same commit: `./narrator client`, then commit `openapi.json` and `web/src/client/` with the Rust change. CI fails otherwise, and the reader's types come from those files.
- If the chunker's output changes, that is a **migration**, not an edit — it invalidates every cache and every stored position. Say so out loud before doing it.
